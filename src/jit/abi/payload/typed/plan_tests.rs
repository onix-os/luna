use super::*;
use crate::{
    jit::resources,
    types::{ConstantIndex16, ConstantIndex8, RegisterIndex, VarCount},
    Value,
};

#[cfg(not(miri))]
#[path = "../../../../../examples/jit_support/workloads.rs"]
mod workloads;

#[cfg(not(miri))]
#[test]
fn typed_source_plan_maps_original_workload_boundaries() {
    fn visit(name: &str, prototype: &crate::FunctionPrototype<'_>) {
        let source = Snapshot::new(prototype, 4096, 2 * 1024 * 1024).unwrap();
        let mut pc = 0;
        while pc < source.operations.len() {
            if Plan::new(&source, pc, pc + 1).is_err() {
                eprintln!(
                    "typed coverage {name} boundary {pc}: {:?}",
                    source.operations[pc]
                );
                pc += 1;
                continue;
            }
            let mut end = pc + 1;
            while end < source.operations.len()
                && end - pc < MAX_STEPS
                && Plan::new(&source, pc, end + 1).is_ok()
            {
                end += 1;
            }
            let plan = Plan::new(&source, pc, end).unwrap();
            eprintln!(
                "typed coverage {name} span {pc}..{end}: {} bindings, {} canonical stores",
                plan.bindings.len(),
                plan.steps
                    .iter()
                    .filter(|step| step.store == Store::Canonical)
                    .count()
            );
            for operation in &source.operations[pc..end] {
                assert!(matches!(
                    operation,
                    Operation::Add { .. }
                        | Operation::Sub { .. }
                        | Operation::Mul { .. }
                        | Operation::Minus { .. }
                        | Operation::Move { .. }
                        | Operation::LoadConstant { .. }
                ));
            }
            pc = end;
        }
        for child in prototype.prototypes.iter() {
            visit(&format!("{name}/child"), child);
        }
    }
    crate::Lua::empty().enter(|ctx| {
        for workload in workloads::WORKLOADS {
            eprintln!(
                "typed coverage {} reference result {}",
                workload.name, workload.expected
            );
            let closure = crate::Closure::load(ctx, None, workload.source).unwrap();
            visit(workload.name, &closure.prototype());
        }
        for (name, source) in [
            ("oslo_predicate", workloads::PREDICATE_SOURCE),
            ("cold_config", workloads::COLD_SOURCE),
        ] {
            let closure = crate::Closure::load(ctx, None, source).unwrap();
            visit(name, &closure.prototype());
        }
    });
}

pub(super) fn snapshot(operations: &[Operation], registers: usize) -> Snapshot {
    let mut operations = resources::owned(operations);
    operations.push(Operation::Return {
        start: RegisterIndex(0),
        count: VarCount::constant(0),
    });
    Snapshot {
        prepared_keys: None,
        operations,
        constants: resources::owned(&[abi::Slot::from_value(Value::Integer(i64::MIN))]),
        registers,
        upvalues: 0,
        prototypes: 0,
    }
}

pub(super) fn sample() -> Snapshot {
    snapshot(
        &[
            Operation::Add {
                dest: RegisterIndex(2),
                left: RegisterIndex(0).into(),
                right: RegisterIndex(1).into(),
            },
            Operation::Mul {
                dest: RegisterIndex(0),
                left: RegisterIndex(2).into(),
                right: RegisterIndex(0).into(),
            },
            Operation::Move {
                dest: RegisterIndex(1),
                source: RegisterIndex(0),
            },
            Operation::Sub {
                dest: RegisterIndex(2),
                left: RegisterIndex(1).into(),
                right: RCIndex::Constant(ConstantIndex8(0)),
            },
            Operation::Minus {
                dest: RegisterIndex(2),
                source: RegisterIndex(2),
            },
            Operation::LoadConstant {
                dest: RegisterIndex(3),
                constant: ConstantIndex16(0),
            },
        ],
        5,
    )
}

pub(super) fn interpret(source: &Snapshot, start: usize, count: usize, values: &mut [Value<'_>]) {
    for operation in &source.operations[start..start + count] {
        let input = |operand: RCIndex| match operand {
            RCIndex::Register(index) => match values[usize::from(index.0)] {
                Value::Integer(value) => value,
                _ => panic!("noninteger oracle input"),
            },
            RCIndex::Constant(index) => source.constants[usize::from(index.0)].bits as i64,
        };
        let (dest, result) = match *operation {
            Operation::Add { dest, left, right } => (dest, input(left).wrapping_add(input(right))),
            Operation::Sub { dest, left, right } => (dest, input(left).wrapping_sub(input(right))),
            Operation::Mul { dest, left, right } => (dest, input(left).wrapping_mul(input(right))),
            Operation::Move { dest, source } => (dest, input(source.into())),
            Operation::Minus { dest, source } => (dest, input(source.into()).wrapping_neg()),
            Operation::LoadConstant { dest, constant } => {
                (dest, source.constants[usize::from(constant.0)].bits as i64)
            }
            _ => panic!("unsupported oracle operation"),
        };
        values[usize::from(dest.0)] = Value::Integer(result);
    }
}

fn run(plan: &Plan, values: &mut [Value<'_>], budget: u32) -> Option<u32> {
    with_bindings(plan, values, |pointers, destinations| {
        let mut results = [0i64; MAX_STEPS];
        let count = (budget as usize).min(plan.steps.len());
        for (index, step) in plan.steps[..count].iter().enumerate() {
            let input = |value: Input| match value {
                Input::Binding(index) => unsafe { pointers[usize::from(index)].read() },
                Input::Previous(index) => results[usize::from(index)],
                Input::Literal(value) => value,
            };
            let value = match step.expression {
                Expression::Copy(a) => input(a),
                Expression::Negate(a) => input(a).wrapping_neg(),
                Expression::Add(a, b) => input(a).wrapping_add(input(b)),
                Expression::Sub(a, b) => input(a).wrapping_sub(input(b)),
                Expression::Mul(a, b) => input(a).wrapping_mul(input(b)),
            };
            match step.store {
                Store::Typed(index) => unsafe { pointers[usize::from(index)].write(value) },
                Store::Canonical => unsafe {
                    super::super::terminal_write(
                        destinations[usize::from(step.destination)],
                        std::ptr::null_mut(),
                        value,
                    )
                },
            }
            results[index] = value;
        }
        count as u32
    })
}

pub(super) fn with_bindings<'gc, T>(
    plan: &Plan,
    values: &mut [Value<'gc>],
    body: impl FnOnce(&[*mut i64], &[*mut Value<'gc>]) -> T,
) -> Option<T> {
    if values.len() != plan.registers {
        return None;
    }
    let mut frame = super::super::Frame::new(values);
    let mut pointers = [std::ptr::null_mut(); 256];
    for (slot, register) in plan.bindings.iter().enumerate() {
        let descriptor = frame.bind(usize::from(*register))?;
        if descriptor.tag != abi::INTEGER || descriptor.pointer.is_null() {
            return None;
        }
        pointers[slot] = descriptor.pointer.cast();
    }
    let mut destinations = [std::ptr::null_mut(); 256];
    for step in &plan.steps {
        if step.store == Store::Canonical {
            destinations[usize::from(step.destination)] =
                unsafe { frame.values.as_ptr().add(usize::from(step.destination)) };
        }
    }
    Some(body(
        &pointers[..plan.bindings.len()],
        &destinations[..plan.registers],
    ))
}

#[test]
fn typed_source_plan_matches_every_prefix_and_fresh_suffix() {
    let source = sample();
    let plan = Plan::new(&source, 0, 6).unwrap();
    assert_eq!(&plan.bindings[..], &[0, 1]);
    assert_eq!(plan.steps[0].store, Store::Canonical);
    assert_eq!(plan.steps[1].store, Store::Typed(0));
    assert_eq!(
        plan.steps[4].expression,
        Expression::Negate(Input::Previous(3))
    );
    crate::Lua::empty().enter(|ctx| {
        for a in [i64::MIN, -1, 0, 1, i64::MAX] {
            for b in [i64::MIN, -1, 0, 1, i64::MAX] {
                for budget in (0..=7).chain([u32::MAX]) {
                    let initial = [
                        Value::Integer(a),
                        Value::Integer(b),
                        Value::Number(-0.0),
                        Value::Boolean(true),
                        Value::Table(crate::Table::new(&ctx)),
                    ];
                    let count = budget.min(6);
                    let mut actual = initial;
                    assert_eq!(run(&plan, &mut actual, budget), Some(count));
                    let mut expected = initial;
                    interpret(&source, 0, count as usize, &mut expected);
                    for (a, b) in actual.into_iter().zip(expected) {
                        super::super::assert_same(a, b);
                    }
                    if count < 6 {
                        let suffix = Plan::new(&source, count as usize, 6).unwrap();
                        assert_eq!(run(&suffix, &mut actual, 64), Some(6 - count));
                        interpret(&source, count as usize, 6 - count as usize, &mut expected);
                        for (a, b) in actual.into_iter().zip(expected) {
                            super::super::assert_same(a, b);
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn typed_source_plan_rejects_corrupted_dependencies_stores_and_admission() {
    let source = sample();
    let mut plan = Plan::new(&source, 0, 6).unwrap();
    for index in 0..plan.steps.len() {
        let original = plan.steps[index];
        for input in [
            Input::Previous(index as u16),
            Input::Previous(u16::MAX),
            Input::Binding(256),
            Input::Literal(99),
        ] {
            plan.steps[index].expression = Expression::Copy(input);
            assert!(plan.verify(&source).is_err());
        }
        for destination in [0, 1, 2, 3, 4, 255] {
            if destination != original.destination {
                plan.steps[index] = Step {
                    destination,
                    ..original
                };
                assert!(plan.verify(&source).is_err());
            }
        }
        for store in [
            Store::Typed(0),
            Store::Typed(1),
            Store::Typed(255),
            Store::Canonical,
        ] {
            if store != original.store {
                plan.steps[index] = Step { store, ..original };
                assert!(plan.verify(&source).is_err());
            }
        }
        plan.steps[index] = original;
    }
    let original = plan.steps[3];
    for previous in [0, 1, 3, 4, u16::MAX] {
        plan.steps[3].expression =
            Expression::Sub(Input::Previous(previous), Input::Literal(i64::MIN));
        assert!(plan.verify(&source).is_err());
    }
    plan.steps[3] = original;
    for register in [0, 2, 4, 255] {
        plan.bindings[1] = register;
        assert!(plan.verify(&source).is_err());
    }
    plan.bindings[1] = 1;
    plan.verify(&source).unwrap();
    for index in 0..2 {
        for value in [Value::Nil, Value::Number(7.0), Value::Boolean(false)] {
            let mut values = [Value::Integer(7); 5];
            values[index] = value;
            let before = values;
            assert_eq!(run(&plan, &mut values, 64), None);
            for (a, b) in values.into_iter().zip(before) {
                super::super::assert_same(a, b);
            }
        }
    }
    assert_eq!(run(&plan, &mut [Value::Integer(7); 4], 64), None);
}

#[test]
fn typed_source_plan_refuses_unsupported_ranges_and_releases_charges() {
    let source = sample();
    for (start, end) in [(0, 0), (6, 7), (0, 7), (7, 8), (usize::MAX, usize::MAX)] {
        assert!(Plan::new(&source, start, end).is_err());
    }
    let ledger = &source.operations.allocator().0;
    let before = ledger.current();
    let bindings = source.registers;
    let steps = 6 * std::mem::size_of::<Step>();
    for available in [0, bindings - 1, bindings, bindings + steps - 1] {
        ledger.set_limit(before + available);
        assert!(matches!(
            Plan::new(&source, 0, 6),
            Err(JitError::ResourceLimit(_))
        ));
        assert_eq!(ledger.current(), before);
    }
    ledger.set_limit(before + bindings + steps);
    let plan = Plan::new(&source, 0, 6).unwrap();
    assert_eq!(ledger.current(), before + bindings + steps);
    drop(plan);
    assert_eq!(ledger.current(), before);
    let source = snapshot(
        &[Operation::Move {
            dest: RegisterIndex(0),
            source: RegisterIndex(0),
        }; 65],
        1,
    );
    assert!(Plan::new(&source, 0, 64).is_ok());
    assert!(Plan::new(&source, 0, 65).is_err());
    let source = snapshot(
        &[Operation::Move {
            dest: RegisterIndex(255),
            source: RegisterIndex(255),
        }],
        256,
    );
    assert!(Plan::new(&source, 0, 1).is_ok());
}

#[test]
fn typed_source_plan_accepts_compiler_generated_arithmetic_spans() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(
            ctx,
            None,
            b"local a,b=... local x=a+b local y=x*a x=-x y=y-x return y",
        )
        .unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 2 * 1024 * 1024).unwrap();
        let start = source
            .operations
            .iter()
            .position(|op| matches!(op, Operation::Add { .. }))
            .unwrap();
        let end = source
            .operations
            .iter()
            .position(|op| matches!(op, Operation::Return { .. }))
            .unwrap();
        let plan = Plan::new(&source, start, end).unwrap();
        for budget in 0..=plan.steps.len() as u32 + 1 {
            let mut actual = [Value::Integer(i64::MAX); 256];
            let mut expected = actual;
            let count = budget.min(plan.steps.len() as u32);
            assert_eq!(
                run(&plan, &mut actual[..source.registers], budget),
                Some(count)
            );
            interpret(
                &source,
                start,
                count as usize,
                &mut expected[..source.registers],
            );
            for (a, b) in actual.into_iter().zip(expected) {
                super::super::assert_same(a, b);
            }
        }
    });
}
