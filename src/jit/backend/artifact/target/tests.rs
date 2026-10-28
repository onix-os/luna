use super::*;
use crate::jit::resources::Ledger;
use std::fmt::Write;

fn allocator() -> BudgetAllocator {
    BudgetAllocator(Ledger::new(32768))
}
fn set(architecture: Architecture, bits: u32) -> Features {
    Features { architecture, bits }
}
fn text(features: Features) -> FeatureBytes {
    FeatureBytes(features.encode(false, allocator()).unwrap())
}

#[test]
fn complete_feature_schemas_roundtrip_every_bit_and_preserve_order() {
    for architecture in [Architecture::X64, Architecture::A64] {
        let schema = set(architecture, 0);
        let count = schema.names().len();
        assert!(count > 0 && count <= 32);
        for (index, &name) in schema.names().iter().enumerate() {
            assert!(!schema.names()[..index].contains(&name));
        }
        for bits in [0, (1u32 << count) - 1]
            .into_iter()
            .chain((0..count).map(|index| 1 << index))
        {
            let expected = set(architecture, bits);
            let encoded = text(expected);
            assert_eq!(Features::parse(architecture, &encoded), Ok(expected));
            assert_eq!(encoded.split(';').count(), count);
            for (index, field) in encoded.split(';').enumerate() {
                assert_eq!(
                    field.split_once('=').unwrap(),
                    (
                        schema.names()[index],
                        if expected.value(index) {
                            "true"
                        } else {
                            "false"
                        }
                    )
                );
            }
        }
    }
}

#[test]
fn truncated_mutated_duplicate_and_noncanonical_features_are_total() {
    for architecture in [Architecture::X64, Architecture::A64] {
        let original = text(set(architecture, 0));
        for length in 0..original.len() {
            assert!(Features::parse(architecture, &original[..length]).is_err());
        }
        for index in 0..original.len() {
            for replacement in [0, b';', b'=', b'x', b't', b'f'] {
                let mut changed = original.as_bytes().to_vec();
                changed[index] = replacement;
                let changed = std::str::from_utf8(&changed).unwrap();
                if let Ok(parsed) = Features::parse(architecture, changed) {
                    assert_eq!(&*text(parsed), changed);
                }
            }
        }
        for invalid in [
            format!("{original};"),
            format!("{original};{}=false", original.split_once('=').unwrap().0),
            original.replacen("false", "0", 1),
            original.replacen("false", "False", 1),
            original.replacen('=', " =", 1),
            "x".repeat(MAX_BYTES + 1),
        ] {
            assert!(Features::parse(architecture, &invalid).is_err());
        }
    }
}

#[test]
fn each_identity_buffer_is_one_fallible_accounted_allocation() {
    for architecture in [Architecture::X64, Architecture::A64] {
        for environment in [false, true] {
            let features = set(architecture, 1);
            let bytes = features.encode(environment, allocator()).unwrap();
            for allowance in [0, 1] {
                let output = allocator();
                output.0.fail_after(allowance);
                match features.encode(environment, output.clone()) {
                    Ok(encoded) => {
                        assert_eq!(allowance, 1);
                        assert_eq!(&*encoded, &*bytes);
                        assert_eq!(output.0.current(), encoded.capacity());
                        assert!(BudgetVec::<u8, _>::new_in(output.clone())
                            .try_reserve_exact(1)
                            .is_err());
                        drop(encoded);
                    }
                    Err(error) => {
                        assert_eq!(allowance, 0);
                        assert_eq!(error, Error::Allocation);
                    }
                }
                assert_eq!(output.0.current(), 0);
            }
            let output = BudgetAllocator(Ledger::new(bytes.len() - 1));
            assert_eq!(
                features.encode(environment, output.clone()).err(),
                Some(Error::Allocation)
            );
            assert_eq!(output.0.current(), 0);
        }
    }
}

#[test]
fn requested_feature_scopes_restore_nested_and_panicking_callers_without_owners() {
    let original = REQUESTED.get();
    assert!(!std::mem::needs_drop::<Features>());
    for architecture in [Architecture::X64, Architecture::A64] {
        let outer = set(architecture, 1);
        let inner = set(architecture, 2);
        with_features(outer, || {
            assert_eq!(Features::current(), Ok(outer));
            with_features(inner, || assert_eq!(Features::current(), Ok(inner)));
            assert_eq!(Features::current(), Ok(outer));
            assert!(
                std::panic::catch_unwind(|| with_features(inner, || panic!("target scope")))
                    .is_err()
            );
            assert_eq!(Features::current(), Ok(outer));
        });
        assert_eq!(REQUESTED.get(), original);
    }
}

#[test]
fn unknown_detector_operations_decline_without_error_strings_or_unwrap_panic() {
    for architecture in [Architecture::X64, Architecture::A64] {
        let mut collector = Collector {
            features: set(architecture, 0),
            invalid: false,
        };
        let name = collector.features.names()[0];
        collector.enable(name).unwrap();
        assert!(collector.features.value(0));
        collector.set(name, "false").unwrap();
        assert!(!collector.features.value(0));
        assert!(collector.finish().is_ok());
        for (key, value) in [
            ("future_native_flag", "true"),
            (name, "1"),
            (name, "true\0"),
        ] {
            let mut collector = Collector {
                features: set(architecture, 0),
                invalid: false,
            };
            collector.set(key, value).unwrap();
            assert_eq!(collector.finish(), Err(Error::Identity));
        }
    }
}

#[test]
fn two_pass_writers_enforce_exact_lengths_and_never_grow_during_output() {
    let mut count = Count(MAX_BYTES);
    assert!(count.write_str("x").is_err());
    let mut overflow = Count(usize::MAX);
    assert!(overflow.write_str("x").is_err());
    let output = allocator();
    let mut bytes = BudgetVec::new_in(output.clone());
    bytes.try_reserve_exact(4).unwrap();
    let mut writer = Output { bytes, length: 4 };
    writer.write_str("abcd").unwrap();
    assert!(writer.write_str("e").is_err());
    assert_eq!(&*writer.bytes, b"abcd");
    drop(writer);
    assert_eq!(output.0.current(), 0);
    assert_compiler_allowed();
    identity(|| {
        identity(|| assert!(IDENTITY.get()));
        assert!(IDENTITY.get());
        assert!(std::panic::catch_unwind(|| identity(assert_compiler_allowed)).is_err());
        assert!(IDENTITY.get());
    });
    assert_compiler_allowed();
    assert!(std::panic::catch_unwind(|| identity(assert_compiler_allowed)).is_err());
    assert_compiler_allowed();
}

#[test]
fn feature_string_and_os_adapters_borrow_the_owned_canonical_buffer() {
    use std::os::unix::ffi::OsStrExt;
    let output = allocator();
    let features = FeatureBytes(
        set(Architecture::native(), 0)
            .encode(false, output.clone())
            .unwrap(),
    );
    let os: &std::ffi::OsStr = features.as_ref();
    assert_eq!(os.as_bytes(), features.as_bytes());
    assert_eq!(os.as_bytes().as_ptr(), features.as_bytes().as_ptr());
    let mut count = Count(0);
    write!(&mut count, "{features}").unwrap();
    assert_eq!(count.0, features.len());
    drop(features);
    assert_eq!(output.0.current(), 0);
}
