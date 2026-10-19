use std::{
    ops::Index,
    sync::atomic::{AtomicU8, Ordering},
};

use allocator_api2::vec::Vec;

use super::{resources::BudgetAllocator, JitError};

#[derive(Debug)]
pub(super) struct EntryBytes {
    bytes: Vec<AtomicU8, BudgetAllocator>,
}

#[derive(Clone, Copy)]
pub(crate) struct Hint<'a>(&'a AtomicU8);

impl<'a> Hint<'a> {
    #[cfg(test)]
    pub(crate) fn new(byte: &'a AtomicU8) -> Self {
        Self(byte)
    }

    pub(crate) fn load(self) -> Option<usize> {
        (self.0.load(Ordering::Relaxed) >> 1)
            .checked_sub(1)
            .map(usize::from)
    }

    pub(crate) fn remember(self, offset: Option<usize>) {
        let encoded = offset
            .and_then(|offset| offset.checked_add(1))
            .filter(|encoded| *encoded <= 127)
            .unwrap_or(0) as u8;
        let flag = self.0.load(Ordering::Relaxed) & 1;
        self.0.store(flag | (encoded << 1), Ordering::Relaxed);
    }
}

impl EntryBytes {
    pub(super) fn try_new(
        flags: impl ExactSizeIterator<Item = bool>,
        allocator: BudgetAllocator,
    ) -> Result<Self, JitError> {
        let count = flags.len();
        let mut bytes = Vec::new_in(allocator);
        bytes
            .try_reserve_exact(count)
            .map_err(|_| JitError::ResourceLimit("JIT metadata"))?;
        bytes.extend(flags.map(|flag| AtomicU8::new(u8::from(flag))));
        Ok(Self { bytes })
    }

    pub(super) fn len(&self) -> usize {
        self.bytes.len()
    }

    pub(super) fn get(&self, pc: usize) -> Option<&bool> {
        if pc >= self.len() {
            return None;
        }
        Some(if self.bytes[pc].load(Ordering::Relaxed) & 1 != 0 {
            &true
        } else {
            &false
        })
    }

    #[cfg(test)]
    pub(super) fn set(&mut self, pc: usize, enabled: bool) {
        assert!(pc < self.len());
        let byte = self.bytes[pc].get_mut();
        *byte = (*byte & !1) | u8::from(enabled);
    }

    pub(super) fn hint(&self, pc: usize) -> Option<Hint<'_>> {
        self.bytes.get(pc).map(Hint)
    }

    #[cfg(test)]
    pub(super) fn retain_only(&mut self, pc: usize) {
        let enabled = self.get(pc).copied().unwrap_or(false);
        for byte in &mut self.bytes {
            *byte.get_mut() = 0;
        }
        if enabled {
            self.set(pc, true);
        }
    }

    #[cfg(test)]
    pub(super) fn iter(&self) -> impl ExactSizeIterator<Item = &bool> {
        (0..self.len()).map(|pc| self.get(pc).unwrap())
    }

    #[cfg(test)]
    pub(super) fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    #[cfg(test)]
    pub(super) fn allocator(&self) -> &BudgetAllocator {
        self.bytes.allocator()
    }
}

impl Index<usize> for EntryBytes {
    type Output = bool;
    fn index(&self, pc: usize) -> &Self::Output {
        self.get(pc).expect("entry PC exceeds source")
    }
}

#[cfg(test)]
impl PartialEq for EntryBytes {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::resources::Ledger;

    #[test]
    fn packed_flags_and_hint_preserve_every_entry_and_charge() {
        for count in [0, 1, 2, 7, 8, 9, 15, 16, 17, 63, 64, 65, 255, 256, 257] {
            let ledger = Ledger::new(4096);
            let flags: std::vec::Vec<_> = (0..count).map(|pc| pc % 3 == 0 || pc % 5 == 2).collect();
            let entries =
                EntryBytes::try_new(flags.iter().copied(), BudgetAllocator(ledger.clone()))
                    .unwrap();
            assert_eq!(entries.len(), count);
            assert_eq!(entries.capacity(), count);
            assert_eq!(entries.allocator().0.current(), count);
            assert_eq!(ledger.peak(), count);
            for value in [
                None,
                Some(0),
                Some(1),
                Some(63),
                Some(126),
                Some(127),
                Some(254),
                Some(usize::MAX),
            ] {
                for pc in 0..entries.len() {
                    let hint = entries.hint(pc).unwrap();
                    hint.remember(value);
                    assert_eq!(hint.load(), value.filter(|offset| *offset < 127));
                }
                assert!(entries.iter().copied().eq(flags.iter().copied()));
                for pc in [count, count + 1, usize::MAX] {
                    assert_eq!(entries.get(pc), None);
                    assert!(entries.hint(pc).is_none());
                }
            }
            for (pc, enabled) in flags.iter().enumerate() {
                assert_eq!(&entries[pc], enabled);
            }
            drop(entries);
            assert_eq!(ledger.current(), 0);
        }
    }

    #[test]
    fn entry_mutation_and_selection_do_not_enable_other_pcs() {
        let ledger = Ledger::new(4096);
        let mut entries =
            EntryBytes::try_new((0..73).map(|_| false), BudgetAllocator(ledger.clone())).unwrap();
        for pc in 0..entries.len() {
            entries.set(pc, true);
            entries.set(pc, false);
            assert!(entries.iter().all(|enabled| !enabled));
            entries.set(pc, true);
            entries.hint(pc).unwrap().remember(Some(126));
            entries.retain_only(pc);
            assert_eq!(entries.hint(pc).unwrap().load(), None);
            for index in 0..entries.len() {
                assert_eq!(entries[index], index == pc);
            }
            entries.retain_only(usize::MAX);
            assert!(entries.iter().all(|enabled| !enabled));
        }
        let peer =
            EntryBytes::try_new((0..73).map(|_| false), BudgetAllocator(ledger.clone())).unwrap();
        entries.hint(0).unwrap().remember(Some(17));
        assert_eq!(entries, peer);
        assert_eq!(ledger.current(), 146);
    }

    #[test]
    fn packed_entry_quota_refusal_releases_every_byte() {
        for count in [1, 2, 7, 8, 9, 64, 65, 256] {
            let ledger = Ledger::new(count - 1);
            assert!(
                EntryBytes::try_new((0..count).map(|_| true), BudgetAllocator(ledger.clone()))
                    .is_err()
            );
            assert_eq!(ledger.current(), 0);
            ledger.set_limit(count);
            let entries =
                EntryBytes::try_new((0..count).map(|_| true), BudgetAllocator(ledger.clone()))
                    .unwrap();
            assert!(entries.iter().all(|enabled| *enabled));
            assert_eq!(ledger.current(), count);
            drop(entries);
            assert_eq!(ledger.current(), 0);
        }
    }

    #[test]
    fn per_pc_hints_remain_independent_when_entry_flags_change() {
        let ledger = Ledger::new(4096);
        let mut entries = EntryBytes::try_new(
            (0..257).map(|pc| pc % 2 == 0),
            BudgetAllocator(ledger.clone()),
        )
        .unwrap();
        for round in 0..2 {
            let expected = |pc: usize| {
                if round == 0 {
                    (pc * 17 + 11) % 127
                } else {
                    pc / 127
                }
            };
            for pc in 0..entries.len() {
                entries.hint(pc).unwrap().remember(Some(expected(pc)));
            }
            for pc in 0..entries.len() {
                assert_eq!(entries.hint(pc).unwrap().load(), Some(expected(pc)));
                let enabled = entries[pc];
                entries.set(pc, !enabled);
                assert_eq!(entries[pc], !enabled);
                assert_eq!(entries.hint(pc).unwrap().load(), Some(expected(pc)));
            }
        }
        assert_eq!(ledger.current(), 257);
    }

    #[test]
    fn concurrent_hint_updates_preserve_immutable_entry_flags() {
        let ledger = Ledger::new(4096);
        let entries = EntryBytes::try_new(
            (0..17).map(|pc| pc % 3 == 0),
            BudgetAllocator(ledger.clone()),
        )
        .unwrap();
        std::thread::scope(|scope| {
            for worker in 0..4 {
                let entries = &entries;
                scope.spawn(move || {
                    for iteration in 0..128 {
                        let pc = iteration % entries.len();
                        let hint = entries.hint(pc).unwrap();
                        hint.remember(Some((iteration + worker) % 256));
                        assert!(hint.load().is_none_or(|offset| offset < 127));
                        assert_eq!(entries[pc], pc % 3 == 0);
                    }
                });
            }
        });
        assert_eq!(ledger.current(), 17);
    }

    #[test]
    fn entry_bytes_match_original_storage_and_remain_send_sync() {
        fn shared<T: Send + Sync>() {}
        shared::<EntryBytes>();
        assert_eq!(std::mem::size_of::<AtomicU8>(), std::mem::size_of::<bool>());
        assert_eq!(
            std::mem::align_of::<AtomicU8>(),
            std::mem::align_of::<bool>()
        );
        assert_eq!(
            std::mem::size_of::<EntryBytes>(),
            std::mem::size_of::<Vec<bool, BudgetAllocator>>()
        );
    }
}
