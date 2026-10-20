use super::*;
use crate::jit::resources::{BudgetAllocator, Ledger};
use crate::{closure::UpValue, thread::LuaRegisters, Callback, CallbackReturn, Lua};

const SOURCE: &[u8] = b"local sum=0 for i=1,5000 do sum=sum+host_increment(i) end return sum";

fn same<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
    match (actual, expected) {
        (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
        (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
        _ => panic!("bound value mismatch"),
    }
}

fn prepared<'gc>(ctx: crate::Context<'gc>) -> (Plan, Closure<'gc>, Table<'gc>, EntryBytes) {
    let environment = Table::new(&ctx);
    environment.set_field(
        ctx,
        "host_increment",
        Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return)),
    );
    let closure = Closure::load_with_env(ctx, None, SOURCE, environment).unwrap();
    let source = crate::jit::ir::Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap();
    let plan = Plan::recognize(&source).unwrap();
    let hints = EntryBytes::try_new(
        source.operations.iter().map(|_| true),
        BudgetAllocator(Ledger::new(4096)),
    )
    .unwrap();
    let Constant::String(key) = closure.prototype().constants[usize::from(plan.constant)] else {
        panic!()
    };
    environment.get_canonical_string(ctx, key, hints.hint(plan.helper_pc as usize));
    (plan, closure, environment, hints)
}

fn enter<'gc>(
    fresh: bool,
    plan: Plan,
    entry: Entry,
    frame: &mut helpers::Frame<'gc, '_, '_, '_>,
    slots: &mut [abi::Slot],
    pc: usize,
    budget: u32,
) -> Result<Option<abi::Exit>, Fault> {
    if fresh {
        unsafe { plan.invoke_fresh(entry, frame, slots, pc, budget) }
    } else {
        plan.with_binding(frame.closure, frame.read_hints.unwrap(), |bound| unsafe {
            bound.invoke(entry, frame, slots, pc, budget)
        })
        .unwrap()
    }
}

unsafe extern "C" fn probe(
    slots: *mut abi::Slot,
    pc: u64,
    budget: u32,
    exit: *mut abi::Exit,
    host: *mut abi::Host,
    view: *mut View,
) {
    unsafe {
        *exit = abi::Exit {
            pc,
            instructions: 0,
            reason: crate::jit::exits::Kind::Budget as u32,
        };
        if budget == 0 {
            return;
        }
        let destination = (((*view).binding[1] >> 16) & 255) as usize;
        *slots.add(destination) = (*view).slot;
        (*view).executed = 1;
        let cookie = (*view).binding;
        let result = helpers::call::<{ abi::HELPER_CONSTANT }>(host, slots, 0, 0, 0, pc as u32);
        assert_eq!(result, abi::HELPER_COMPLETED);
        assert_eq!((*view).binding, cookie);
        assert_eq!((*view).executed, 1);
        (*exit).instructions = 1;
        (*exit).pc = pc + 1;
    }
}

unsafe extern "C" fn never(
    _: *mut abi::Slot,
    _: u64,
    _: u32,
    exit: *mut abi::Exit,
    _: *mut abi::Host,
    _: *mut View,
) {
    unsafe {
        (*exit).reason = 99;
    }
}

unsafe extern "C" fn panicked_helper<const READ: bool>(
    slots: *mut abi::Slot,
    pc: u64,
    _: u32,
    exit: *mut abi::Exit,
    host: *mut abi::Host,
    view: *mut View,
) {
    unsafe {
        if READ {
            let destination = (((*view).binding[1] >> 16) & 255) as usize;
            *slots.add(destination) = (*view).slot;
            (*view).executed = 1;
        }
        let result =
            helpers::call::<{ abi::HELPER_CONSTANT }>(host, slots, 0, u32::MAX, 0, pc as u32);
        assert_eq!(result, abi::HELPER_PANICKED);
        *exit = abi::Exit {
            pc,
            instructions: 1,
            reason: crate::jit::exits::Kind::Panic as u32,
        };
    }
}

#[test]
fn helper_panic_preserves_materialization_payload_and_releases_binding() {
    Lua::empty().enter(|ctx| {
        let (plan, closure, environment, hints) = prepared(ctx);
        let width = usize::from(plan.registers);
        let Constant::String(key) = closure.prototype().constants[usize::from(plan.constant)]
        else {
            panic!()
        };
        let expected = environment.get_raw(&ctx, key.into());
        for (read, fresh) in [false, true]
            .into_iter()
            .flat_map(|read| [false, true].map(|fresh| (read, fresh)))
        {
            let mut values = vec![Value::Integer(-7); width];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                let mut slots = vec![abi::Slot::from_value(Value::Integer(91)); width];
                let mut frame = helpers::Frame {
                    ctx,
                    closure,
                    registers: &mut registers,
                    count: helpers::Counts::default(),
                    slot_count: width,
                    panic: None,
                    projection: None,
                    read_hints: Some(&hints),
                };
                let entry: Entry = if read {
                    panicked_helper::<true>
                } else {
                    panicked_helper::<false>
                };
                let exit = enter(fresh, plan, entry, &mut frame, &mut slots, 17, 64)
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    (exit.pc, exit.instructions, exit.reason),
                    (17, 1, crate::jit::exits::Kind::Panic as u32)
                );
                assert_eq!(*frame.registers.pc, 18);
                for (index, &value) in frame.registers.stack_frame.iter().enumerate() {
                    same(
                        value,
                        if read && index == usize::from(plan.destination) {
                            expected
                        } else {
                            Value::Integer(91)
                        },
                    );
                }
                assert_eq!(
                    (
                        frame.count.calls,
                        frame.count.completed,
                        frame.count.declined,
                        frame.count.table_reads,
                        frame.count.upvalue_reads
                    ),
                    (1, 0, 0, u64::from(read), u64::from(read))
                );
                assert!(environment.into_inner().try_borrow_mut(&ctx).is_ok());
                let payload = frame.panic.take().unwrap();
                let pointer = payload.as_ref() as *const dyn std::any::Any as *const ();
                let propagated = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    std::panic::resume_unwind(payload)
                }))
                .unwrap_err();
                assert_eq!(
                    propagated.as_ref() as *const dyn std::any::Any as *const (),
                    pointer
                );
                assert!(propagated
                    .downcast_ref::<String>()
                    .unwrap()
                    .contains("index out of bounds"));
            });
        }
    });
}

#[test]
fn typed_entry_reborrows_real_helpers_and_publishes_only_executed_reads() {
    Lua::empty().enter(|ctx| {
        let (plan, closure, environment, hints) = prepared(ctx);
        let Constant::String(key) = closure.prototype().constants[usize::from(plan.constant)]
        else {
            panic!()
        };
        let expected = environment.get_raw(&ctx, key.into());
        let width = usize::from(plan.registers);
        for (budget, fresh) in [0, 1, 64, 65, u32::MAX]
            .into_iter()
            .flat_map(|budget| [false, true].map(|fresh| (budget, fresh)))
        {
            let mut values = vec![Value::Integer(-7); width];
            let mut pc = plan.helper_pc as usize;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                let mut slots = vec![abi::Slot::from_value(Value::Integer(-7)); width];
                let mut frame = helpers::Frame {
                    ctx,
                    closure,
                    registers: &mut registers,
                    count: helpers::Counts::default(),
                    slot_count: width,
                    panic: None,
                    projection: None,
                    read_hints: Some(&hints),
                };
                let exit = enter(
                    fresh,
                    plan,
                    probe,
                    &mut frame,
                    &mut slots,
                    plan.helper_pc as usize,
                    budget,
                )
                .unwrap()
                .unwrap();
                assert_eq!(exit.instructions, u32::from(budget != 0));
                let expected = if budget == 0 {
                    Value::Integer(-7)
                } else {
                    expected
                };
                same(
                    frame.registers.stack_frame[usize::from(plan.destination)],
                    expected,
                );
                for (slot, value) in slots.iter().zip(frame.registers.stack_frame.iter_mut()) {
                    slot.write_back(value);
                }
                same(
                    frame.registers.stack_frame[usize::from(plan.destination)],
                    expected,
                );
                let completed = u64::from(budget != 0);
                assert_eq!(
                    (
                        frame.count.calls,
                        frame.count.completed,
                        frame.count.table_reads,
                        frame.count.upvalue_reads
                    ),
                    (completed, completed, completed, completed)
                );
                assert!(frame.panic.is_none());
                assert!(environment.into_inner().try_borrow_mut(&ctx).is_ok());
            });
        }
    });
}

#[test]
fn closed_environment_rebinding_and_open_cells_refuse_before_entry() {
    Lua::empty().enter(|ctx| {
        let (plan, closure, environment, hints) = prepared(ctx);
        let width = usize::from(plan.registers);
        let mut values = vec![Value::Table(environment); width];
        let mut pc = 0;
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut slots = vec![abi::Slot::from_value(Value::Table(environment)); width];
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: width,
                panic: None,
                projection: None,
                read_hints: Some(&hints),
            };
            let peer = Table::new(&ctx);
            let result = plan
                .with_binding(closure, &hints, |bound| {
                    closure.set_upvalue(
                        &ctx,
                        usize::from(plan.upvalue),
                        UpValue::new(&ctx, UpValueState::Closed(peer.into())),
                    );
                    unsafe { bound.invoke(never, &mut frame, &mut slots, 0, 64) }
                })
                .unwrap()
                .unwrap();
            assert!(result.is_none());
            assert!(environment.into_inner().try_borrow_mut(&ctx).is_ok());
            let open = frame
                .registers
                .open_test_upvalue(&ctx, crate::types::RegisterIndex(0));
            closure.set_upvalue(&ctx, usize::from(plan.upvalue), open);
            assert!(plan
                .with_binding(closure, &hints, |_| panic!("captured open environment"))
                .is_none());
            assert_eq!(
                (
                    frame.count.calls,
                    frame.count.table_reads,
                    frame.count.upvalue_reads
                ),
                (0, 0, 0)
            );
        });
    });
}

#[test]
fn pending_panic_and_bad_register_extents_do_not_enter_or_replace_state() {
    Lua::empty().enter(|ctx| {
        let (plan, closure, environment, hints) = prepared(ctx);
        let width = usize::from(plan.registers);
        for (case, fresh) in (0..4).flat_map(|case| [false, true].map(|fresh| (case, fresh))) {
            let mut values = vec![Value::Integer(-7); width - usize::from(case == 3)];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
                let mut slots =
                    vec![abi::Slot::from_value(Value::Integer(-7)); width - usize::from(case == 1)];
                let mut frame = helpers::Frame {
                    ctx,
                    closure,
                    registers: &mut registers,
                    count: helpers::Counts::default(),
                    slot_count: width - usize::from(case == 0),
                    panic: (case == 2).then(|| Box::new(19u32) as Box<dyn std::any::Any + Send>),
                    projection: None,
                    read_hints: Some(&hints),
                };
                let result = enter(fresh, plan, never, &mut frame, &mut slots, 0, 64).unwrap();
                assert!(result.is_none());
                if case == 2 {
                    assert_eq!(*frame.panic.take().unwrap().downcast::<u32>().unwrap(), 19);
                }
                assert!(frame
                    .registers
                    .stack_frame
                    .iter()
                    .all(|value| matches!(value, Value::Integer(-7))));
                assert!(environment.into_inner().try_borrow_mut(&ctx).is_ok());
            });
        }
    });
}

#[test]
fn fresh_entry_samples_replaced_environments_and_refuses_missing_or_borrowed_views() {
    Lua::empty().enter(|ctx| {
        let (plan, closure, environment, hints) = prepared(ctx);
        let width = usize::from(plan.registers);
        let Constant::String(key) = closure.prototype().constants[usize::from(plan.constant)]
        else {
            panic!()
        };
        let mut values = vec![Value::Integer(-7); width];
        let mut pc = 0;
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut slots = vec![abi::Slot::from_value(Value::Integer(-7)); width];
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: width,
                panic: None,
                projection: None,
                read_hints: None,
            };
            assert!(
                unsafe { plan.invoke_fresh(never, &mut frame, &mut slots, 0, 64) }
                    .unwrap()
                    .is_none()
            );
            frame.read_hints = Some(&hints);
            {
                let _conflict = environment.into_inner().borrow_mut(&ctx);
                assert!(
                    unsafe { plan.invoke_fresh(never, &mut frame, &mut slots, 0, 64) }
                        .unwrap()
                        .is_none()
                );
            }
            hints.hint(plan.helper_pc as usize).unwrap().remember(None);
            assert!(
                unsafe { plan.invoke_fresh(never, &mut frame, &mut slots, 0, 64) }
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                (
                    frame.count.calls,
                    frame.count.table_reads,
                    frame.count.upvalue_reads
                ),
                (0, 0, 0)
            );
            for turn in 0..3 {
                let current = if turn == 0 {
                    environment
                } else {
                    Table::new(&ctx)
                };
                let callback = Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return));
                current.set(ctx, key, callback).unwrap();
                closure.set_upvalue(
                    &ctx,
                    usize::from(plan.upvalue),
                    UpValue::new(&ctx, UpValueState::Closed(current.into())),
                );
                current.get_canonical_string(ctx, key, hints.hint(plan.helper_pc as usize));
                let exit = unsafe { plan.invoke_fresh(probe, &mut frame, &mut slots, 0, 64) }
                    .unwrap()
                    .unwrap();
                assert_eq!(exit.instructions, 1);
                same(
                    frame.registers.stack_frame[usize::from(plan.destination)],
                    callback.into(),
                );
                assert!(current.into_inner().try_borrow_mut(&ctx).is_ok());
                assert!(environment.into_inner().try_borrow_mut(&ctx).is_ok());
            }
            assert_eq!(
                (
                    frame.count.calls,
                    frame.count.completed,
                    frame.count.table_reads,
                    frame.count.upvalue_reads
                ),
                (3, 3, 3, 3)
            );
            let open = frame
                .registers
                .open_test_upvalue(&ctx, crate::types::RegisterIndex(0));
            closure.set_upvalue(&ctx, usize::from(plan.upvalue), open);
            assert!(
                unsafe { plan.invoke_fresh(never, &mut frame, &mut slots, 0, 64) }
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                (
                    frame.count.calls,
                    frame.count.table_reads,
                    frame.count.upvalue_reads
                ),
                (3, 3, 3)
            );
        });
    });
}

#[test]
fn checked_capability_keeps_its_closure_identity_revalidation() {
    Lua::empty().enter(|ctx| {
        let (plan, closure, environment, hints) = prepared(ctx);
        let peer = Closure::load_with_env(ctx, None, SOURCE, environment).unwrap();
        let width = usize::from(plan.registers);
        let mut values = vec![Value::Integer(-7); width];
        let mut pc = 0;
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut slots = vec![abi::Slot::from_value(Value::Integer(-7)); width];
            let mut frame = helpers::Frame {
                ctx,
                closure: peer,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: width,
                panic: None,
                projection: None,
                read_hints: Some(&hints),
            };
            assert!(plan
                .with_binding(closure, &hints, |bound| unsafe {
                    bound.invoke(never, &mut frame, &mut slots, 0, 64)
                })
                .unwrap()
                .unwrap()
                .is_none());
            assert_eq!(
                (
                    frame.count.calls,
                    frame.count.table_reads,
                    frame.count.upvalue_reads
                ),
                (0, 0, 0)
            );
            assert!(environment.into_inner().try_borrow_mut(&ctx).is_ok());
        });
    });
}
