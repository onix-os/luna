use allocator_api2::vec::Vec;

use crate::opcode::{CallTransition, Operation};

use super::{abi, exits, ir::Snapshot, resources::BudgetAllocator, JitError};

pub(super) struct Continuations {
    records: Vec<Option<CallTransition>, BudgetAllocator>,
}

#[derive(Clone, Copy)]
pub(super) struct Request<'a> {
    pub pc: usize,
    pub transition: CallTransition,
    _source: &'a Continuations,
}

fn source_transition(operation: Operation) -> Option<CallTransition> {
    match operation {
        Operation::Call {
            func,
            args,
            returns,
        } => Some(CallTransition::Call {
            func,
            args,
            returns,
        }),
        Operation::TailCall { func, args } => Some(CallTransition::TailCall { func, args }),
        Operation::Return { start, count } => Some(CallTransition::Return { start, count }),
        _ => None,
    }
}

impl Continuations {
    pub fn new(source: &Snapshot, allocator: BudgetAllocator) -> Result<Self, JitError> {
        let graph = super::flow::FlowGraph::new(source)?;
        let mut records = Vec::new_in(allocator);
        records
            .try_reserve_exact(source.operations.len())
            .map_err(|_| JitError::ResourceLimit("native continuations"))?;
        for (operation, node) in source.operations.iter().copied().zip(&graph.nodes) {
            let transition = source_transition(operation);
            if transition.is_some() && node.lowering != super::flow::Lowering::Interpreter {
                return Err(JitError::Compilation("native continuation lowering".into()));
            }
            records.push(transition);
        }
        let table = Self { records };
        table.verify(source)?;
        Ok(table)
    }

    fn verify(&self, source: &Snapshot) -> Result<(), JitError> {
        use crate::opcode::OpCode;
        if self.records.len() != source.operations.len()
            || self
                .records
                .iter()
                .copied()
                .zip(&source.operations)
                .any(|(record, operation)| record != OpCode::encode(*operation).call_transition())
        {
            return Err(JitError::Compilation(
                "native continuation source mismatch".into(),
            ));
        }
        Ok(())
    }

    pub fn resolve(&self, exit: &abi::Exit) -> Option<Request<'_>> {
        if exit.reason != exits::Kind::Interpreter as u32 {
            return None;
        }
        let pc = usize::try_from(exit.pc).ok()?;
        self.at(pc)
    }

    pub fn at(&self, pc: usize) -> Option<Request<'_>> {
        Some(Request {
            pc,
            transition: self.records.get(pc).copied().flatten()?,
            _source: self,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::resources::{self, Ledger};
    use super::*;
    use crate::types::{RegisterIndex, VarCount};

    fn snapshot(operation: Operation) -> Snapshot {
        Snapshot {
            operations: resources::owned(&[
                Operation::LoadConstant {
                    dest: RegisterIndex(0),
                    constant: crate::types::ConstantIndex16(0),
                },
                operation,
                Operation::Return {
                    start: RegisterIndex(0),
                    count: VarCount::constant(0),
                },
            ]),
            constants: resources::owned(&[abi::Slot::from_value(crate::Value::Integer(7))]),
            registers: 3,
            upvalues: 0,
            prototypes: 0,
        }
    }

    fn operations() -> [Operation; 6] {
        [
            Operation::Call {
                func: RegisterIndex(1),
                args: VarCount::constant(1),
                returns: VarCount::constant(0),
            },
            Operation::TailCall {
                func: RegisterIndex(1),
                args: VarCount::constant(1),
            },
            Operation::Return {
                start: RegisterIndex(0),
                count: VarCount::constant(1),
            },
            Operation::Call {
                func: RegisterIndex(1),
                args: VarCount::variable(),
                returns: VarCount::variable(),
            },
            Operation::TailCall {
                func: RegisterIndex(1),
                args: VarCount::variable(),
            },
            Operation::Return {
                start: RegisterIndex(0),
                count: VarCount::variable(),
            },
        ]
    }

    #[test]
    fn source_requests_preserve_all_operands_and_reject_other_exits() {
        for operation in operations() {
            let source = snapshot(operation);
            let table = Continuations::new(&source, BudgetAllocator(Ledger::new(4096))).unwrap();
            for pc in [0, 1, 2, 3, u64::MAX] {
                for reason in 0..=4 {
                    let exit = abi::Exit {
                        pc,
                        instructions: 1,
                        reason,
                    };
                    let request = table.resolve(&exit);
                    if reason == 0 && (pc == 1 || pc == 2) {
                        let request = request.unwrap();
                        assert_eq!(request.pc as u64, pc);
                        assert_eq!(
                            request.transition,
                            source_transition(source.operations[pc as usize]).unwrap()
                        );
                    } else {
                        assert!(request.is_none());
                    }
                }
            }
        }
    }

    #[test]
    fn changed_pc_operands_and_record_cardinality_fail_source_verification() {
        let changed_count = |count: VarCount| {
            if count.is_variable() {
                VarCount::constant(1)
            } else {
                VarCount::variable()
            }
        };
        for operation in operations() {
            let source = snapshot(operation);
            let allocator = BudgetAllocator(Ledger::new(4096));
            let mut table = Continuations::new(&source, allocator).unwrap();
            let original = table.records[1].unwrap();
            let changes = match original {
                CallTransition::Call {
                    func,
                    args,
                    returns,
                } => vec![
                    CallTransition::Call {
                        func: RegisterIndex(0),
                        args,
                        returns,
                    },
                    CallTransition::Call {
                        func,
                        args: changed_count(args),
                        returns,
                    },
                    CallTransition::Call {
                        func,
                        args,
                        returns: changed_count(returns),
                    },
                    CallTransition::TailCall { func, args },
                ],
                CallTransition::TailCall { func, args } => vec![
                    CallTransition::TailCall {
                        func: RegisterIndex(0),
                        args,
                    },
                    CallTransition::TailCall {
                        func,
                        args: changed_count(args),
                    },
                    CallTransition::Call {
                        func,
                        args,
                        returns: VarCount::constant(0),
                    },
                ],
                CallTransition::Return { start, count } => vec![
                    CallTransition::Return {
                        start: RegisterIndex(1),
                        count,
                    },
                    CallTransition::Return {
                        start,
                        count: changed_count(count),
                    },
                    CallTransition::TailCall {
                        func: start,
                        args: count,
                    },
                ],
            };
            for changed in changes {
                table.records[1] = Some(changed);
                assert!(table.verify(&source).is_err());
            }
            table.records[1] = None;
            assert!(table.verify(&source).is_err());
            table.records[1] = Some(original);
            table.records.swap(0, 1);
            assert!(table.verify(&source).is_err());
            table.records.swap(0, 1);
            table.records.pop();
            assert!(table.verify(&source).is_err());
        }
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    fn compile(
        source: &Snapshot,
        metadata: BudgetAllocator,
        mappings: resources::MappingCounter,
        failure: super::super::backend::Failure,
    ) -> super::super::backend::Code {
        super::super::backend::compile_continuations_in(
            source,
            mappings,
            128 * 1024,
            metadata,
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            failure,
        )
        .unwrap()
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn actual_native_exits_resolve_source_calls_without_granting_budget_or_guard_entries() {
        for operation in operations() {
            let source = snapshot(operation);
            let metadata = BudgetAllocator(Ledger::new(2 * 1024 * 1024));
            let mappings = resources::MappingCounter::new(Ledger::new(2 * 1024 * 1024));
            let code = compile(
                &source,
                metadata.clone(),
                mappings,
                super::super::backend::Failure::None,
            )
            .into_shared(metadata)
            .unwrap();
            drop(source);
            let table = code.continuations.as_ref().unwrap();
            for budget in [0, 1, 2, 64] {
                let mut slots = [abi::Slot::from_value(crate::Value::Nil); 3];
                let exit = code.invoke(&mut slots, 0, budget);
                if budget > 1 {
                    assert_eq!((exit.pc, exit.instructions, exit.reason), (1, 1, 0));
                    assert_eq!(slots[0].bits, 7);
                    assert_eq!(
                        table.resolve(&exit).unwrap().transition,
                        source_transition(operation).unwrap()
                    );
                } else {
                    assert_eq!(exit.reason, exits::Kind::Budget as u32);
                    assert!(table.resolve(&exit).is_none());
                }
            }
        }
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn optional_table_failures_and_late_owner_refusals_preserve_ordinary_code_and_peer_leases() {
        use super::super::backend::Failure;
        let metadata = BudgetAllocator(Ledger::new(2 * 1024 * 1024));
        let mappings = resources::MappingCounter::new(Ledger::new(2 * 1024 * 1024));
        let source = snapshot(operations()[0]);
        let peer = compile(&source, metadata.clone(), mappings.clone(), Failure::None)
            .into_shared(metadata.clone())
            .unwrap();
        let baseline = (
            metadata.0.current(),
            mappings.load(std::sync::atomic::Ordering::Relaxed),
        );
        for failure in [
            Failure::RefuseContinuationStorage,
            Failure::RefuseContinuationAllocation,
        ] {
            {
                let code = compile(&source, metadata.clone(), mappings.clone(), failure)
                    .into_shared(metadata.clone())
                    .unwrap();
                assert!(code.continuations.is_none());
                let mut slots = [abi::Slot::from_value(crate::Value::Nil); 3];
                assert_eq!(code.invoke(&mut slots, 0, 64).instructions, 1);
                assert_eq!(peer.invoke(&mut slots, 0, 64).instructions, 1);
            }
            assert_eq!(
                (
                    metadata.0.current(),
                    mappings.load(std::sync::atomic::Ordering::Relaxed)
                ),
                baseline
            );
        }
        let mut long = snapshot(operations()[0]);
        long.operations = resources::owned(&vec![source.operations[0]; 128]);
        long.operations.push(source.operations[1]);
        long.operations.push(source.operations[2]);
        let code = compile(&long, metadata.clone(), mappings.clone(), Failure::None);
        assert!(code.continuations.is_some());
        let previous_limit = metadata.0.limit();
        metadata.0.set_limit(metadata.0.current());
        let code = code.into_shared(metadata.clone()).unwrap();
        metadata.0.set_limit(previous_limit);
        assert!(code.continuations.is_none());
        let mut slots = [abi::Slot::from_value(crate::Value::Nil); 3];
        assert_eq!(code.invoke(&mut slots, 0, 64).instructions, 64);
        assert_eq!(slots[0].bits, 7);
        assert_eq!(peer.invoke(&mut slots, 0, 64).instructions, 1);
        drop(code);
        assert_eq!(
            (
                metadata.0.current(),
                mappings.load(std::sync::atomic::Ordering::Relaxed)
            ),
            baseline
        );
        drop(peer);
        assert_eq!(metadata.0.current(), 0);
        assert_eq!(mappings.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn continuation_storage_is_charged_and_failed_reservations_reclaim_it() {
        let source = snapshot(operations()[0]);
        let ledger = Ledger::new(4096);
        let bytes;
        {
            let table = Continuations::new(&source, BudgetAllocator(ledger.clone())).unwrap();
            bytes = ledger.current();
            assert!(bytes >= table.records.len() * std::mem::size_of::<Option<CallTransition>>());
        }
        assert_eq!(ledger.current(), 0);
        ledger.set_limit(bytes - 1);
        assert!(Continuations::new(&source, BudgetAllocator(ledger.clone())).is_err());
        assert_eq!(ledger.current(), 0);
    }
}
