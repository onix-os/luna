use super::*;
use crate::jit::resources::Ledger;

#[test]
fn exact_utf8_capacity_charge_and_explicit_caller_transfer() {
    for text in ["compiler worker compilation", "λ\0é🤖"] {
        let ledger = Ledger::new(text.len());
        let (mut value, charge) =
            try_string(text, text.len(), BudgetAllocator(ledger.clone())).unwrap();
        assert_eq!(value, text);
        assert_eq!(ledger.current(), value.capacity());
        assert_eq!(charge.bytes, value.capacity());
        drop(charge);
        assert_eq!(ledger.current(), 0);
        value.push_str(" caller-owned");
        assert!(value.starts_with(text));
        assert_eq!(ledger.current(), 0);
    }
}

#[test]
fn host_metadata_and_underlying_refusals_roll_back_and_recover() {
    let text = "compiler worker compilation";
    for cause in 0..3 {
        let host = Ledger::try_host(65536).unwrap();
        let ledger = Ledger::try_child(65536, host.clone()).unwrap();
        let baseline = host.accounted();
        match cause {
            0 => ledger.set_limit(text.len() - 1),
            1 => host.set_limit(baseline + text.len() - 1),
            _ => ledger.fail_after(0),
        }
        for _ in 0..2 {
            assert!(try_string(text, 32, BudgetAllocator(ledger.clone())).is_err());
            assert_eq!(ledger.current(), 0);
            assert_eq!(host.accounted(), baseline);
        }
        ledger.set_limit(text.len());
        host.set_limit(baseline + text.len());
        ledger.fail_after(usize::MAX);
        let (value, charge) = try_string(text, 32, BudgetAllocator(ledger.clone())).unwrap();
        assert_eq!(host.accounted(), baseline + value.capacity());
        drop(value);
        assert_eq!(ledger.current(), text.len());
        drop(charge);
        assert_eq!(host.accounted(), baseline);
    }
}

#[test]
fn empty_and_byte_cap_refusal_need_no_allocation_or_fault_reset() {
    let ledger = Ledger::new(0);
    ledger.fail_after(0);
    let (value, charge) = try_string("", 0, BudgetAllocator(ledger.clone())).unwrap();
    assert_eq!(value.capacity(), 0);
    drop(value);
    drop(charge);
    assert!(try_string("λ", 1, BudgetAllocator(ledger.clone())).is_err());
    assert!(try_string("x", 1, BudgetAllocator(ledger.clone())).is_err());
    assert_eq!(ledger.current(), 0);
    assert_eq!(ledger.peak(), 0);
    ledger.set_limit(1);
    assert!(try_string("x", 1, BudgetAllocator(ledger)).is_err());
}

#[test]
fn exported_public_error_outlives_lua_without_retaining_jit_storage() {
    let mut lua = crate::Lua::empty();
    let metadata = lua.enter(|ctx| ctx.jit().0.borrow().metadata.clone());
    let text = "compiler worker compilation";
    let baseline = lua.accounted_memory();
    lua.set_memory_limit(Some(baseline + text.len()));
    let (value, charge) = try_string(text, 32, metadata).unwrap();
    assert_eq!(lua.accounted_memory(), baseline + value.capacity());
    let error = crate::JitError::Compilation(value);
    drop(charge);
    assert_eq!(lua.accounted_memory(), baseline);
    drop(lua);
    let crate::JitError::Compilation(mut value) = error else {
        panic!("error class changed")
    };
    assert_eq!(value, text);
    value.push_str(" after Lua");
    assert!(value.ends_with(" after Lua"));
}

#[test]
fn construction_unwinding_reclaims_the_string_and_its_charge() {
    let ledger = Ledger::new(32);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let charge;
        let value;
        (value, charge) = try_string(
            "compiler worker target",
            32,
            BudgetAllocator(ledger.clone()),
        )
        .unwrap();
        assert_eq!(ledger.current(), value.capacity());
        std::hint::black_box((&value, &charge));
        panic!("construction unwind");
    }));
    assert!(result.is_err());
    assert_eq!(ledger.current(), 0);
}
