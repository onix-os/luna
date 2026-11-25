use std::{
    hash::{BuildHasher, Hash, Hasher},
    num::NonZeroUsize,
    ops::{Deref, DerefMut},
};

use super::MetadataMap;

pub(super) struct Index<V> {
    entries: MetadataMap<u64, V>,
    hint: Option<NonZeroUsize>,
    previous: Option<NonZeroUsize>,
}

impl<V> From<MetadataMap<u64, V>> for Index<V> {
    fn from(entries: MetadataMap<u64, V>) -> Self {
        Self {
            entries,
            hint: None,
            previous: None,
        }
    }
}

impl<V> Index<V> {
    pub fn lookup_mut(&mut self, key: u64) -> Option<&mut V> {
        let table = self.entries.raw_table();
        let cached = |hint: Option<NonZeroUsize>| {
            let offset = hint?;
            // Cached offsets identify live buckets until mutable map access.
            let bucket = unsafe { table.bucket(offset.get() - 1) };
            (unsafe { bucket.as_ref().0 } == key).then_some((offset, bucket))
        };
        let first = self.hint;
        let bucket = if let Some((_, bucket)) = cached(first) {
            bucket
        } else {
            let (offset, bucket) = if let Some(found) = cached(self.previous) {
                found
            } else {
                self.hint = None;
                self.previous = None;
                let mut hasher = self.entries.hasher().build_hasher();
                key.hash(&mut hasher);
                let bucket = table.find(hasher.finish(), |(id, _)| *id == key)?;
                // The found bucket belongs to this allocated table.
                let offset = unsafe { table.bucket_index(&bucket) };
                (NonZeroUsize::new(offset + 1).unwrap(), bucket)
            };
            self.previous = first;
            self.hint = Some(offset);
            bucket
        };
        // The exclusive index borrow scopes access to this live value.
        Some(unsafe { &mut (*bucket.as_ptr()).1 })
    }
}

impl<V> Deref for Index<V> {
    type Target = MetadataMap<u64, V>;

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl<V> DerefMut for Index<V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.hint = None;
        self.previous = None;
        &mut self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::{
        metadata_map,
        resources::{BudgetAllocator, Ledger, LedgerRef},
    };

    fn index<V>() -> (Index<V>, LedgerRef) {
        let ledger = Ledger::new(1024 * 1024);
        (metadata_map(BudgetAllocator(ledger.clone())).into(), ledger)
    }

    #[test]
    fn two_hints_occupy_only_two_machine_words() {
        assert_eq!(
            std::mem::size_of::<Index<u64>>(),
            std::mem::size_of::<MetadataMap<u64, u64>>() + 2 * std::mem::size_of::<usize>()
        );
    }

    #[test]
    fn cold_lookup_matches_inserted_hashes_for_wide_keys() {
        let (mut map, _) = index();
        let keys = [0, 1, u64::MAX, 1 << 63, u64::MAX - 1, 256];
        for key in keys {
            map.insert(key, key);
        }
        for _ in 0..8 {
            for key in keys {
                assert_eq!(map.lookup_mut(key).copied(), Some(key));
            }
        }
    }

    #[test]
    fn repeated_hits_reborrow_the_same_live_entry() {
        let (mut map, _) = index();
        map.insert(7, [1u64, 2]);
        for value in 0..128 {
            map.lookup_mut(7).unwrap()[0] = value;
            let offset = map.hint.unwrap();
            assert_eq!(map.lookup_mut(7).unwrap()[0], value);
            assert_eq!(map.hint.unwrap(), offset);
        }
        assert_eq!(map.lookup_mut(7), Some(&mut [127, 2]));
    }

    #[test]
    fn alternating_hits_preserve_disjoint_entry_provenance() {
        let (mut map, ledger) = index();
        map.insert(1, vec![0u64, 1]);
        map.insert(2, vec![0u64, 2]);
        map.insert(3, vec![0u64, 3]);
        let bytes = ledger.current();
        for iteration in 0..128 {
            for key in [1, 2, 1, 2, 3, 2, 3, 1] {
                assert_eq!(map.lookup_mut(key).unwrap()[1], key);
                *map.lookup_mut(key).unwrap() = vec![iteration, key];
                let offset = map.hint.unwrap();
                assert_eq!(map.lookup_mut(key).unwrap(), &[iteration, key]);
                assert_eq!(map.hint.unwrap(), offset);
                assert_ne!(map.previous, Some(offset));
            }
        }
        assert_eq!(ledger.current(), bytes);
        assert_eq!(map.get(&1).unwrap(), &[127, 1]);
        assert!(map.hint.is_some());
        assert!(map.previous.is_some());
        drop(map);
        assert_eq!(ledger.current(), 0);
    }

    #[test]
    fn two_hints_invalidate_on_mutation_and_misses() {
        let (mut map, _) = index();
        map.insert(1, 10u64);
        map.insert(2, 20u64);
        for mutation in 0..4 {
            assert_eq!(map.lookup_mut(1), Some(&mut 10));
            assert_eq!(map.lookup_mut(2), Some(&mut 20));
            assert!(map.hint.is_some());
            assert!(map.previous.is_some());
            match mutation {
                0 => {
                    map.try_reserve(128).unwrap();
                }
                1 => {
                    map.shrink_to_fit();
                }
                2 => {
                    assert_eq!(map.insert(1, 10), Some(10));
                }
                _ => {
                    assert!(map.lookup_mut(3).is_none());
                }
            }
            assert!(map.hint.is_none());
            assert!(map.previous.is_none());
        }
        map.lookup_mut(1).unwrap();
        map.lookup_mut(2).unwrap();
        assert_eq!(map.remove(&1), Some(10));
        assert!(map.hint.is_none());
        assert!(map.previous.is_none());
        assert!(map.lookup_mut(1).is_none());
        assert_eq!(map.lookup_mut(2), Some(&mut 20));
    }

    #[test]
    fn shared_access_preserves_offsets_and_mutation_invalidates() {
        let (mut map, _) = index();
        map.insert(1, 10u64);
        assert_eq!(map.lookup_mut(1), Some(&mut 10));
        let offset = map.hint;
        assert_eq!(map.get(&1), Some(&10));
        assert_eq!(map.hint, offset);
        *map.lookup_mut(1).unwrap() = 11;
        *map.get_mut(&1).unwrap() = 12;
        assert!(map.hint.is_none());
        assert_eq!(map.lookup_mut(1), Some(&mut 12));
        assert_eq!(map.iter().count(), 1);
        assert_eq!(map.hint, offset);
        assert_eq!(map.lookup_mut(1), Some(&mut 12));
    }

    #[test]
    fn shared_entry_borrows_end_before_fresh_mutable_bucket_access() {
        let (mut map, _) = index();
        map.insert(1, vec![1u64]);
        map.insert(2, vec![2u64]);
        for value in 3..35 {
            map.lookup_mut(1).unwrap();
            map.lookup_mut(2).unwrap();
            let hints = (map.hint, map.previous);
            let first = &map[&1];
            assert_eq!(first.len(), 1);
            assert_eq!(map.values().map(Vec::len).sum::<usize>(), 2);
            assert_eq!((map.hint, map.previous), hints);
            *map.lookup_mut(1).unwrap() = vec![value];
            assert_eq!(map.get(&1).unwrap(), &[value]);
            *map.lookup_mut(2).unwrap() = vec![value + 1];
            assert_eq!(map.get(&2).unwrap(), &[value + 1]);
        }
    }

    #[test]
    fn growth_shrink_clear_and_reinsertion_refresh_the_hint() {
        let (mut map, _) = index();
        map.insert(7, 70u64);
        for count in [32, 128, 512] {
            assert_eq!(map.lookup_mut(7), Some(&mut 70));
            map.try_reserve(count).unwrap();
            assert!(map.hint.is_none());
            assert_eq!(map.lookup_mut(7), Some(&mut 70));
        }
        map.shrink_to_fit();
        assert_eq!(map.lookup_mut(7), Some(&mut 70));
        assert_eq!(map.remove(&7), Some(70));
        assert!(map.lookup_mut(7).is_none());
        map.insert(7, 71);
        assert_eq!(map.lookup_mut(7), Some(&mut 71));
        map.clear();
        assert!(map.lookup_mut(7).is_none());
        map.insert(7, 72);
        assert_eq!(map.lookup_mut(7), Some(&mut 72));
    }

    #[test]
    fn container_moves_and_value_replacement_preserve_live_addresses() {
        let (mut first, _) = index();
        first.insert(1, vec![1, 2, 3]);
        first.lookup_mut(1).unwrap().push(4);
        let mut second = Box::new(first);
        assert_eq!(second.lookup_mut(1).unwrap(), &[1, 2, 3, 4]);
        *second.lookup_mut(1).unwrap() = vec![5, 6];
        let mut third = *second;
        assert_eq!(third.lookup_mut(1).unwrap(), &[5, 6]);
        assert_eq!(third.insert(1, vec![7]), Some(vec![5, 6]));
        assert_eq!(third.lookup_mut(1).unwrap(), &[7]);
    }

    #[test]
    fn failed_reservation_keeps_values_and_releases_all_charges() {
        let (mut map, ledger) = index();
        map.insert(1, 9u64);
        assert_eq!(map.lookup_mut(1), Some(&mut 9));
        let before = ledger.current();
        ledger.fail_after(0);
        assert!(map.try_reserve(4096).is_err());
        assert!(map.hint.is_none());
        assert_eq!(map.lookup_mut(1), Some(&mut 9));
        assert_eq!(ledger.current(), before);
        drop(map);
        assert_eq!(ledger.current(), 0);
    }

    #[test]
    fn panicking_mutation_invalidates_before_unwinding() {
        let (mut map, _) = index();
        for key in 0..16 {
            map.insert(key, key * 2);
        }
        assert_eq!(map.lookup_mut(7), Some(&mut 14));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            map.retain(|_, _| panic!("injected retention panic"));
        }));
        assert!(result.is_err());
        assert!(map.hint.is_none());
        for key in 0..16 {
            assert_eq!(map.lookup_mut(key).copied(), Some(key * 2));
        }
    }

    #[test]
    fn hints_do_not_keep_removed_values_alive() {
        let (mut map, ledger) = index();
        let value = std::rc::Rc::new(17);
        map.insert(1, value.clone());
        for _ in 0..16 {
            assert_eq!(**map.lookup_mut(1).unwrap(), 17);
            assert_eq!(std::rc::Rc::strong_count(&value), 2);
        }
        drop(map.remove(&1));
        assert_eq!(std::rc::Rc::strong_count(&value), 1);
        assert!(map.lookup_mut(1).is_none());
        drop(map);
        assert_eq!(ledger.current(), 0);
    }

    #[test]
    fn zero_sized_values_and_misses_do_not_reuse_another_key() {
        let (mut map, _) = index();
        map.insert(1, ());
        map.insert(2, ());
        for key in [1, 1, 2, 2, 3, 1] {
            assert_eq!(map.lookup_mut(key).is_some(), key != 3);
            assert_eq!(map.hint.is_some(), key != 3);
        }
    }

    #[test]
    fn mixed_operations_match_an_independent_ordered_map() {
        let (mut map, _) = index();
        let mut model = std::collections::BTreeMap::new();
        let mut state = 0x9876543210abcdefu64;
        for step in 0..1024 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let key = (state >> 32) % 64;
            match state % 5 {
                0 => {
                    assert_eq!(map.insert(key, step), model.insert(key, step));
                }
                1 => {
                    assert_eq!(map.remove(&key), model.remove(&key));
                }
                2 => {
                    map.try_reserve(1).unwrap();
                }
                _ => {
                    for _ in 0..3 {
                        assert_eq!(map.lookup_mut(key).copied(), model.get(&key).copied());
                    }
                }
            }
        }
        for key in 0..64 {
            assert_eq!(map.lookup_mut(key).copied(), model.get(&key).copied());
        }
    }
}
