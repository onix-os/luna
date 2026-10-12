use super::*;
use crate::{Lua, Value};

#[test]
fn builtin_scope_restores_selection_and_counts_on_unwind() {
    let (_, hits) = scoped(true, || {
        assert!(ENABLED.get());
        HITS.set(3);
        let result = std::panic::catch_unwind(|| scoped(false, || panic!("scope")));
        assert!(result.is_err());
        assert!(ENABLED.get());
        assert_eq!(HITS.get(), 3);
    });
    assert_eq!(hits, 3);
    assert!(!ENABLED.get());
}

#[test]
fn builtin_normal_call_admission_preserves_refused_state_and_exact_fuel() {
    let mut admitted = 0;
    let mut refused = 0;
    for case in 0..12 {
        for available in [i32::MIN, 0, 6, 13, 14, 20, 64, i32::MAX] {
            Lua::core().enter(|ctx| {
                let callback: Callback = ctx.get_global("setmetatable").unwrap();
                if case == 11 {
                    ctx.set_debug_hook(Value::Function(callback.into()), true, 1);
                }
                let table = Table::new(&ctx);
                let mt = if case == 1 { table } else { Table::new(&ctx) };
                let old = Table::new(&ctx);
                if case == 2 {
                    old.set_field(ctx, "__metatable", false);
                    table.set_metatable(ctx, Some(old));
                }
                if case == 3 {
                    mt.set_field(ctx, "__mode", "k");
                }
                if case == 4 {
                    mt.set_field(ctx, "__gc", false);
                }
                if case == 5 {
                    table.into_inner().borrow_mut(&ctx).readonly = true;
                }
                let thread = Thread::new(ctx);
                let mut state = thread.into_inner().borrow_mut(&ctx);
                let caller = Closure::load(ctx, None, b"return 1").unwrap();
                state.frames.reserve(2);
                state.frames.push(Frame::Lua {
                    bottom: 0,
                    base: 0,
                    closure: caller,
                    is_variable: false,
                    pc: 1,
                    stack_size: 5,
                    expected_return: None,
                });
                if case == 6 {
                    state.max_call_depth = 1;
                }
                if case == 7 {
                    state.to_be_closed.push(1);
                }
                let mut fuel = Fuel::with(available);
                if case == 8 {
                    fuel.interrupt();
                }
                let storage = state.stack;
                let mut stack = storage.borrow_mut(&ctx);
                let actual_callback = if case == 9 {
                    Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return))
                } else {
                    callback
                };
                stack.extend([
                    Value::Function(actual_callback.into()),
                    Value::Table(table),
                    Value::Table(mt),
                    Value::Integer(42),
                    Value::Number(-0.0),
                ]);
                let held = (case == 10).then(|| table.into_inner().borrow());
                let before: Vec<_> = stack.iter().map(|v| format!("{v:?}")).collect();
                let original_mt = table.metatable();
                let (used, hits) = scoped(true, || {
                    LuaFrame {
                        #[cfg(all(
                            not(miri),
                            target_os = "linux",
                            any(target_arch = "x86_64", target_arch = "aarch64")
                        ))]
                        pair_handoff: None,
                        state: &mut state,
                        stack: &mut stack,
                        fuel: &mut fuel,
                    }
                    .try_set_metatable(
                        ctx,
                        RegisterIndex(0),
                        VarCount::constant(2),
                        Some(VarCount::constant(1)),
                        3,
                    )
                });
                drop(held);
                let expected = available > 13 && matches!(case, 0 | 1 | 5);
                assert_eq!(used, expected, "case={case} fuel={available}");
                assert_eq!(hits, u64::from(used));
                assert_eq!(state.frames.len(), 1);
                assert!(!state.running);
                assert_eq!(fuel.is_interrupted(), case == 8);
                if used {
                    admitted += 1;
                    assert_eq!(fuel.remaining(), available.saturating_sub(18));
                    assert!(matches!(stack[0], Value::Table(found) if found == table));
                    assert!(stack[1..].iter().all(|value| value.is_nil()));
                    assert_eq!(table.metatable(), Some(mt));
                } else {
                    refused += 1;
                    assert_eq!(fuel.remaining(), available);
                    assert_eq!(
                        stack.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>(),
                        before
                    );
                    assert_eq!(table.metatable(), original_mt);
                }
            });
        }
    }
    assert_eq!((admitted, refused), (12, 84));
}
