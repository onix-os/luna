use super::*;
use crate::jit::{resources::Ledger, work::Limits, JitConfig};

fn snapshot(source: &[u8]) -> Snapshot {
    crate::Lua::empty().enter(|ctx| {
        let prototype = crate::FunctionPrototype::compile(ctx, "relocations", source).unwrap();
        Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
    })
}

#[test]
fn exact_relocation_limit_admits_and_refusal_preserves_peer_before_allocation() {
    let source = snapshot(b"local t={} t.x=40 t.y=2 return t.x+t.y");
    let snapshots = source.operations.allocator().0.clone();
    let snapshot_baseline = snapshots.current();
    let total = MappingCounter::new(Ledger::new(usize::MAX));
    let metadata = BudgetAllocator(Ledger::new(2 * 1024 * 1024));
    let defaults = Limits::from(&JitConfig::default());
    let compile_source = |limits, failure| {
        compile_in(
            &source,
            total.clone(),
            8 * 1024 * 1024,
            metadata.clone(),
            limits,
            failure,
        )
    };
    let calibration = compile_source(defaults, Failure::None).unwrap();
    let count = calibration.relocations;
    assert!(count > 1);
    drop(calibration);
    assert_eq!(total.load(Ordering::Relaxed), 0);
    assert_eq!(metadata.0.current(), 0);

    let peer = compile_in(
        &snapshot(b"return 42"),
        total.clone(),
        8 * 1024 * 1024,
        metadata.clone(),
        defaults,
        Failure::None,
    )
    .unwrap();
    let baseline = (total.load(Ordering::Relaxed), metadata.0.current());
    for failure in [Failure::None, Failure::Allocate, Failure::Protect] {
        let result = compile_source(
            Limits {
                relocations: count - 1,
                ..defaults
            },
            failure,
        );
        assert!(matches!(
            result,
            Err(JitError::ResourceLimit("native relocations"))
        ));
        assert_eq!(
            (total.load(Ordering::Relaxed), metadata.0.current()),
            baseline
        );
        assert_eq!(snapshots.current(), snapshot_baseline);
        let mut slots = vec![Slot::from_value(crate::Value::Nil); peer.registers];
        let exit = peer.invoke(&mut slots, 0, 64);
        assert!(exit.instructions > 0);
        assert_eq!(slots[0].tag, abi::INTEGER);
        assert_eq!(slots[0].bits, 42);
    }

    let exact = Limits {
        relocations: count,
        ..defaults
    };
    for allocation in [false, true] {
        let refused = compile_source(exact, Failure::RefuseRelocationStorage(allocation));
        snapshots.set_limit(2 * 1024 * 1024);
        snapshots.fail_after(usize::MAX);
        assert!(matches!(
            refused,
            Err(JitError::ResourceLimit("native relocation staging"))
        ));
        assert_eq!(
            (total.load(Ordering::Relaxed), metadata.0.current()),
            baseline
        );
        assert_eq!(snapshots.current(), snapshot_baseline);
        let mut slots = vec![Slot::from_value(crate::Value::Nil); peer.registers];
        assert!(peer.invoke(&mut slots, 0, 64).instructions > 0);
        assert_eq!(slots[0].tag, abi::INTEGER);
        assert_eq!(slots[0].bits, 42);
    }
    assert!(matches!(
        compile_source(exact, Failure::Allocate),
        Err(JitError::Unavailable(_))
    ));
    assert!(matches!(
        compile_source(exact, Failure::Protect),
        Err(JitError::Unavailable(_))
    ));
    let recovered = compile_source(exact, Failure::None).unwrap();
    assert_eq!(recovered.relocations, count);
    drop(recovered);
    assert_eq!(
        (total.load(Ordering::Relaxed), metadata.0.current()),
        baseline
    );
    drop(peer);
    assert_eq!(total.load(Ordering::Relaxed), 0);
    assert_eq!(metadata.0.current(), 0);
    assert_eq!(snapshots.current(), snapshot_baseline);
}
