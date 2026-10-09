#[cfg(feature = "jit")]
use crate::jit::Prepared;

#[cfg(not(feature = "jit"))]
pub(super) enum Dispatch {
    Interpreted,
    Hooked,
}

#[cfg(feature = "jit")]
#[repr(u8)]
pub(super) enum Dispatch {
    Hooked,
    Observing(u64),
    Compiled(Prepared),
    Interpreted = u8::MAX,
}

impl Dispatch {
    pub(super) fn interpreted(hook_enabled: bool) -> Self {
        if hook_enabled {
            Self::Hooked
        } else {
            Self::Interpreted
        }
    }

    #[cfg(feature = "jit")]
    pub(super) fn new(source: Option<u64>, code: Option<Prepared>, hook_enabled: bool) -> Self {
        if hook_enabled {
            return Self::Hooked;
        }
        match (code, source) {
            (Some(code), _) => Self::Compiled(code),
            (None, Some(source)) => Self::Observing(source),
            (None, None) => Self::interpreted(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Dispatch;

    #[test]
    fn storage_stays_within_tagged_counter_payload() {
        let bound = if cfg!(feature = "jit") { 16 } else { 1 };
        assert!(std::mem::size_of::<Dispatch>() <= bound);
        eprintln!(
            "dispatch_size={} dispatch_alignment={}",
            std::mem::size_of::<Dispatch>(),
            std::mem::align_of::<Dispatch>()
        );
    }

    #[test]
    fn hook_choice_preserves_plain_interpreter_dispatch() {
        assert!(matches!(
            Dispatch::interpreted(false),
            Dispatch::Interpreted
        ));
        assert!(matches!(Dispatch::interpreted(true), Dispatch::Hooked));
    }

    #[cfg(feature = "jit")]
    #[test]
    fn absent_code_preserves_source_observation_choice() {
        assert!(matches!(
            Dispatch::new(None, None, false),
            Dispatch::Interpreted
        ));
        assert!(matches!(Dispatch::new(None, None, true), Dispatch::Hooked));
        for source in [0, 1, u64::MAX] {
            assert!(matches!(
                Dispatch::new(Some(source), None, false),
                Dispatch::Observing(found) if found == source
            ));
            assert!(matches!(
                Dispatch::new(Some(source), None, true),
                Dispatch::Hooked
            ));
        }
    }

    #[cfg(all(
        feature = "jit",
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn compiled_state_keeps_one_owner_through_retirement_and_gc() {
        use crate::{Closure, JitConfig, JitMode, Lua};

        for source_choice in 0..3 {
            let mut lua = Lua::empty();
            lua.set_jit_config(JitConfig {
                mode: JitMode::Auto,
                ..Default::default()
            })
            .unwrap();
            let closure = lua.enter(|ctx| {
                let closure =
                    Closure::load(ctx, None, b"local n=0 for i=1,32 do n=n+i end return n")
                        .unwrap();
                ctx.stash(closure)
            });
            assert_eq!(lua.prepare_jit().unwrap(), 1);
            let (source, code) = lua.enter(|ctx| {
                let source = ctx
                    .jit_registry()
                    .borrow()
                    .identity(ctx, ctx.fetch(&closure).prototype())
                    .unwrap();
                (source, ctx.jit().lookup(source).unwrap())
            });
            let before = lua.jit_stats();
            let source = match source_choice {
                0 => None,
                1 => Some(source),
                _ => Some(u64::MAX),
            };
            let dispatch = Dispatch::new(source, Some(code), false);
            assert!(matches!(&dispatch, Dispatch::Compiled(_)));
            let after = lua.jit_stats();
            assert_eq!(
                (
                    after.code_bytes,
                    after.metadata_bytes,
                    after.code_lookups,
                    after.code_leases
                ),
                (
                    before.code_bytes,
                    before.metadata_bytes,
                    before.code_lookups,
                    before.code_leases
                ),
            );
            assert!(before.code_bytes > 0);
            lua.clear_jit_cache();
            lua.gc_collect();
            lua.gc_collect();
            assert_eq!(lua.jit_stats().code_bytes, before.code_bytes);
            drop(dispatch);
            let reclaimed = lua.jit_stats();
            assert_eq!(
                (reclaimed.code_bytes, reclaimed.code_requested_bytes),
                (0, 0)
            );
        }
    }
}
