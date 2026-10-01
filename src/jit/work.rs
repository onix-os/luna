use crate::opcode::Operation;

use super::{ir::Snapshot, JitConfig, JitError};

#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    pub instructions: usize,
    pub blocks: usize,
}

impl From<&JitConfig> for Limits {
    fn from(config: &JitConfig) -> Self {
        Self {
            instructions: config.max_ir_instructions,
            blocks: config.max_ir_blocks,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Expansion {
    pub instructions: usize,
    pub blocks: usize,
}

impl Expansion {
    pub fn admit(snapshot: &Snapshot, limits: Limits) -> Result<Self, JitError> {
        snapshot.verify()?;
        let mut expansion = Self {
            instructions: 256,
            blocks: 64,
        };
        expansion.check(limits)?;
        for &op in &snapshot.operations {
            expansion.add(op)?;
            expansion.check(limits)?;
        }
        Ok(expansion)
    }

    fn add(&mut self, op: Operation) -> Result<(), JitError> {
        use Operation::*;
        let extra = match op {
            LoadNil { count, .. } => 2 * usize::from(count),
            Move { .. }
            | LoadConstant { .. }
            | LoadBool { .. }
            | NewTable { .. }
            | GetTable { .. }
            | SetTable { .. }
            | GetUpTable { .. }
            | SetUpTable { .. }
            | SetList { .. }
            | Call { .. }
            | TailCall { .. }
            | Return { .. }
            | VarArgs { .. }
            | MarkToBeClosed { .. }
            | Jump { .. }
            | Test { .. }
            | TestSet { .. }
            | Closure { .. }
            | NumericForPrep { .. }
            | NumericForLoop { .. }
            | GenericForCall { .. }
            | GenericForLoop { .. }
            | Method { .. }
            | Concat { .. }
            | GetUpValue { .. }
            | SetUpValue { .. }
            | Length { .. }
            | Eq { .. }
            | Less { .. }
            | LessEq { .. }
            | Not { .. }
            | Minus { .. }
            | Add { .. }
            | Sub { .. }
            | Mul { .. }
            | Div { .. }
            | IDiv { .. }
            | Mod { .. }
            | Pow { .. }
            | BitAnd { .. }
            | BitOr { .. }
            | BitXor { .. }
            | ShiftLeft { .. }
            | ShiftRight { .. }
            | BitNot { .. } => 0,
        };
        self.instructions = self
            .instructions
            .checked_add(256 + extra)
            .ok_or(JitError::ResourceLimit("IR instruction expansion overflow"))?;
        self.blocks = self
            .blocks
            .checked_add(32)
            .ok_or(JitError::ResourceLimit("IR block expansion overflow"))?;
        Ok(())
    }

    fn check(self, limits: Limits) -> Result<(), JitError> {
        if self.instructions > limits.instructions {
            Err(JitError::ResourceLimit("IR instructions"))
        } else if self.blocks > limits.blocks {
            Err(JitError::ResourceLimit("IR blocks"))
        } else {
            Ok(())
        }
    }

    pub fn verify_actual(self, instructions: usize, blocks: usize) -> Result<(), JitError> {
        if instructions > self.instructions || blocks > self.blocks {
            Err(JitError::Compilation(
                "IR expansion exceeds admitted bound".into(),
            ))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RegisterIndex, VarCount};

    fn snapshot(count: u8, repeats: usize) -> Snapshot {
        let allocator = super::super::resources::BudgetAllocator(
            super::super::resources::Ledger::new(2 * 1024 * 1024),
        );
        let mut operations = allocator_api2::vec::Vec::new_in(allocator.clone());
        operations.try_reserve_exact(repeats + 1).unwrap();
        for _ in 0..repeats {
            operations.push(Operation::LoadNil {
                dest: RegisterIndex(0),
                count,
            });
        }
        operations.push(Operation::Return {
            start: RegisterIndex(0),
            count: VarCount::constant(0),
        });
        Snapshot {
            operations,
            constants: allocator_api2::vec::Vec::new_in(allocator),
            registers: 256,
            upvalues: 0,
            prototypes: 0,
        }
    }

    #[test]
    fn nil_fanout_is_charged_even_when_opcode_count_is_identical() {
        let limits = Limits::from(&JitConfig::default());
        let small = Expansion::admit(&snapshot(0, 1), limits).unwrap();
        let large = Expansion::admit(&snapshot(255, 1), limits).unwrap();
        assert_eq!(large.instructions - small.instructions, 510);
        assert_eq!(large.blocks, small.blocks);
    }

    #[test]
    fn exact_bounds_admit_and_one_less_refuses_independently() {
        let source = snapshot(255, 1);
        let bound = Expansion::admit(&source, Limits::from(&JitConfig::default())).unwrap();
        assert_eq!(
            Expansion::admit(
                &source,
                Limits {
                    instructions: bound.instructions,
                    blocks: bound.blocks
                }
            )
            .unwrap(),
            bound
        );
        assert!(matches!(
            Expansion::admit(
                &source,
                Limits {
                    instructions: bound.instructions - 1,
                    blocks: bound.blocks
                }
            ),
            Err(JitError::ResourceLimit("IR instructions"))
        ));
        assert!(matches!(
            Expansion::admit(
                &source,
                Limits {
                    instructions: bound.instructions,
                    blocks: bound.blocks - 1
                }
            ),
            Err(JitError::ResourceLimit("IR blocks"))
        ));
    }

    #[test]
    fn expansion_overflow_and_actual_bound_violation_are_refused() {
        let op = Operation::LoadNil {
            dest: RegisterIndex(0),
            count: 255,
        };
        let mut bound = Expansion {
            instructions: usize::MAX,
            blocks: 0,
        };
        assert!(matches!(
            bound.add(op),
            Err(JitError::ResourceLimit("IR instruction expansion overflow"))
        ));
        let mut bound = Expansion {
            instructions: 0,
            blocks: usize::MAX,
        };
        assert!(matches!(
            bound.add(op),
            Err(JitError::ResourceLimit("IR block expansion overflow"))
        ));
        let bound = Expansion {
            instructions: 100,
            blocks: 10,
        };
        bound.verify_actual(100, 10).unwrap();
        assert!(matches!(
            bound.verify_actual(101, 10),
            Err(JitError::Compilation(_))
        ));
        assert!(matches!(
            bound.verify_actual(100, 11),
            Err(JitError::Compilation(_))
        ));
    }

    #[test]
    fn malformed_snapshots_are_rejected_before_work_admission() {
        let mut source = snapshot(255, 1);
        source.registers = 1;
        assert!(matches!(
            Expansion::admit(
                &source,
                Limits {
                    instructions: 1,
                    blocks: 1
                }
            ),
            Err(JitError::Compilation(_))
        ));
    }

    #[test]
    fn default_work_bounds_refuse_large_fanout_before_graph_allocation() {
        let source = snapshot(255, 1800);
        let before = source.operations.allocator().0.current();
        assert!(matches!(
            Expansion::admit(&source, Limits::from(&JitConfig::default())),
            Err(JitError::ResourceLimit("IR instructions"))
        ));
        assert_eq!(source.operations.allocator().0.current(), before);
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn backend_work_refusal_precedes_graph_metadata_and_mapping_allocation() {
        use super::super::{
            backend,
            resources::{BudgetAllocator, Ledger, MappingCounter},
        };
        use std::sync::atomic::Ordering;

        for instructions in [true, false] {
            let source = snapshot(0, 1);
            let storage = source.operations.allocator().0.clone();
            storage.set_limit(storage.current());
            let metadata = Ledger::new(0);
            let mappings = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
            let before = (storage.current(), storage.peak(), storage.refusals());
            let mut limits = Limits::from(&JitConfig::default());
            let expected = if instructions {
                limits.instructions = 1;
                "IR instructions"
            } else {
                limits.blocks = 1;
                "IR blocks"
            };
            assert!(matches!(
                backend::compile_in(
                    &source,
                    mappings.clone(),
                    0,
                    BudgetAllocator(metadata.clone()),
                    limits,
                    backend::Failure::None,
                ),
                Err(JitError::ResourceLimit(reason)) if reason == expected
            ));
            assert_eq!(
                (storage.current(), storage.peak(), storage.refusals()),
                before
            );
            assert_eq!(
                (metadata.current(), metadata.peak(), metadata.refusals()),
                (0, 0, 0)
            );
            assert_eq!(mappings.load(Ordering::Relaxed), 0);
        }
    }
}
