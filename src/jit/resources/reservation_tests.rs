use super::*;

#[test]
fn additional_capacity_stays_charged_independently_without_allocating() {
    let host = Ledger::try_host(65536).unwrap();
    let ledger = Ledger::try_child(4096, host.clone()).unwrap();
    let baseline = host.accounted();
    let original = Reservation::new(ledger.clone(), 8).unwrap();
    ledger.fail_after(0);
    let additional = original.additional(13).unwrap();
    assert_eq!((ledger.current(), host.accounted()), (21, baseline + 21));
    let mut bytes = allocator_api2::vec::Vec::<u8, _>::new_in(BudgetAllocator(ledger.clone()));
    assert!(bytes.try_reserve_exact(1).is_err());
    drop(original);
    assert_eq!((ledger.current(), host.accounted()), (13, baseline + 13));
    drop(additional);
    assert_eq!((ledger.current(), host.accounted()), (0, baseline));
}

#[test]
fn additional_child_and_host_refusals_preserve_existing_capacity_then_recover() {
    for parent_denied in [false, true] {
        let host = Ledger::try_host(65536).unwrap();
        let ledger = Ledger::try_child(4096, host.clone()).unwrap();
        let original = Reservation::new(ledger.clone(), 8).unwrap();
        let baseline = host.accounted();
        if parent_denied {
            host.set_limit(baseline + 12);
        } else {
            ledger.set_limit(20);
        }
        for _ in 0..2 {
            assert!(original.additional(13).is_err());
            assert_eq!((ledger.current(), host.accounted()), (8, baseline));
        }
        host.set_limit(65536);
        ledger.set_limit(4096);
        drop(original.additional(13).unwrap());
        assert_eq!((ledger.current(), host.accounted()), (8, baseline));
        assert!(original.additional(usize::MAX).is_err());
        assert_eq!((ledger.current(), host.accounted()), (8, baseline));
        drop(original);
        assert_eq!((ledger.current(), host.accounted()), (0, baseline - 8));
    }
}

#[test]
fn additional_owner_survives_original_and_external_ledger_until_unwinding() {
    let host = Ledger::try_host(65536).unwrap();
    let baseline = host.accounted();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ledger = Ledger::try_child(4096, host.clone()).unwrap();
        let original = Reservation::new(ledger.clone(), 8).unwrap();
        let additional = original.additional(13).unwrap();
        drop(original);
        drop(ledger);
        assert_eq!(
            host.accounted(),
            baseline + LedgerRef::allocation_bytes() + 13
        );
        std::hint::black_box(&additional);
        panic!("reservation unwind");
    }));
    assert!(result.is_err());
    assert_eq!(host.accounted(), baseline);
}
