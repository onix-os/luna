use super::{JitMode, Runtime};

impl Runtime {
    pub(crate) fn vm_policy(&self, hooked: bool) -> (bool, bool) {
        if hooked {
            return (false, false);
        }
        let manager = self.0.borrow();
        let active = manager.config.mode == JitMode::Auto;
        #[cfg(all(
            not(miri),
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        let pairs = active && manager.pairs.is_some();
        #[cfg(not(all(
            not(miri),
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )))]
        let pairs = false;
        (active, pairs)
    }
}

#[cfg(test)]
mod tests {
    use crate::{JitConfig, JitMode, Lua};

    #[test]
    fn policy_is_fresh_and_does_not_change_execution_state() {
        let mut lua = Lua::empty();
        for mode in [JitMode::Off, JitMode::Auto, JitMode::Off, JitMode::Auto] {
            lua.set_jit_config(JitConfig {
                mode,
                ..Default::default()
            })
            .unwrap();
            lua.enter(|ctx| {
                let before = ctx.jit().0.borrow().stats;
                let active = ctx.jit().active();
                #[cfg(all(
                    not(miri),
                    target_os = "linux",
                    any(target_arch = "x86_64", target_arch = "aarch64")
                ))]
                let pairs = ctx.jit().call_pairs_enabled();
                #[cfg(not(all(
                    not(miri),
                    target_os = "linux",
                    any(target_arch = "x86_64", target_arch = "aarch64")
                )))]
                let pairs = false;
                assert_eq!(ctx.jit().vm_policy(false), (active, pairs));
                assert_eq!(ctx.jit().vm_policy(true), (false, false));
                assert_eq!(ctx.jit().0.borrow().stats, before);
                assert!(ctx.jit().0.try_borrow_mut().is_ok());
            });
        }
    }

    #[test]
    fn hooked_policy_does_not_borrow_the_manager() {
        Lua::empty().enter(|ctx| {
            let mut manager = ctx.jit().0.borrow_mut();
            for mode in [JitMode::Off, JitMode::Auto] {
                manager.config.mode = mode;
                assert_eq!(ctx.jit().vm_policy(true), (false, false));
            }
        });
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn pair_policy_tracks_mode_and_pair_state_without_a_source() {
        let mut lua = Lua::empty();
        for mode in [JitMode::Off, JitMode::Auto, JitMode::Off] {
            lua.set_jit_config(JitConfig {
                mode,
                ..Default::default()
            })
            .unwrap();
            lua.enter(|ctx| {
                for enabled in [false, true, false, true] {
                    ctx.jit().test_call_pairs(enabled);
                    let before = ctx.jit().0.borrow().stats;
                    assert_eq!(
                        ctx.jit().vm_policy(false),
                        (mode == JitMode::Auto, mode == JitMode::Auto && enabled)
                    );
                    assert_eq!(ctx.jit().vm_policy(true), (false, false));
                    assert_eq!(ctx.jit().0.borrow().stats, before);
                }
            });
        }
    }
}
