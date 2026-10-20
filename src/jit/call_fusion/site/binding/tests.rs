use super::*;
use crate::{
    closure::{UpValue, UpValueState},
    jit::resources::Ledger,
    types::RegisterIndex,
    Context, Executor, Fuel, JitConfig, JitMode, Lua,
};
use allocator_api2::vec;
use ottavino_gc_arena::{allocator_api::MetricsAlloc, lock::Lock};

const SOURCE: &[u8] =
    b"local amount=7 local sum=11 local function add(v) sum=sum+v end add(amount) return sum";

fn prepared(ctx: Context<'_>) -> (Closure<'_>, Input) {
    let caller = Closure::load(ctx, Some("fusion-binding"), SOURCE).unwrap();
    let input = Input::new(
        &caller.prototype(),
        0,
        260,
        BudgetAllocator(Ledger::new(1 << 20)),
    )
    .unwrap()
    .unwrap();
    (caller, input)
}

fn callee<'gc>(
    ctx: Context<'gc>,
    prototype: Gc<'gc, FunctionPrototype<'gc>>,
    captures: &[UpValue<'gc>],
) -> Closure<'gc> {
    let mut upvalues = vec::Vec::new_in(MetricsAlloc::new(&ctx));
    upvalues.extend(captures.iter().copied().map(Lock::new));
    Closure::from_parts(&ctx, prototype, upvalues)
}

fn with_valid_frame<'gc, R>(
    ctx: Context<'gc>,
    caller: Closure<'gc>,
    input: &Input,
    run: impl FnOnce(LuaRegisters<'gc, '_>) -> R,
) -> R {
    let mut values = vec![Value::Nil; usize::from(input.site.registers)];
    let mut pc = input.site.region_pc as usize;
    LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
        if let Argument::Register(argument) = input.site.argument {
            registers.stack_frame[usize::from(argument)] = Value::Integer(7);
        }
        registers.stack_frame[usize::from(input.site.leaf.parent)] = Value::Integer(11);
        let capture = registers.open_test_upvalue(&ctx, RegisterIndex(input.site.leaf.parent));
        registers.stack_frame[usize::from(input.site.holder)] =
            callee(ctx, caller.prototype().prototypes[0], &[capture]).into();
        run(registers)
    })
}

#[test]
fn paired_binding_covers_constant_register_aliased_and_loop_arguments() {
    Lua::empty().enter(|ctx| {
        for (source, argument) in [
            (SOURCE, 7),
            (b"local sum=11 local function add(v) sum=sum+v end add(7) return sum".as_slice(), 7),
            (b"local sum=11 local function add(v) sum=sum+v end add(sum) return sum".as_slice(), 11),
            (b"local sum=0 local function add(v) sum=sum+v end for i=1,100 do add(i) end return sum".as_slice(), 7),
        ] {
            let caller = Closure::load(ctx, None, source).unwrap();
            let input = Input::new(&caller.prototype(), 0, 260, BudgetAllocator(Ledger::new(1 << 20)))
                .unwrap()
                .unwrap();
            with_valid_frame(ctx, caller, &input, |registers| {
                assert_eq!(input.with_binding(caller, &registers, |b| (b.sum, b.argument)), Some((11, argument)));
            });
        }
    });
}

#[test]
fn live_sources_are_rechecked_against_both_owned_snapshots() {
    Lua::empty().enter(|ctx| {
        for case in 0..9 {
            let (caller, mut input) = prepared(ctx);
            match case {
                0 => input.parent.constants[0].bits ^= 1,
                1 => {
                    input.parent.operations[0] = crate::opcode::Operation::LoadNil {
                        dest: RegisterIndex(0),
                        count: 1,
                    }
                }
                2 => input.parent.prototypes += 1,
                3 => input.fixed_params += 1,
                4 => input.varargs = !input.varargs,
                5 => input.child.registers += 1,
                6 => input.child.upvalues += 1,
                7 => input.child.operations.swap(0, 1),
                8 => input.child.prototypes += 1,
                _ => unreachable!(),
            }
            with_valid_frame(ctx, caller, &input, |registers| {
                let mut entered = false;
                assert!(
                    input
                        .with_binding(caller, &registers, |_| entered = true)
                        .is_none(),
                    "case={case}"
                );
                assert!(!entered);
            });
        }
    });
}

#[test]
fn upper_and_out_of_range_captures_are_not_current_parent_registers() {
    Lua::empty().enter(|ctx| {
        let (caller, input) = prepared(ctx);
        for capture_index in [0, 1, usize::MAX] {
            let mut pc = input.site.region_pc as usize;
            let mut values = vec![Value::Integer(11); usize::from(input.site.registers) + 2];
            LuaRegisters::projection_split_frame(ctx, &mut pc, &mut values, 2, |registers| {
                let capture = registers.projection_open_at(ctx, capture_index);
                registers.stack_frame[usize::from(input.site.holder)] =
                    callee(ctx, caller.prototype().prototypes[0], &[capture]).into();
                assert!(input.with_binding(caller, &registers, |_| ()).is_none());
            });
        }
    });
}

#[test]
fn source_pair_uses_one_instruction_and_allocation_budget() {
    Lua::empty().enter(|ctx| {
        let caller = Closure::load(ctx, None, SOURCE).unwrap();
        let parent = caller.prototype();
        let count = parent.opcodes.len() + parent.prototypes[0].opcodes.len();
        let ledger = Ledger::new(1 << 20);
        assert!(matches!(
            Input::new(&parent, 0, count - 1, BudgetAllocator(ledger.clone())),
            Err(JitError::ResourceLimit("fused source instructions"))
        ));
        assert_eq!(ledger.current(), 0);
        let input = Input::new(&parent, 0, count, BudgetAllocator(ledger.clone()))
            .unwrap()
            .unwrap();
        let bytes = ledger.current();
        assert!(bytes >= count * std::mem::size_of::<crate::opcode::Operation>());
        drop(input);
        assert_eq!(ledger.current(), 0);
        let constrained = Ledger::new(bytes - 1);
        assert!(matches!(
            Input::new(&parent, 0, count, BudgetAllocator(constrained.clone())),
            Err(JitError::ResourceLimit(_))
        ));
        assert_eq!(constrained.current(), 0);
        for allocations in 0..3 {
            let failing = Ledger::new(1 << 20);
            failing.fail_after(allocations);
            assert!(matches!(
                Input::new(&parent, 0, count, BudgetAllocator(failing.clone())),
                Err(JitError::ResourceLimit(_))
            ));
            assert_eq!(failing.current(), 0);
        }
        assert!(
            Input::new(&parent, 255, count, BudgetAllocator(ledger.clone()))
                .unwrap()
                .is_none()
        );
        assert_eq!(ledger.current(), 0);
    });
}

#[test]
fn source_pair_refuses_unrelated_or_unsupported_real_children() {
    Lua::empty().enter(|ctx| {
        for source in [
            b"local sum=11 local function add(v) sum=sum-v end add(7) return sum".as_slice(),
            b"local sum=11 local function add(v,...) sum=sum+v end add(7) return sum".as_slice(),
            b"local sum=11 local function add(v) sum=sum+v return sum end add(7) return sum".as_slice(),
            b"local sum=11 local function add(v) sum=sum+v end local function other() end add(7) return sum".as_slice(),
            b"local text='reference' local sum=11 local function add(v) sum=sum+v end add(7) return sum".as_slice(),
        ] {
            let caller = Closure::load(ctx, None, source).unwrap();
            let ledger = Ledger::new(1 << 20);
            assert!(Input::new(&caller.prototype(), 0, 260, BudgetAllocator(ledger.clone()))
                .unwrap()
                .is_none());
            assert_eq!(ledger.current(), 0);
        }
    });
}

#[test]
fn binding_refuses_wrong_callable_source_capture_frame_and_scalar_types() {
    Lua::empty().enter(|ctx| {
        let (caller, input) = prepared(ctx);
        let identical = Closure::load(ctx, None, SOURCE).unwrap();
        let different = Closure::load(ctx, None, b"return 1").unwrap();
        let Argument::Register(argument) = input.site.argument else {
            panic!("fixture needs a register argument");
        };
        for case in 0..16 {
            let width = usize::from(input.site.registers) - usize::from(case == 14);
            let mut values = vec![Value::Nil; width];
            let mut pc = input.site.region_pc as usize + usize::from(case == 1);
            LuaRegisters::with_test_frame_state(
                ctx,
                &mut pc,
                &mut values,
                &mut Fuel::with(10000),
                case == 15,
                |mut registers| {
                    registers.stack_frame[usize::from(input.site.leaf.parent)] = Value::Integer(11);
                    registers.stack_frame[usize::from(argument)] = Value::Integer(7);
                    let correct =
                        registers.open_test_upvalue(&ctx, RegisterIndex(input.site.leaf.parent));
                    let capture = match case {
                        5 => UpValue::new(&ctx, UpValueState::Closed(Value::Integer(11))),
                        6 => registers.open_test_upvalue(&ctx, RegisterIndex(argument)),
                        7 => {
                            let mut foreign_pc = 0;
                            LuaRegisters::with_test_frame(
                                ctx,
                                &mut foreign_pc,
                                &mut [Value::Integer(11)],
                                |mut foreign| foreign.open_test_upvalue(&ctx, RegisterIndex(0)),
                            )
                        }
                        _ => correct,
                    };
                    let prototype = if case == 4 {
                        identical.prototype().prototypes[0]
                    } else {
                        caller.prototype().prototypes[0]
                    };
                    let captures = match case {
                        9 => &[][..],
                        10 => &[capture, correct][..],
                        _ => &[capture][..],
                    };
                    let target = callee(ctx, prototype, captures);
                    registers.stack_frame[usize::from(input.site.holder)] = match case {
                        2 => Value::Nil,
                        3 => crate::Callback::from_fn(&ctx, |_, _, _| unreachable!()).into(),
                        _ => target.into(),
                    };
                    match case {
                        11 => {
                            registers.stack_frame[usize::from(input.site.leaf.parent)] =
                                Value::Number(11.0)
                        }
                        12 => {
                            registers.stack_frame[usize::from(input.site.leaf.parent)] = Value::Nil
                        }
                        13 => registers.stack_frame[usize::from(argument)] = Value::Number(7.0),
                        _ => {}
                    }
                    let before = registers.stack_frame.to_vec();
                    let original_pc = *registers.pc;
                    let result = input.with_binding(
                        if case == 8 { different } else { caller },
                        &registers,
                        |bound| (bound.site, bound.sum, bound.argument),
                    );
                    if case == 0 {
                        assert_eq!(result, Some((input.site, 11, 7)));
                    } else {
                        assert!(result.is_none(), "case={case}");
                    }
                    assert_eq!(*registers.pc, original_pc);
                    for (actual, expected) in registers.stack_frame.iter().zip(&before) {
                        match (*actual, *expected) {
                            (Value::Nil, Value::Nil) => {}
                            (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
                            (Value::Number(a), Value::Number(b)) => {
                                assert_eq!(a.to_bits(), b.to_bits())
                            }
                            (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
                            _ => panic!("binding changed a canonical value"),
                        }
                    }
                },
            );
        }
    });
}

#[test]
fn every_binding_resamples_current_capture_and_argument() {
    Lua::empty().enter(|ctx| {
        let (caller, input) = prepared(ctx);
        let Argument::Register(argument) = input.site.argument else {
            panic!();
        };
        let mut values = vec![Value::Nil; usize::from(input.site.registers)];
        let mut pc = input.site.region_pc as usize;
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let capture = registers.open_test_upvalue(&ctx, RegisterIndex(input.site.leaf.parent));
            let target = callee(ctx, caller.prototype().prototypes[0], &[capture]);
            registers.stack_frame[usize::from(input.site.holder)] = target.into();
            for (sum, arg) in [(11, 7), (i64::MAX, 1), (i64::MIN, -1), (-91, 37)] {
                registers.stack_frame[usize::from(input.site.leaf.parent)] = Value::Integer(sum);
                registers.stack_frame[usize::from(argument)] = Value::Integer(arg);
                assert_eq!(
                    input.with_binding(caller, &registers, |b| (b.sum, b.argument)),
                    Some((sum, arg))
                );
            }
            target.set_upvalue(
                &ctx,
                0,
                UpValue::new(&ctx, UpValueState::Closed(Value::Integer(-91))),
            );
            assert!(input.with_binding(caller, &registers, |_| ()).is_none());
            target.set_upvalue(&ctx, 0, capture);
            assert!(input.with_binding(caller, &registers, |_| ()).is_some());
        });
    });
}

#[test]
fn live_executor_binding_survives_collection_without_retaining_gc_objects() {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Off,
        ..Default::default()
    })
    .unwrap();
    let (executor, input) = lua.enter(|ctx| {
        let (caller, input) = prepared(ctx);
        (ctx.stash(Executor::start(ctx, caller.into(), ())), input)
    });
    for _ in 0..2 {
        lua.enter(|ctx| {
            assert!(!ctx.fetch(&executor).step(ctx, &mut Fuel::with(1)).unwrap());
        });
    }
    for _ in 0..3 {
        lua.gc_collect();
        lua.enter(|ctx| {
            let executor = ctx.fetch(&executor);
            let before = executor.test_frame_snapshot();
            executor.test_with_lua_registers(ctx, |caller, registers| {
                let saved = *registers.pc;
                *registers.pc = input.site.region_pc as usize;
                assert_eq!(
                    input.with_binding(caller, &registers, |b| (b.sum, b.argument)),
                    Some((18, 7))
                );
                *registers.pc = saved;
            });
            assert_eq!(executor.test_frame_snapshot(), before);
        });
    }
    assert_eq!(lua.execute::<i64>(&executor).unwrap(), 18);
    assert_eq!(lua.jit_stats().native_instructions, 0);
}
