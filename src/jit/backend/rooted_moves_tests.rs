use super::*;
use crate::{
    jit::resources::{owned, Ledger},
    types::{RegisterIndex as R, VarCount},
    Lua, Table, Value,
};

fn source() -> Snapshot {
    Snapshot {
        operations: owned(&[
            Operation::Move {
                dest: R(1),
                source: R(0),
            },
            Operation::LoadNil {
                dest: R(0),
                count: 1,
            },
            Operation::Move {
                dest: R(2),
                source: R(1),
            },
            Operation::Move {
                dest: R(0),
                source: R(2),
            },
            Operation::Return {
                start: R(0),
                count: VarCount::constant(0),
            },
        ]),
        constants: owned(&[]),
        registers: 3,
        upvalues: 0,
        prototypes: 0,
    }
}

fn compile_source(source: &Snapshot, failure: Failure) -> Result<Code, JitError> {
    let root = Ledger::new(8 * 1024 * 1024);
    compile_selected_rooted(
        source,
        MappingCounter::new(Ledger::child(2 * 1024 * 1024, root.clone())),
        2 * 1024 * 1024,
        BudgetAllocator(Ledger::child(2 * 1024 * 1024, root)),
        super::super::work::Limits::from(&super::super::JitConfig::default()),
        Selection {
            projected: false,
            scoped_helpers: true,
            leaf: false,
            cell_kernel: false,
            integer_activation: false,
            failure,
        },
        true,
    )
}

#[test]
fn rooted_moves_execute_without_helpers_at_every_budget_and_entry() {
    let source = source();
    let code = compile_source(&source, Failure::None).unwrap();
    Lua::empty().enter(|ctx| {
        let table = Table::new(&ctx);
        for start in 0..5 {
            for budget in 0..=8 {
                let mut values = [Value::Table(table); 3];
                let mut expected = values;
                let mut roots = values;
                let mut slots = [Slot::from_value(Value::Nil); 3];
                assert!(abi::roots::capture(&mut roots, &mut slots, &values));
                let exit = unsafe {
                    code.invoke_raw(slots.as_mut_ptr(), start, budget, std::ptr::null_mut())
                };
                let mut pc = start;
                let mut count = 0;
                while count < budget && pc < 4 {
                    match source.operations[pc] {
                        Operation::Move { dest, source } => {
                            expected[usize::from(dest.0)] = expected[usize::from(source.0)]
                        }
                        Operation::LoadNil { dest, .. } => {
                            expected[usize::from(dest.0)] = Value::Nil
                        }
                        _ => unreachable!(),
                    }
                    pc += 1;
                    count += 1;
                }
                assert_eq!((exit.pc, exit.instructions), (pc as u64, count));
                assert!(abi::roots::materialize(&roots, &slots, &mut values));
                for (actual, expected) in values.into_iter().zip(expected) {
                    match (actual, expected) {
                        (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
                        (Value::Nil, Value::Nil) => {}
                        _ => panic!("rooted move mismatch"),
                    }
                }
            }
        }
    });
}

#[test]
fn rooted_move_corruptions_refuse_before_installation() {
    use super::super::tags::TransferCorruption::*;
    for fault in [
        Payload,
        Source,
        Destination,
        Targets,
        Count,
        MissingWrite,
        MissingEdge,
        ProgramCounter,
        Ordinal,
        GrowthWrite,
        GrowthEdge,
        ExtraStore,
    ] {
        assert!(compile_source(&source(), Failure::CorruptTransfer(fault)).is_err());
    }
    for fault in [
        super::super::tags::BindingFault::StorePc,
        super::super::tags::BindingFault::Destination,
    ] {
        assert!(compile_source(&source(), Failure::CorruptBinding(fault)).is_err());
    }
}

#[test]
fn rooted_mode_refuses_reference_constants_and_heap_helpers() {
    let mut source = source();
    source.constants = owned(&[Slot {
        tag: abi::REFERENCE,
        bits: 0,
    }]);
    for op in [
        Operation::LoadConstant {
            dest: R(0),
            constant: crate::types::ConstantIndex16(0),
        },
        Operation::GetUpValue {
            dest: R(0),
            source: crate::types::UpValueIndex(0),
        },
    ] {
        source.operations[0] = op;
        source.upvalues = 1;
        assert!(!super::super::helper_flow::rooted_moves_admitted(&source));
        assert!(compile_source(&source, Failure::None).is_err());
    }
}
