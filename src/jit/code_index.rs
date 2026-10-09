use std::{
    cell::Cell,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use super::MetadataMap;

pub(super) struct Index<V> {
    entries: MetadataMap<u64, V>,
    hint: Cell<Option<(u64, NonNull<V>)>>,
}

impl<V> From<MetadataMap<u64, V>> for Index<V> {
    fn from(entries: MetadataMap<u64, V>) -> Self {
        Self {
            entries,
            hint: Cell::new(None),
        }
    }
}

impl<V> Index<V> {
    pub fn lookup_mut(&mut self, key: u64) -> Option<&mut V> {
        let mut pointer = match self.hint.take().filter(|(previous, _)| *previous == key) {
            Some((_, pointer)) => pointer,
            None => NonNull::from(self.entries.get_mut(&key)?),
        };
        self.hint.set(Some((key, pointer)));
        // Map access invalidates the hint; this exclusive lookup reborrows its live entry.
        Some(unsafe { pointer.as_mut() })
    }
}

impl<V> Deref for Index<V> {
    type Target = MetadataMap<u64, V>;

    fn deref(&self) -> &Self::Target {
        self.hint.set(None);
        &self.entries
    }
}

impl<V> DerefMut for Index<V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.hint.set(None);
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
    fn repeated_hits_reborrow_the_same_live_entry() {
        let (mut map, _) = index();
        map.insert(7, [1u64, 2]);
        for value in 0..128 {
            map.lookup_mut(7).unwrap()[0] = value;
            let pointer = map.hint.get().unwrap().1;
            assert_eq!(map.lookup_mut(7).unwrap()[0], value);
            assert_eq!(map.hint.get().unwrap().1, pointer);
        }
        assert_eq!(map.lookup_mut(7), Some(&mut [127, 2]));
    }

    #[test]
    fn shared_and_mutable_access_invalidate_before_exposing_entries() {
        let (mut map, _) = index();
        map.insert(1, 10u64);
        assert_eq!(map.lookup_mut(1), Some(&mut 10));
        assert_eq!(map.get(&1), Some(&10));
        assert!(map.hint.get().is_none());
        *map.lookup_mut(1).unwrap() = 11;
        *map.get_mut(&1).unwrap() = 12;
        assert!(map.hint.get().is_none());
        assert_eq!(map.lookup_mut(1), Some(&mut 12));
        assert_eq!(map.iter().count(), 1);
        assert!(map.hint.get().is_none());
        assert_eq!(map.lookup_mut(1), Some(&mut 12));
    }

    #[test]
    fn growth_shrink_clear_and_reinsertion_refresh_the_hint() {
        let (mut map, _) = index();
        map.insert(7, 70u64);
        for count in [32, 128, 512] {
            assert_eq!(map.lookup_mut(7), Some(&mut 70));
            map.try_reserve(count).unwrap();
            assert!(map.hint.get().is_none());
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
        assert!(map.hint.get().is_none());
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
        assert!(map.hint.get().is_none());
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
            assert_eq!(map.hint.get().map(|(id, _)| id), (key != 3).then_some(key));
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
