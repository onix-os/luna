#[cfg(not(miri))]
use super::readonly_lookup::ReadCode;
use crate::jit::entry_admission::Plan;

pub(in crate::jit) enum Entry {
    #[cfg(not(miri))]
    Lookup(ReadCode),
    Decline(Plan),
}

impl Entry {
    #[cfg(not(miri))]
    pub(in crate::jit) fn lookup(&self) -> Option<&ReadCode> {
        match self {
            Self::Lookup(code) => Some(code),
            Self::Decline(_) => None,
        }
    }

    pub(in crate::jit) fn relocations(&self) -> usize {
        match self {
            #[cfg(not(miri))]
            Self::Lookup(code) => code.relocations,
            Self::Decline(_) => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        jit::{
            ir::Snapshot,
            owner::Shared,
            resources::{self, BudgetAllocator, Ledger},
        },
        opcode::Operation,
        types::{RegisterIndex as R, VarCount},
        Value,
    };

    fn plan() -> Plan {
        let source = Snapshot {
            prepared_keys: None,
            operations: resources::owned(&[
                Operation::Add {
                    dest: R(0),
                    left: R(0).into(),
                    right: R(1).into(),
                },
                Operation::NumericForLoop {
                    base: R(2),
                    jump: -2,
                },
                Operation::Return {
                    start: R(0),
                    count: VarCount::constant(1),
                },
            ]),
            constants: resources::owned(&[]),
            registers: 6,
            upvalues: 0,
            prototypes: 0,
        };
        Plan::recognize(&source).unwrap()
    }

    #[test]
    fn exact_descriptor_quota_and_shared_lease_reclaim_all_charges() {
        let bytes = Shared::<Entry>::allocation_bytes();
        for limit in [bytes - 1, bytes, bytes + 1] {
            let ledger = Ledger::new(limit);
            let entry = Shared::try_new(Entry::Decline(plan()), BudgetAllocator(ledger.clone()));
            assert_eq!(entry.is_ok(), limit >= bytes);
            if let Ok(entry) = entry {
                assert_eq!(ledger.current(), bytes);
                assert_eq!(entry.relocations(), 0);
                let lease = entry.clone();
                drop(entry);
                assert_eq!(ledger.current(), bytes);
                drop(lease);
            }
            assert_eq!(ledger.current(), 0);
        }
    }

    #[test]
    fn declined_entry_keeps_no_source_buffer_and_survives_allocation_refusal() {
        let plan = plan();
        let ledger = Ledger::new(4096);
        ledger.fail_after(0);
        assert!(Shared::try_new(Entry::Decline(plan), BudgetAllocator(ledger.clone())).is_err());
        assert_eq!(ledger.current(), 0);
        ledger.fail_after(usize::MAX);
        let entry = Shared::try_new(Entry::Decline(plan), BudgetAllocator(ledger.clone())).unwrap();
        let frame = [
            Value::Nil,
            Value::Nil,
            Value::Integer(0),
            Value::Integer(4),
            Value::Integer(1),
            Value::Nil,
        ];
        assert!(
            matches!(&*entry,Entry::Decline(plan) if plan.refuses(&frame,0,64)&&plan.refuses(&frame,1,64))
        );
        #[cfg(not(miri))]
        assert!(entry.lookup().is_none());
        drop(entry);
        assert_eq!(ledger.current(), 0);
    }
}
