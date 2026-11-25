use super::*;
use crate::jit::resources::Ledger;
use std::panic::{catch_unwind, AssertUnwindSafe};

#[test]
fn exact_utf8_charge_outlives_moves_and_nested_results() {
    for text in ["native failure", "λ\0é🤖"] {
        let ledger = Ledger::new(text.len());
        let message = Message::format(
            format_args!("{text}"),
            text.len(),
            BudgetAllocator(ledger.clone()),
        )
        .unwrap();
        assert_eq!(message.as_str(), text);
        assert_eq!(ledger.current(), text.len());
        let result: Result<(), Message> = Err(message);
        assert_eq!(ledger.current(), text.len());
        let moved = result.unwrap_err();
        assert_eq!(&moved, text);
        drop(moved);
        assert_eq!(ledger.current(), 0);
        assert_eq!(ledger.peak(), text.len());
    }
}

#[test]
fn byte_limits_and_sticky_write_errors_refuse_before_allocation() {
    struct IgnoresErrors;
    impl fmt::Display for IgnoresErrors {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let _ = f.write_str("λ");
            let _ = f.write_str("x");
            Ok(())
        }
    }
    let ledger = Ledger::new(0);
    ledger.fail_after(0);
    for result in [
        Message::format(format_args!("λ"), 1, BudgetAllocator(ledger.clone())),
        Message::format(
            format_args!("{IgnoresErrors}"),
            1,
            BudgetAllocator(ledger.clone()),
        ),
        Message::format(
            format_args!(""),
            MAX_BYTES + 1,
            BudgetAllocator(ledger.clone()),
        ),
        Message::format(
            format_args!("{:width$}", "x", width = MAX_BYTES + 1),
            MAX_BYTES,
            BudgetAllocator(ledger.clone()),
        ),
    ] {
        assert_eq!(result.unwrap_err(), Error::Limit);
        assert_eq!((ledger.current(), ledger.peak()), (0, 0));
    }
    ledger.set_limit(1);
    assert_eq!(
        Message::format(format_args!("x"), 1, BudgetAllocator(ledger)).unwrap_err(),
        Error::Allocation
    );
}

#[test]
fn formatter_refusal_and_panic_leave_no_owned_allocations() {
    struct Fails(bool);
    impl fmt::Display for Fails {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("prefix λ")?;
            if self.0 {
                std::panic::panic_any(41u64);
            }
            Err(fmt::Error)
        }
    }
    let ledger = Ledger::new(64);
    assert_eq!(
        Message::format(
            format_args!("{}", Fails(false)),
            64,
            BudgetAllocator(ledger.clone())
        )
        .unwrap_err(),
        Error::Formatting
    );
    let panic = catch_unwind(AssertUnwindSafe(|| {
        Message::format(
            format_args!("{}", Fails(true)),
            64,
            BudgetAllocator(ledger.clone()),
        )
    }))
    .unwrap_err();
    assert_eq!(*panic.downcast::<u64>().unwrap(), 41);
    assert_eq!((ledger.current(), ledger.peak()), (0, 0));
}

#[test]
fn child_parent_and_underlying_refusals_roll_back_and_recover() {
    let text = "compiler failure λ";
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
            assert_eq!(
                Message::format(format_args!("{text}"), 64, BudgetAllocator(ledger.clone()))
                    .unwrap_err(),
                Error::Allocation
            );
            assert_eq!(ledger.current(), 0);
            assert_eq!(host.accounted(), baseline);
        }
        ledger.set_limit(text.len());
        host.set_limit(baseline + text.len());
        ledger.fail_after(usize::MAX);
        let message =
            Message::format(format_args!("{text}"), 64, BudgetAllocator(ledger.clone())).unwrap();
        assert_eq!(host.accounted(), baseline + text.len());
        drop(message);
        assert_eq!(ledger.current(), 0);
        assert_eq!(host.accounted(), baseline);
    }
}

#[test]
fn export_moves_owned_text_without_allocating_and_releases_charge() {
    let ledger = Ledger::new(64);
    let message = Message::format(
        format_args!("error {}", 42),
        64,
        BudgetAllocator(ledger.clone()),
    )
    .unwrap();
    let pointer = message.as_str().as_ptr();
    assert_eq!(ledger.current(), "error 42".len());
    ledger.fail_after(0);
    let mut value = message.into_host(BudgetAllocator(ledger.clone())).unwrap();
    assert_eq!(value.as_ptr(), pointer);
    assert_eq!(ledger.current(), 0);
    value.push_str(" host-owned");
    assert_eq!(value, "error 42 host-owned");
    assert_eq!(ledger.current(), 0);
    assert_eq!(
        Message::format(format_args!("x"), 1, BudgetAllocator(ledger)).unwrap_err(),
        Error::Allocation
    );

    let ledger = Ledger::new(MAX_BYTES + 1);
    let (value, charge) =
        global_box::try_string("adopted", MAX_BYTES, BudgetAllocator(ledger.clone())).unwrap();
    let message = Message::from_owned(value, charge).unwrap();
    assert_eq!(ledger.current(), "adopted".len());
    drop(message);
    assert_eq!(ledger.current(), 0);
    let oversized = "x".repeat(MAX_BYTES + 1);
    let (mut value, charge) =
        global_box::try_string(&oversized, MAX_BYTES + 1, BudgetAllocator(ledger.clone())).unwrap();
    value.truncate(1);
    assert_eq!(
        Message::from_owned(value, charge).unwrap_err(),
        Error::Limit
    );
    assert_eq!(ledger.current(), 0);
}

#[test]
fn static_text_allocates_only_at_export_and_checks_the_byte_cap() {
    let ledger = Ledger::new(0);
    ledger.fail_after(0);
    let message = Message::from("static failure");
    assert_eq!(message.as_str(), "static failure");
    assert_eq!((ledger.current(), ledger.peak()), (0, 0));
    assert_eq!(
        message
            .into_host(BudgetAllocator(ledger.clone()))
            .unwrap_err(),
        Error::Allocation
    );
    static LONG: [u8; MAX_BYTES + 1] = [b'x'; MAX_BYTES + 1];
    let text = std::str::from_utf8(&LONG).unwrap();
    assert_eq!(
        Message::from(text)
            .into_host(BudgetAllocator(ledger.clone()))
            .unwrap_err(),
        Error::Limit
    );
    ledger.set_limit(64);
    ledger.fail_after(usize::MAX);
    let value = Message::from("static failure")
        .into_host(BudgetAllocator(ledger.clone()))
        .unwrap();
    assert_eq!(value, "static failure");
    assert_eq!(ledger.current(), 0);
    assert_eq!(ledger.peak(), value.len());
}

#[test]
fn exported_public_error_outlives_lua_without_retaining_jit_storage() {
    let mut lua = crate::Lua::empty();
    let metadata = lua.enter(|ctx| ctx.jit().0.borrow().metadata.clone());
    let text = "compiler failure λ";
    let baseline = lua.accounted_memory();
    lua.set_memory_limit(Some(baseline + text.len()));
    let message = Message::format(format_args!("{text}"), 64, metadata.clone()).unwrap();
    assert_eq!(lua.accounted_memory(), baseline + text.len());
    let error = crate::JitError::Compilation(message.into_host(metadata).unwrap());
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
fn empty_messages_need_no_allocation_or_failure_reset() {
    let ledger = Ledger::new(0);
    ledger.fail_after(0);
    let message = Message::format(format_args!(""), 0, BudgetAllocator(ledger.clone())).unwrap();
    assert_eq!(message.as_str(), "");
    assert_eq!(
        message
            .into_host(BudgetAllocator(ledger.clone()))
            .unwrap()
            .capacity(),
        0
    );
    assert_eq!((ledger.current(), ledger.peak()), (0, 0));
    ledger.set_limit(1);
    assert_eq!(
        Message::format(format_args!("x"), 1, BudgetAllocator(ledger)).unwrap_err(),
        Error::Allocation
    );
}

#[test]
fn charged_messages_retain_the_child_ledger_until_internal_destruction() {
    let host = Ledger::try_host(65536).unwrap();
    let baseline = host.accounted();
    let message = {
        let child = Ledger::try_child(65536, host.clone()).unwrap();
        let message = Message::format(
            format_args!("owned child"),
            64,
            BudgetAllocator(child.clone()),
        )
        .unwrap();
        let retained = host.accounted();
        drop(child);
        assert_eq!(host.accounted(), retained);
        assert!(retained > baseline + "owned child".len());
        message
    };
    assert_eq!(message.as_str(), "owned child");
    drop(message);
    assert_eq!(host.accounted(), baseline);
}

#[test]
fn unwinding_keeps_the_charge_until_the_owned_message_is_destroyed() {
    struct Observe {
        ledger: crate::jit::resources::LedgerRef,
        bytes: usize,
    }
    impl Drop for Observe {
        fn drop(&mut self) {
            assert_eq!(self.ledger.current(), self.bytes);
        }
    }
    let ledger = Ledger::new(64);
    let panic = catch_unwind(AssertUnwindSafe(|| {
        let message = Message::format(
            format_args!("owned failure"),
            64,
            BudgetAllocator(ledger.clone()),
        )
        .unwrap();
        let observe = Observe {
            ledger: ledger.clone(),
            bytes: message.as_str().len(),
        };
        std::hint::black_box((&message, &observe));
        std::panic::panic_any(42u64);
    }))
    .unwrap_err();
    assert_eq!(*panic.downcast::<u64>().unwrap(), 42);
    assert_eq!(ledger.current(), 0);
}
