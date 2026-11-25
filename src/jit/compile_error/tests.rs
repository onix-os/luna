use super::*;
use crate::jit::resources::Ledger;

#[test]
fn formatted_errors_retain_charge_through_internal_cleanup() {
    struct Cleanup(crate::jit::resources::LedgerRef);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            assert_eq!(self.0.current(), "failure 42".len());
        }
    }
    let ledger = Ledger::new(64);
    let result: Result<(), Error> = (|| {
        let _cleanup = Cleanup(ledger.clone());
        Err(Error::compilation(
            format_args!("failure {}", 42),
            BudgetAllocator(ledger.clone()),
        ))
    })();
    assert_eq!(ledger.current(), "failure 42".len());
    let error = result.unwrap_err();
    assert_eq!(error.class(), Class::Compilation);
    let Error::Compilation(ref message) = error else {
        panic!("private error class")
    };
    let pointer = message.as_str().as_ptr();
    ledger.fail_after(0);
    let JitError::Compilation(text) = error.into_public(BudgetAllocator(ledger.clone())) else {
        panic!("public error class")
    };
    assert_eq!(text, "failure 42");
    assert_eq!(text.as_ptr(), pointer);
    assert_eq!(ledger.current(), 0);
}

#[test]
fn static_errors_allocate_only_at_public_export() {
    let ledger = Ledger::new(64);
    let error = Error::Compilation("static failure".into());
    assert_eq!(error.class(), Class::Compilation);
    assert_eq!((ledger.current(), ledger.peak()), (0, 0));
    let JitError::Compilation(text) = error.into_public(BudgetAllocator(ledger.clone())) else {
        panic!("error class")
    };
    assert_eq!(text, "static failure");
    assert_eq!(ledger.current(), 0);
    assert_eq!(ledger.peak(), text.len());
}

#[test]
fn every_error_class_keeps_its_category_and_text_at_publication() {
    let ledger = Ledger::new(64);
    for (error, class) in [
        (
            Error::InvalidConfiguration("host configuration"),
            Class::Configuration,
        ),
        (Error::Unavailable("host unavailable"), Class::Unavailable),
        (Error::ResourceLimit("host resource"), Class::Resource),
        (
            Error::Compilation("host compiler".into()),
            Class::Compilation,
        ),
    ] {
        let expected = error.to_string();
        assert_eq!(error.class(), class);
        assert_eq!(
            error
                .into_public(BudgetAllocator(ledger.clone()))
                .to_string(),
            expected
        );
        assert_eq!(ledger.current(), 0);
    }
}

#[test]
fn size_formatter_allocation_and_export_failures_are_unallocated_classes() {
    struct Fails;
    impl fmt::Display for Fails {
        fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
            Err(fmt::Error)
        }
    }
    let ledger = Ledger::new(0);
    ledger.fail_after(0);
    let cap = Error::compilation(
        format_args!("{:width$}", "x", width = diagnostic::MAX_BYTES + 1),
        BudgetAllocator(ledger.clone()),
    );
    assert!(matches!(
        cap,
        Error::ResourceLimit("native diagnostic bytes")
    ));
    let format = Error::compilation(format_args!("{Fails}"), BudgetAllocator(ledger.clone()));
    assert!(matches!(
        format,
        Error::Unavailable("native diagnostic formatting")
    ));
    let allocation = Error::compilation(format_args!("x"), BudgetAllocator(ledger.clone()));
    assert!(matches!(
        allocation,
        Error::ResourceLimit("native diagnostic allocation")
    ));
    let export =
        Error::Compilation("static failure".into()).into_public(BudgetAllocator(ledger.clone()));
    assert!(matches!(
        export,
        JitError::ResourceLimit("native diagnostic allocation")
    ));
    assert_eq!((ledger.current(), ledger.peak()), (0, 0));
}
