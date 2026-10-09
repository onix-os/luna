use luna::{Closure, Constant, Executor, Fuel, String};
use ottavino_gc_arena::Gc;

mod common;

#[test]
fn constants_keep_string_identity_across_metamethod_slices_and_gc() {
    for budget in [1, 4, 64] {
        let mut lua = common::core();
        let (closure, executor) = lua.enter(|ctx| {
            let closure = Closure::load(
                ctx,
                None,
                &br#"
                local t=setmetatable({}, {__index=function(_, key) return key end})
                return t['constant-identity'], 'constant-identity', 9007199254740993, -0.0
            "#[..],
            )
            .unwrap();
            (
                ctx.stash(closure),
                ctx.stash(Executor::start(ctx, closure.into(), ())),
            )
        });
        #[cfg(feature = "jit")]
        lua.prepare_jit().unwrap();
        let mut finished = false;
        for _ in 0..1000 {
            lua.gc_collect();
            finished = lua.enter(|ctx| {
                ctx.fetch(&executor)
                    .step(ctx, &mut Fuel::with(budget))
                    .unwrap()
            });
            if finished {
                break;
            }
        }
        assert!(finished);
        lua.enter(|ctx| {
            let expected = ctx
                .fetch(&closure)
                .prototype()
                .constants
                .iter()
                .find_map(|constant| match constant {
                    Constant::String(value) if value.as_bytes() == b"constant-identity" => {
                        Some(*value)
                    }
                    _ => None,
                })
                .unwrap();
            let (indexed, loaded, integer, number) = ctx
                .fetch(&executor)
                .take_result::<(String, String, i64, f64)>(ctx)
                .unwrap()
                .unwrap();
            assert!(Gc::ptr_eq(indexed.into_inner(), expected.into_inner()));
            assert!(Gc::ptr_eq(loaded.into_inner(), expected.into_inner()));
            assert_eq!(integer, 9_007_199_254_740_993);
            assert_eq!(number.to_bits(), (-0.0f64).to_bits());
        });
    }
}
