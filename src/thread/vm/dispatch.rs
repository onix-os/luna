use crate::jit::Prepared;

pub(super) enum Dispatch {
    Interpreted,
    Observing(u64),
    Compiled(Prepared),
}

impl Dispatch {
    pub(super) fn new(source: Option<u64>, code: Option<Prepared>) -> Self {
        match (code, source) {
            (Some(code), _) => Self::Compiled(code),
            (None, Some(source)) => Self::Observing(source),
            (None, None) => Self::Interpreted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Dispatch;

    #[test]
    fn absent_code_preserves_source_observation_choice() {
        assert!(matches!(Dispatch::new(None, None), Dispatch::Interpreted));
        for source in [0, 1, u64::MAX] {
            assert!(matches!(
                Dispatch::new(Some(source), None),
                Dispatch::Observing(found) if found == source
            ));
        }
    }

    #[cfg(all(
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
            let dispatch = Dispatch::new(source, Some(code));
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
