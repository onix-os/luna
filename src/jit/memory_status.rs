use std::sync::atomic::{AtomicBool, Ordering};

use super::{atomic_owner::AtomicShared, resources::BudgetAllocator, JitError};

#[derive(Default)]
pub(super) struct MemoryStatus {
    pub quota_refused: AtomicBool,
    pub metadata_refused: AtomicBool,
    pub unavailable: AtomicBool,
}

impl MemoryStatus {
    pub fn try_new(allocator: BudgetAllocator) -> Result<AtomicShared<Self>, JitError> {
        AtomicShared::try_new(Self::default(), allocator)
            .map_err(|_| JitError::ResourceLimit("JIT metadata"))
    }

    pub fn error(&self) -> Option<JitError> {
        if self.metadata_refused.load(Ordering::Relaxed) {
            Some(JitError::ResourceLimit("JIT metadata"))
        } else if self.quota_refused.load(Ordering::Relaxed) {
            Some(JitError::ResourceLimit("native mappings"))
        } else if self.unavailable.load(Ordering::Relaxed) {
            Some(JitError::Unavailable(
                "native memory allocation or protection denied",
            ))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::resources::Ledger;

    #[test]
    fn exact_parent_charge_and_shared_flags_survive_until_last_owner() {
        let bytes = AtomicShared::<MemoryStatus>::allocation_bytes();
        let host = Ledger::new(bytes);
        let metadata = Ledger::child(bytes, host.clone());
        let status = MemoryStatus::try_new(BudgetAllocator(metadata.clone())).unwrap();
        let provider = status.clone();
        assert_eq!((metadata.current(), host.current()), (bytes, bytes));
        provider.quota_refused.store(true, Ordering::Relaxed);
        assert!(matches!(
            status.error(),
            Some(JitError::ResourceLimit("native mappings"))
        ));
        drop(status);
        assert_eq!((metadata.current(), host.current()), (bytes, bytes));
        drop(provider);
        assert_eq!((metadata.current(), host.current()), (0, 0));
    }

    #[test]
    fn child_parent_and_underlying_refusals_roll_back_then_recover() {
        let bytes = AtomicShared::<MemoryStatus>::allocation_bytes();
        for cause in 0..3 {
            let host = Ledger::new(if cause == 1 { bytes - 1 } else { bytes });
            let metadata = Ledger::child(if cause == 0 { bytes - 1 } else { bytes }, host.clone());
            if cause == 2 {
                metadata.fail_after(0);
            }
            assert!(matches!(
                MemoryStatus::try_new(BudgetAllocator(metadata.clone())),
                Err(JitError::ResourceLimit("JIT metadata"))
            ));
            assert_eq!((metadata.current(), host.current()), (0, 0));
            assert_eq!(metadata.refusals(), 1);
            host.set_limit(bytes);
            metadata.set_limit(bytes);
            metadata.fail_after(usize::MAX);
            let recovered = MemoryStatus::try_new(BudgetAllocator(metadata.clone())).unwrap();
            assert!(recovered.error().is_none());
            drop(recovered);
            assert_eq!((metadata.current(), host.current()), (0, 0));
        }
    }

    #[test]
    fn error_precedence_matches_provider_protocol_for_every_flag_combination() {
        let status = MemoryStatus::default();
        for bits in 0..8 {
            status
                .metadata_refused
                .store(bits & 4 != 0, Ordering::Relaxed);
            status.quota_refused.store(bits & 2 != 0, Ordering::Relaxed);
            status.unavailable.store(bits & 1 != 0, Ordering::Relaxed);
            let error = status.error();
            if bits & 4 != 0 {
                assert!(matches!(
                    error,
                    Some(JitError::ResourceLimit("JIT metadata"))
                ));
            } else if bits & 2 != 0 {
                assert!(matches!(
                    error,
                    Some(JitError::ResourceLimit("native mappings"))
                ));
            } else if bits & 1 != 0 {
                assert!(matches!(
                    error,
                    Some(JitError::Unavailable(
                        "native memory allocation or protection denied"
                    ))
                ));
            } else {
                assert!(error.is_none());
            }
        }
    }
}
