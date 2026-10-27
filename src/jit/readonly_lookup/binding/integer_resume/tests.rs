use super::*;
use crate::{
    jit::resources::{BudgetAllocator, Ledger},
    types::RegisterIndex,
    Callback, CallbackReturn, Lua,
};

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod integration;

fn same<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
    match (actual, expected) {
        (Value::Nil, Value::Nil) => {}
        (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
        (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
        (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
        (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
        (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
        _ => panic!("different canonical values"),
    }
}

fn same_all<'gc>(actual: &[Value<'gc>], expected: &[Value<'gc>]) {
    assert_eq!(actual.len(), expected.len());
    for (&actual, &expected) in actual.iter().zip(expected) {
        same(actual, expected);
    }
}

fn prepare<'gc>(ctx: crate::Context<'gc>) -> (Plan, Closure<'gc>, Table<'gc>, EntryBytes) {
    let env = Table::new(&ctx);
    env.set_field(
        ctx,
        "host_increment",
        Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return)),
    );
    let closure = Closure::load_with_env(
        ctx,
        None,
        b"local sum=0 for i=1,5000 do sum=sum+host_increment(i) end return sum",
        env,
    )
    .unwrap();
    let source = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
    let plan = Plan::recognize(&source).unwrap_or_else(|| panic!("source={:?}", source.operations));
    let hints = EntryBytes::try_new(
        source.operations.iter().map(|_| true),
        BudgetAllocator(Ledger::new(4096)),
    )
    .unwrap();
    let Constant::String(key) = closure.prototype().constants[usize::from(plan.lookup.constant)]
    else {
        panic!()
    };
    env.get_canonical_string(ctx, key, hints.hint(plan.lookup.helper_pc as usize));
    (plan, closure, env, hints)
}

unsafe extern "C" fn model(view: *mut View, budget: u32, exit: *mut abi::Exit) {
    unsafe {
        let view = &mut *view;
        let entry = (view.cookie[0] >> 32) as usize;
        let output = &mut *exit;
        *output = abi::Exit {
            pc: entry as u64,
            instructions: 0,
            reason: crate::jit::exits::Kind::Budget as u32,
        };
        if budget == 0 {
            return;
        }
        *view.cells[0] = (*view.cells[0]).wrapping_add(*view.cells[1]);
        output.pc = (entry + 1) as u64;
        output.instructions = 1;
        if budget == 1 {
            return;
        }
        let (next, overflow) = (*view.cells[2]).overflowing_add(*view.cells[4]);
        *view.cells[2] = next;
        output.instructions = 2;
        if overflow
            || if *view.cells[4] < 0 {
                next < *view.cells[3]
            } else {
                next > *view.cells[3]
            }
        {
            output.pc = (entry + 2) as u64;
            if budget == 2 {
                return;
            }
            view.finish = *view.cells[0];
            view.executed = 4;
            output.pc = (entry + 3) as u64;
            output.instructions = 3;
            if budget > 3 {
                output.reason = crate::jit::exits::Kind::Interpreter as u32
            }
        } else {
            *view.cells[5] = next;
            output.pc = (entry - 3) as u64;
            if budget == 2 {
                return;
            }
            view.executed = 1;
            output.pc = (entry - 2) as u64;
            output.instructions = 3;
            if budget == 3 {
                return;
            }
            view.argument = next;
            view.executed = 3;
            output.pc = (entry - 1) as u64;
            output.instructions = 4;
            if budget > 4 {
                output.reason = crate::jit::exits::Kind::Interpreter as u32
            }
        }
    }
}

#[test]
fn typed_loans_preserve_prefix_writes_and_fresh_binding_publication() {
    Lua::empty().enter(|ctx| {
        let (plan, closure, env, hints) = prepare(ctx);
        for (index, limit, step) in [
            (0, 3, 1),
            (3, 3, 1),
            (3, 0, -1),
            (0, 0, -1),
            (i64::MAX, i64::MAX, 1),
            (i64::MIN, i64::MIN, -1),
        ] {
            for binding in [
                Value::Integer(77),
                Value::Function(
                    Callback::from_fn(&ctx, |_, _, _| panic!("unexpected user call")).into(),
                ),
            ] {
                env.set_field(ctx, "host_increment", binding);
                for budget in [0, 1, 2, 3, 4, 5, 64, u32::MAX] {
                    let mut initial =
                        vec![Value::Integer(19); usize::from(plan.lookup.registers) + 2];
                    for (role, value) in
                        plan.roles
                            .into_iter()
                            .zip([i64::MAX, 1, index, limit, step, 19])
                    {
                        initial[usize::from(role)] = Value::Integer(value);
                    }
                    let before = initial.clone();
                    let mut pc = plan.entry;
                    crate::thread::LuaRegisters::with_test_frame(
                        ctx,
                        &mut pc,
                        &mut initial,
                        |mut registers| {
                            let outcome = unsafe {
                                plan.invoke(
                                    model,
                                    closure,
                                    &hints,
                                    &mut registers,
                                    plan.entry,
                                    budget,
                                )
                            }
                            .unwrap()
                            .unwrap();
                            assert_eq!(*registers.pc, plan.entry);
                            assert!(outcome.exit.instructions <= budget.min(64));
                            let sum = registers.stack_frame[usize::from(plan.roles[0])];
                            same(
                                sum,
                                if budget == 0 {
                                    Value::Integer(i64::MAX)
                                } else {
                                    Value::Integer(i64::MIN)
                                },
                            );
                            same_all(
                                &registers.stack_frame[usize::from(plan.lookup.registers)..],
                                &before[usize::from(plan.lookup.registers)..],
                            );
                            if outcome.reads != 0 {
                                same(
                                    registers.stack_frame[usize::from(plan.lookup.destination())],
                                    binding,
                                );
                            }
                        },
                    );
                }
            }
        }
    });
}

#[test]
fn admission_rejects_nonintegers_wrong_pc_and_unverified_shapes_before_mutation() {
    Lua::empty().enter(|ctx| {
        let (plan, closure, _, hints) = prepare(ctx);
        for role in plan.roles {
            for replacement in [
                Value::Nil,
                Value::Boolean(true),
                Value::Number(1.0),
                Value::Table(Table::new(&ctx)),
            ] {
                let mut values = vec![Value::Integer(7); usize::from(plan.lookup.registers)];
                values[usize::from(role)] = replacement;
                let before = values.clone();
                let mut pc = plan.entry;
                crate::thread::LuaRegisters::with_test_frame(
                    ctx,
                    &mut pc,
                    &mut values,
                    |mut registers| {
                        assert!(unsafe {
                            plan.invoke(model, closure, &hints, &mut registers, plan.entry, 64)
                        }
                        .unwrap()
                        .is_none());
                    },
                );
                same_all(&values, &before);
            }
        }
        for pc in [0, plan.entry - 1, plan.entry + 1, usize::MAX] {
            let mut values = vec![Value::Integer(7); usize::from(plan.lookup.registers)];
            let before = values.clone();
            let mut cursor = pc;
            crate::thread::LuaRegisters::with_test_frame(
                ctx,
                &mut cursor,
                &mut values,
                |mut registers| {
                    assert!(
                        unsafe { plan.invoke(model, closure, &hints, &mut registers, pc, 64) }
                            .unwrap()
                            .is_none()
                    );
                },
            );
            same_all(&values, &before);
        }
        let mut source = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
        let original = source.operations[plan.entry];
        source.operations[plan.entry] = Operation::Add {
            dest: RegisterIndex(plan.roles[2]),
            left: RCIndex::Register(RegisterIndex(plan.roles[2])),
            right: RCIndex::Register(RegisterIndex(plan.roles[1])),
        };
        assert!(Plan::recognize(&source).is_none());
        source.operations[plan.entry] = original;
        assert_eq!(Plan::recognize(&source), Some(plan));
    });
}
