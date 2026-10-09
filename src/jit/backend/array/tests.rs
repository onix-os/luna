use super::*;
use crate::{
    jit::{resources::Ledger, JitConfig},
    table::RawTable,
    Lua, Value,
};

fn source() -> Snapshot {
    Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(
            ctx,
            None,
            b"local t={} for i=1,20 do t[i]=i end local s=0 for i=1,20 do s=s+t[i] end return s",
        )
        .unwrap();
        Snapshot::new(&closure.prototype(), 4096, 2 * 1024 * 1024).unwrap()
    })
}

fn limits() -> work::Limits {
    work::Limits::from(&JitConfig::default())
}

#[test]
fn owned_kernels_release_workspace_and_keep_code_live_without_the_source() {
    let snapshot = source();
    let workspace = snapshot.operations.allocator().0.clone();
    let baseline = workspace.current();
    let metadata = Ledger::new(4 * 1024 * 1024);
    let total = MappingCounter::new(metadata.clone());
    let code = compile(
        &snapshot,
        total.clone(),
        1024 * 1024,
        BudgetAllocator(metadata.clone()),
        limits(),
        Failure::None,
    )
    .unwrap();
    assert_eq!(code.bindings.len(), 2);
    assert_eq!(workspace.current(), baseline);
    assert!(metadata.current() > 0 && total.load(Ordering::Relaxed) > 0);
    drop(snapshot);
    assert_eq!(workspace.current(), 0);
    Lua::empty().enter(|ctx| {
        let plan = code.bindings[0].plan;
        let mut raw = RawTable::with_capacity(&ctx, 32, 0);
        raw.array_mut().fill(Value::Nil);
        let table = crate::Table::from_parts(&ctx, raw, None);
        let mut canonical = vec![Value::Integer(0); code.registers];
        canonical[usize::from(plan.table)] = Value::Table(table);
        for (offset, value) in [1, 20, 1, 1].into_iter().enumerate() {
            canonical[usize::from(plan.base) + offset] = Value::Integer(value);
        }
        let mut slots: Vec<_> = canonical.iter().copied().map(Slot::from_value).collect();
        let outcome = code
            .invoke(ctx, &canonical, &mut slots, plan.start, 64)
            .unwrap();
        assert_eq!(outcome.counts.writes, 20);
        assert_eq!(outcome.exit.pc, plan.end as u64 + 1);
        for key in 1..=20 {
            assert!(matches!(table.get_value(ctx, key), Value::Integer(value) if value == key));
        }
    });
    drop(code);
    assert_eq!(total.load(Ordering::Relaxed), 0);
    assert_eq!(total.requested(), 0);
    assert_eq!(metadata.current(), 0);
}

#[test]
fn allocation_and_protection_failures_release_all_owned_storage() {
    let source = source();
    let baseline = source.operations.allocator().0.current();
    for failure in [Failure::Allocate, Failure::Protect] {
        let metadata = Ledger::new(4 * 1024 * 1024);
        let total = MappingCounter::new(metadata.clone());
        assert!(compile(
            &source,
            total.clone(),
            1024 * 1024,
            BudgetAllocator(metadata.clone()),
            limits(),
            failure
        )
        .is_err());
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(total.requested(), 0);
        assert_eq!(metadata.current(), 0);
        assert_eq!(source.operations.allocator().0.current(), baseline);
    }
    let metadata = Ledger::new(4 * 1024 * 1024);
    let total = MappingCounter::new(metadata.clone());
    assert!(compile(
        &source,
        total.clone(),
        1,
        BudgetAllocator(metadata.clone()),
        limits(),
        Failure::None
    )
    .is_err());
    assert_eq!(total.load(Ordering::Relaxed), 0);
    assert_eq!(metadata.current(), 0);
}

#[test]
fn insufficient_combined_work_keeps_the_ordinary_native_entry() {
    let source = source();
    let expansion = work::Expansion::admit(&source, limits()).unwrap();
    let mut work = limits();
    work.instructions = expansion.instructions * 2 - 1;
    let metadata = Ledger::new(4 * 1024 * 1024);
    let total = MappingCounter::new(metadata.clone());
    let code = compile_pair(
        &source,
        total.clone(),
        1024 * 1024,
        BudgetAllocator(metadata.clone()),
        work,
        Failure::None,
    )
    .unwrap();
    assert!(code.array_kernels.is_none());
    assert!(code.entries.iter().any(|&entry| entry));
    drop(code);
    assert_eq!(total.load(Ordering::Relaxed), 0);
    assert_eq!(metadata.current(), 0);
}

#[test]
fn optional_kernel_retirement_preserves_the_ordinary_image() {
    let source = source();
    let metadata = Ledger::new(4 * 1024 * 1024);
    let total = MappingCounter::new(metadata.clone());
    let mut code = compile_pair(
        &source,
        total.clone(),
        1024 * 1024,
        BudgetAllocator(metadata.clone()),
        limits(),
        Failure::None,
    )
    .unwrap();
    assert!(code.array_kernels.is_some());
    let bytes = total.load(Ordering::Relaxed);
    assert!(code.discard_optional_entries());
    assert!(total.load(Ordering::Relaxed) > 0 && total.load(Ordering::Relaxed) < bytes);
    let mut slots = vec![Slot::from_value(Value::Nil); code.registers];
    let exit = code.invoke(&mut slots, 0, 0);
    assert_eq!(exit.instructions, 0);
    drop(code);
    assert_eq!(total.load(Ordering::Relaxed), 0);
    assert_eq!(metadata.current(), 0);
}
