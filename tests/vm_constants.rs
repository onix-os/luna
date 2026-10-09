use luna::{Closure, Constant, Executor, Fuel, String};
use ottavino_gc_arena::Gc;

mod common;

#[test]
fn constant_addition_preserves_operand_order_across_gc_slices() {
    for budget in [1, 4, 64] {
        let mut lua = common::core();
        let executor = lua.enter(|ctx| {
            let closure = Closure::load(
                ctx,
                None,
                &br#"
                local n=9223372036854775807
                n=n+1
                assert(n == -9223372036854775807-1)
                local large=9007199254740993
                large=large+1
                assert(large == 9007199254740994)
                local half=0.5
                half=half+1
                assert(half == 1.5)
                local zero=-0.0
                zero=zero+-0.0
                assert(1/zero == -math.huge)
                local text='41'
                assert(text+1 == 42)
                local calls=0
                local t={}
                setmetatable(t, {__add=function(a,b)
                    calls=calls+1
                    if a==t then assert(b==5) return b end
                    assert(a==7 and b==t)
                    return a
                end})
                assert(t+5==5 and 7+t==7 and calls==2)
                local invalid=false
                assert(not pcall(function() return invalid+5 end))
                invalid=nil
                assert(not pcall(function() return invalid+5 end))
                local a,b=2,3
                assert(a+b==5)
                return true
            "#[..],
            )
            .unwrap();
            assert!(closure.prototype().opcodes.iter().any(|op| matches!(
                op.decode(),
                luna::opcode::Operation::Add {
                    right: luna::opcode::RCIndex::Constant(_),
                    ..
                }
            )));
            assert!(closure.prototype().opcodes.iter().any(|op| matches!(
                op.decode(),
                luna::opcode::Operation::Add {
                    right: luna::opcode::RCIndex::Register(_),
                    ..
                }
            )));
            ctx.stash(Executor::start(ctx, closure.into(), ()))
        });
        let mut finished = false;
        for _ in 0..2000 {
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
            assert!(ctx
                .fetch(&executor)
                .take_result::<bool>(ctx)
                .unwrap()
                .unwrap())
        });
    }
}

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
