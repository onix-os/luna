use super::super::thread::LuaRegisters;
use crate::{closure::UpValue, opcode::ScalarPrefix, Context, FunctionPrototype, Value};
use ottavino_gc_arena::lock::Lock;

#[cfg(test)]
thread_local! {
    static ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    static HITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub(super) fn plan(prototype: &FunctionPrototype<'_>) -> Option<ScalarPrefix> {
    #[cfg(test)]
    if !ENABLED.get() {
        return None;
    }
    ScalarPrefix::new(
        &prototype.opcodes,
        usize::from(prototype.stack_size),
        prototype.upvalues.len(),
    )
}

pub(super) fn run<'gc>(
    plan: ScalarPrefix,
    ctx: Context<'gc>,
    upvalues: &[Lock<UpValue<'gc>>],
    registers: &mut LuaRegisters<'gc, '_>,
    budget: u32,
    mut dispatched: impl FnMut(),
) -> u32 {
    if budget == 0 || !plan.fits(registers.stack_frame.len(), upvalues.len()) {
        return 0;
    }
    let mut completed = 0;
    if *registers.pc == 0 {
        dispatched();
        *registers.pc = 1;
        registers.stack_frame[usize::from(plan.get_dest.0)] =
            registers.get_upvalue(&ctx, upvalues[usize::from(plan.get_source.0)].get());
        completed += 1;
        #[cfg(test)]
        HITS.set(HITS.get() + 1);
    }
    if completed == budget {
        return completed;
    }
    if *registers.pc == 1 {
        let (Value::Integer(left), Value::Integer(right)) = (
            registers.stack_frame[usize::from(plan.left.0)],
            registers.stack_frame[usize::from(plan.right.0)],
        ) else {
            return completed;
        };
        dispatched();
        *registers.pc = 2;
        registers.stack_frame[usize::from(plan.add_dest.0)] =
            Value::Integer(left.wrapping_add(right));
        completed += 1;
        #[cfg(test)]
        HITS.set(HITS.get() + 1);
    }
    if completed == budget {
        return completed;
    }
    if *registers.pc == 2 {
        dispatched();
        *registers.pc = 3;
        registers.set_upvalue(
            &ctx,
            upvalues[usize::from(plan.set_dest.0)].get(),
            registers.stack_frame[usize::from(plan.set_source.0)],
        );
        completed += 1;
        #[cfg(test)]
        HITS.set(HITS.get() + 1);
    }
    completed
}

#[cfg(test)]
pub(in crate::thread) fn scoped<R>(enabled: bool, run: impl FnOnce() -> R) -> (R, u64) {
    struct Restore(bool, u64);
    impl Drop for Restore {
        fn drop(&mut self) {
            ENABLED.set(self.0);
            HITS.set(self.1);
        }
    }
    let _restore = Restore(ENABLED.replace(enabled), HITS.replace(0));
    let result = run();
    (result, HITS.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_and_panicking_scopes_restore_selection_and_counts() {
        assert!(ENABLED.get());
        scoped(false, || {
            HITS.set(17);
            let result = std::panic::catch_unwind(|| {
                scoped(true, || {
                    assert!(ENABLED.get());
                    HITS.set(23);
                    std::panic::panic_any(91u32);
                })
            });
            assert_eq!(result.unwrap_err().downcast_ref::<u32>(), Some(&91));
            assert!(!ENABLED.get());
            assert_eq!(HITS.get(), 17);
        });
        assert!(ENABLED.get());
    }

    #[cfg(feature = "jit")]
    #[test]
    fn invalid_actual_extents_decline_without_any_effect() {
        crate::Lua::empty().enter(|ctx| {
            let plan = ScalarPrefix {
                get_dest: crate::types::RegisterIndex(1),
                get_source: crate::types::UpValueIndex(0),
                add_dest: crate::types::RegisterIndex(1),
                left: crate::types::RegisterIndex(1),
                right: crate::types::RegisterIndex(0),
                set_source: crate::types::RegisterIndex(1),
                set_dest: crate::types::UpValueIndex(1),
            };
            let mut values = [Value::Integer(7)];
            let mut pc = 0;
            let mut count = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                assert_eq!(run(plan, ctx, &[], &mut registers, 64, || count += 1), 0);
            });
            assert_eq!((pc, count), (0, 0));
            assert!(matches!(values[0], Value::Integer(7)));
        });
    }

    #[cfg(feature = "jit")]
    #[test]
    fn borrowed_foreign_upvalue_panics_preserve_pc_and_dispatch_publication() {
        use crate::thread::OpenUpValue;
        crate::Lua::empty().enter(|ctx| {
            OpenUpValue::with_test_stack(ctx, &[Value::Integer(13)], 0, true, |cell| {
                let upvalues = [Lock::new(cell), Lock::new(cell)];
                let plan = ScalarPrefix {
                    get_dest: crate::types::RegisterIndex(1),
                    get_source: crate::types::UpValueIndex(0),
                    add_dest: crate::types::RegisterIndex(1),
                    left: crate::types::RegisterIndex(1),
                    right: crate::types::RegisterIndex(0),
                    set_source: crate::types::RegisterIndex(1),
                    set_dest: crate::types::UpValueIndex(1),
                };
                for initial in [0, 2] {
                    let observe = |candidate| {
                        let mut pc = initial;
                        let mut values = [Value::Integer(7), Value::Integer(9)];
                        let mut count = 0;
                        let panic = LuaRegisters::with_test_frame(
                            ctx,
                            &mut pc,
                            &mut values,
                            |mut registers| {
                                let result =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        if candidate {
                                            run(plan, ctx, &upvalues, &mut registers, 1, || {
                                                count += 1
                                            });
                                        } else {
                                            count += 1;
                                            *registers.pc += 1;
                                            if initial == 0 {
                                                registers.stack_frame[1] =
                                                    registers.get_upvalue(&ctx, cell);
                                            } else {
                                                registers.set_upvalue(
                                                    &ctx,
                                                    cell,
                                                    registers.stack_frame[1],
                                                );
                                            }
                                        }
                                    }));
                                result.unwrap_err().as_ref().type_id()
                            },
                        );
                        let values = values.map(|value| {
                            if let Value::Integer(n) = value {
                                n
                            } else {
                                panic!("unexpected value")
                            }
                        });
                        (pc, count, panic, values)
                    };
                    assert_eq!(observe(false), observe(true));
                }
            });
        });
    }
}
