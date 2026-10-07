use super::*;
use crate::jit::{
    owner::Shared,
    resources::{owned, Ledger},
    work, JitConfig,
};
use crate::{
    types::{ConstantIndex8, RegisterIndex, UpValueIndex, VarCount},
    Value,
};

fn source(read: u8, result: u8, right: RCIndex, arithmetic: Arithmetic) -> Snapshot {
    let left = RCIndex::Register(RegisterIndex(read));
    let dest = RegisterIndex(result);
    let op = match arithmetic {
        Arithmetic::Add => Operation::Add { dest, left, right },
        Arithmetic::Sub => Operation::Sub { dest, left, right },
        Arithmetic::Mul => Operation::Mul { dest, left, right },
    };
    Snapshot {
        operations: owned(&[
            Operation::GetUpValue {
                dest: RegisterIndex(read),
                source: UpValueIndex(0),
            },
            op,
            Operation::SetUpValue {
                dest: UpValueIndex(0),
                source: dest,
            },
            Operation::Return {
                start: RegisterIndex(0),
                count: VarCount::constant(0),
            },
        ]),
        constants: owned(&[Slot::from_value(Value::Integer(-7))]),
        registers: 3,
        upvalues: 1,
        prototypes: 0,
    }
}

fn limits() -> work::Limits {
    work::Limits::from(&JitConfig::default())
}

fn resources() -> (BudgetAllocator, MappingCounter) {
    let root = Ledger::new(8 * 1024 * 1024);
    (
        BudgetAllocator(Ledger::child(2 * 1024 * 1024, root.clone())),
        MappingCounter::new(Ledger::child(2 * 1024 * 1024, root)),
    )
}

#[test]
fn generated_compact_cells_preserve_all_register_and_capture_aliases() {
    let mut invocations = 0;
    for arithmetic in [Arithmetic::Add, Arithmetic::Sub, Arithmetic::Mul] {
        for read in 0..3 {
            for result in 0..3 {
                for right in [
                    RCIndex::Register(RegisterIndex(0)),
                    RCIndex::Register(RegisterIndex(1)),
                    RCIndex::Register(RegisterIndex(2)),
                    RCIndex::Constant(ConstantIndex8(0)),
                ] {
                    let snapshot = source(read, result, right, arithmetic);
                    let (metadata, mappings) = resources();
                    let code = compile(
                        &snapshot,
                        mappings.clone(),
                        2 * 1024 * 1024,
                        metadata.clone(),
                        limits(),
                        Failure::None,
                    )
                    .unwrap();
                    for capture in [None, Some(0), Some(1), Some(2)] {
                        for initial in [[3, 5, 11], [i64::MAX, i64::MIN, -1], [-1, 0, i64::MAX]] {
                            let values = initial.map(Value::Integer);
                            let mut frame = code.binding.prepare(&values, capture, 13).unwrap();
                            let output = code.invoke(&mut frame).unwrap();
                            let mut expected = initial;
                            let mut upper = 13;
                            expected[usize::from(read)] =
                                capture.map(|i| expected[i]).unwrap_or(upper);
                            let right = match right {
                                RCIndex::Register(index) => expected[usize::from(index.0)],
                                RCIndex::Constant(_) => -7,
                            };
                            let left = expected[usize::from(read)];
                            let value = match arithmetic {
                                Arithmetic::Add => left.wrapping_add(right),
                                Arithmetic::Sub => left.wrapping_sub(right),
                                Arithmetic::Mul => left.wrapping_mul(right),
                            };
                            expected[usize::from(result)] = value;
                            if let Some(index) = capture {
                                expected[index] = expected[usize::from(result)];
                            } else {
                                upper = expected[usize::from(result)];
                            }
                            let mut actual = initial;
                            actual[usize::from(read)] = output.read;
                            actual[usize::from(result)] = output.result;
                            if let Some(index) = capture {
                                actual[index] = output.capture;
                            }
                            assert_eq!(
                                actual, expected,
                                "{arithmetic:?} read={read} result={result} capture={capture:?}"
                            );
                            assert_eq!(
                                output.capture,
                                capture.map(|i| expected[i]).unwrap_or(upper)
                            );
                            invocations += 1;
                        }
                    }
                    drop(code);
                    assert_eq!(metadata.0.current(), 0);
                    assert_eq!(mappings.load(Ordering::Relaxed), 0);
                    assert_eq!(mappings.requested(), 0);
                }
            }
        }
    }
    assert_eq!(invocations, 1296);
}

#[test]
fn compact_cells_support_register_255_without_copying_reference_prefixes() {
    let mut snapshot = source(
        255,
        255,
        RCIndex::Constant(ConstantIndex8(0)),
        Arithmetic::Sub,
    );
    snapshot.registers = 256;
    let (metadata, mappings) = resources();
    let code = compile(
        &snapshot,
        mappings.clone(),
        2 * 1024 * 1024,
        metadata.clone(),
        limits(),
        Failure::None,
    )
    .unwrap();
    let values = [Value::Nil; 256];
    let mut frame = code.binding.prepare(&values, None, i64::MIN).unwrap();
    assert_eq!(frame.cells.len(), 4);
    let expected = i64::MIN.wrapping_sub(-7);
    assert_eq!(
        code.invoke(&mut frame).unwrap(),
        Outputs {
            capture: expected,
            read: expected,
            result: expected
        }
    );
    frame.indices[0] = 4;
    assert!(code.invoke(&mut frame).is_err());
    drop(code);
    assert_eq!(metadata.0.current(), 0);
    assert_eq!(mappings.load(Ordering::Relaxed), 0);
}

#[test]
fn compact_preparation_rejects_unreadable_inputs_but_allows_overwritten_values() {
    let ordinary = Binding::new(&source(
        1,
        2,
        RCIndex::Register(RegisterIndex(0)),
        Arithmetic::Add,
    ))
    .unwrap();
    assert!(ordinary.prepare(&[Value::Nil; 3], None, 7).is_none());
    assert!(ordinary
        .prepare(&[Value::Integer(2), Value::Nil, Value::Nil], None, 7)
        .is_some());
    assert!(ordinary
        .prepare(&[Value::Integer(2), Value::Nil, Value::Nil], Some(2), 7)
        .is_none());
    assert!(ordinary.prepare(&[Value::Integer(2); 2], None, 7).is_none());
    assert!(ordinary
        .prepare(&[Value::Integer(2); 3], Some(3), 7)
        .is_none());
    for right in [
        RCIndex::Register(RegisterIndex(1)),
        RCIndex::Constant(ConstantIndex8(0)),
    ] {
        let plan = Binding::new(&source(1, 2, right, Arithmetic::Add)).unwrap();
        assert!(plan.prepare(&[Value::Nil; 3], None, 7).is_some());
    }
    let mut invalid = source(1, 2, RCIndex::Constant(ConstantIndex8(0)), Arithmetic::Add);
    invalid.constants[0] = Slot::from_value(Value::Number(1.5));
    assert!(Binding::new(&invalid).is_err());
    invalid.constants[0] = Slot::from_value(Value::Integer(7));
    invalid.operations[3] = Operation::Return {
        start: RegisterIndex(0),
        count: VarCount::variable(),
    };
    assert!(Binding::new(&invalid).is_err());
}

#[test]
fn compact_verifier_rejects_ir_signature_and_source_binding_changes() {
    use cranelift_codegen::ir::InstructionData;
    let isa = native_builder(cranelift_native::builder()).unwrap();
    let module = JITModule::new(isa);
    let source = source(1, 2, RCIndex::Register(RegisterIndex(0)), Arithmetic::Sub);
    let binding = Binding::new(&source).unwrap();
    let original = binding.program(module.isa());
    binding.verify(&original, module.isa()).unwrap();
    let mut mutations = 0;
    for block in original.function.layout.blocks() {
        for inst in original.function.layout.block_insts(block) {
            let mut program = binding.program(module.isa());
            let changed = match &mut program.function.dfg.insts[inst] {
                InstructionData::Load { offset, .. } | InstructionData::Store { offset, .. } => {
                    *offset = 8.into();
                    true
                }
                InstructionData::Binary { args, .. } => {
                    args.swap(0, 1);
                    true
                }
                _ => false,
            };
            if changed {
                assert!(binding.verify(&program, module.isa()).is_err());
                mutations += 1;
            }
        }
    }
    assert_eq!(mutations, 8);
    for field in 0..6 {
        let mut program = binding.program(module.isa());
        match field {
            0 => program.function.signature.params.pop().map(|_| ()).unwrap(),
            1 => program
                .function
                .signature
                .returns
                .push(AbiParam::new(types::I64)),
            2 => program.function.name = UserFuncName::user(0, 1),
            3 => program.binding.pattern.read.0 = 0,
            4 => program.binding.returns = (1, 1),
            5 => program.binding.constant = Some(7),
            _ => unreachable!(),
        }
        assert!(binding.verify(&program, module.isa()).is_err());
    }
}

#[test]
fn compact_owners_and_failed_compilation_preserve_live_leases() {
    let snapshot = source(1, 2, RCIndex::Register(RegisterIndex(0)), Arithmetic::Mul);
    let (metadata, mappings) = resources();
    let workspace = snapshot.operations.allocator().0.clone();
    let initial = workspace.current();
    let build = |failure, limit, limits| {
        compile(
            &snapshot,
            mappings.clone(),
            limit,
            metadata.clone(),
            limits,
            failure,
        )
    };
    let code = build(Failure::None, 2 * 1024 * 1024, limits()).unwrap();
    let owner_before = metadata.0.current();
    let owner = Shared::try_new(code, metadata.clone()).unwrap();
    assert_eq!(
        metadata.0.current() - owner_before,
        Shared::<Code>::allocation_bytes()
    );
    let lease = owner.clone();
    drop(owner);
    let baseline = (
        workspace.current(),
        metadata.0.current(),
        mappings.load(Ordering::Relaxed),
        mappings.requested(),
    );
    for failure in [Failure::Allocate, Failure::Protect] {
        assert!(build(failure, 2 * 1024 * 1024, limits()).is_err());
        assert_eq!(
            (
                workspace.current(),
                metadata.0.current(),
                mappings.load(Ordering::Relaxed),
                mappings.requested()
            ),
            baseline
        );
        let mut frame = lease
            .binding
            .prepare(&[Value::Integer(3); 3], None, 7)
            .unwrap();
        assert_eq!(lease.invoke(&mut frame).unwrap().capture, 21);
    }
    assert!(build(Failure::None, 0, limits()).is_err());
    let mut tight = limits();
    tight.instructions = 0;
    assert!(build(Failure::None, 2 * 1024 * 1024, tight).is_err());
    let mut tight = limits();
    tight.blocks = 0;
    assert!(build(Failure::None, 2 * 1024 * 1024, tight).is_err());
    let limit = workspace.limit();
    workspace.set_limit(workspace.current());
    assert!(build(Failure::None, 2 * 1024 * 1024, limits()).is_err());
    workspace.set_limit(limit);
    let limit = metadata.0.limit();
    metadata.0.set_limit(metadata.0.current());
    assert!(build(Failure::None, 2 * 1024 * 1024, limits()).is_err());
    metadata.0.set_limit(limit);
    let mut reached_success = false;
    for allowance in 0..32 {
        metadata.0.fail_after(allowance);
        let attempt = build(Failure::None, 2 * 1024 * 1024, limits());
        metadata.0.fail_after(usize::MAX);
        reached_success = attempt.is_ok();
        drop(attempt);
        assert_eq!(
            (
                workspace.current(),
                metadata.0.current(),
                mappings.load(Ordering::Relaxed),
                mappings.requested()
            ),
            baseline
        );
        if reached_success {
            break;
        }
    }
    assert!(reached_success);
    assert_eq!(
        (
            workspace.current(),
            metadata.0.current(),
            mappings.load(Ordering::Relaxed),
            mappings.requested()
        ),
        baseline
    );
    let other = Binding::new(&source(
        1,
        2,
        RCIndex::Register(RegisterIndex(0)),
        Arithmetic::Sub,
    ))
    .unwrap();
    let mut frame = other.prepare(&[Value::Integer(3); 3], None, 7).unwrap();
    assert!(lease.invoke(&mut frame).is_err());
    drop(lease);
    assert_eq!(workspace.current(), initial);
    assert_eq!(metadata.0.current(), 0);
    assert_eq!(mappings.load(Ordering::Relaxed), 0);
    assert_eq!(mappings.requested(), 0);
}
