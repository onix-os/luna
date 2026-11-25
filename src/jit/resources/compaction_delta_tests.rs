use super::*;
use allocator_api2::vec::Vec;
use std::{
    cell::RefCell,
    hash::{Hash, Hasher},
    rc::Rc,
};

type Action = Rc<RefCell<Option<Box<dyn FnOnce()>>>>;

struct Key {
    id: u64,
    action: Action,
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Key {}

impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        let action = self.action.borrow_mut().take();
        if let Some(action) = action {
            action();
        }
    }
}

fn values(ledger: LedgerRef, action: &Action) -> super::super::MetadataMap<Key, u64> {
    let mut values = super::super::metadata_map(BudgetAllocator(ledger));
    values.try_reserve(128).unwrap();
    for id in 0..8 {
        values.insert(
            Key {
                id,
                action: action.clone(),
            },
            id + 1,
        );
    }
    values
}

#[test]
fn map_delta_excludes_unrelated_shared_ledger_release_during_rehash() {
    let ledger = Ledger::new(1 << 20);
    let action: Action = Rc::new(RefCell::new(None));
    let mut values = values(ledger.clone(), &action);
    let before = values.raw_table().allocation_info().1.size();
    let mut noise = Vec::<u8, _>::new_in(BudgetAllocator(ledger.clone()));
    noise.try_reserve_exact(65536).unwrap();
    let shared_before = ledger.current();
    *action.borrow_mut() = Some(Box::new(move || drop(noise)));
    let Compaction::Compacted(bytes) = Compactor::default().map(&mut values) else {
        panic!("not compacted")
    };
    let after = values.raw_table().allocation_info().1.size();
    assert_eq!(bytes, before - after);
    assert_eq!(shared_before - ledger.current(), bytes + 65536);
    assert_eq!(values.len(), 8);
    for (key, value) in &values {
        assert_eq!(key.id + 1, *value);
    }
    drop(values);
    assert_eq!(ledger.current(), 0);
}

#[test]
fn map_delta_excludes_unrelated_shared_ledger_growth_during_rehash() {
    let ledger = Ledger::new(1 << 20);
    let action: Action = Rc::new(RefCell::new(None));
    let mut values = values(ledger.clone(), &action);
    let before = values.raw_table().allocation_info().1.size();
    let storage = Rc::new(RefCell::new(None));
    let destination = storage.clone();
    let allocator = BudgetAllocator(ledger.clone());
    *action.borrow_mut() = Some(Box::new(move || {
        let mut noise = Vec::<u8, _>::new_in(allocator);
        noise.try_reserve_exact(65536).unwrap();
        *destination.borrow_mut() = Some(noise);
    }));
    let Compaction::Compacted(bytes) = Compactor::default().map(&mut values) else {
        panic!("not compacted")
    };
    let after = values.raw_table().allocation_info().1.size();
    assert_eq!(bytes, before - after);
    assert!(bytes > 0);
    assert_eq!(ledger.current(), after + 65536);
    drop(values);
    drop(storage);
    assert_eq!(ledger.current(), 0);
}

#[test]
fn vector_delta_is_exact_with_other_shared_ledger_storage_retained() {
    let ledger = Ledger::new(1 << 20);
    let allocator = BudgetAllocator(ledger.clone());
    let mut noise = Vec::<u8, _>::new_in(allocator.clone());
    noise.try_reserve_exact(65536).unwrap();
    let mut values = Vec::<u64, _>::new_in(allocator);
    values.try_reserve_exact(128).unwrap();
    values.extend(0..8);
    assert!(matches!(
        Compactor::default().vector(&mut values),
        Compaction::Compacted(960)
    ));
    assert_eq!(values.as_slice(), &[0, 1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(ledger.current(), 65536 + 64);
    values.clear();
    assert!(matches!(
        Compactor::default().vector(&mut values),
        Compaction::Compacted(64)
    ));
    assert_eq!(ledger.current(), 65536);
    drop(noise);
    assert_eq!(ledger.current(), 0);
}
