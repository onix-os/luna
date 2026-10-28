use super::super::{environment, environment_of, native_builder, JITModule};
use super::*;
use crate::jit::resources::Ledger;
use cranelift_module::Module;

fn allocator() -> BudgetAllocator {
    BudgetAllocator(Ledger::new(MAX_BYTES))
}

fn reference() -> JITModule {
    JITModule::new(native_builder(cranelift_native::builder()).unwrap())
}

#[test]
fn complete_native_schema_defaults_and_identity_match_the_actual_compiler() {
    assert!(REQUESTED.get().is_none());
    let detected = Features::native().unwrap();
    let module = reference();
    let flags = module.isa().isa_flags();
    assert_eq!(flags.len(), detected.names().len());
    for (index, (flag, &name)) in flags.iter().zip(detected.names()).enumerate() {
        assert_eq!(flag.name, name);
        assert_eq!(flag.as_bool(), Some(detected.value(index)));
    }
    let defaults =
        JITModule::new(native_builder(cranelift_native::builder_with_options(false)).unwrap());
    assert!(defaults
        .isa()
        .isa_flags()
        .iter()
        .all(|flag| flag.as_bool() == Some(false)));
    assert_eq!(
        &*environment().unwrap(),
        &*environment_of(&module, allocator()).unwrap()
    );
}

#[test]
fn all_feature_bits_and_mixed_scopes_preserve_exact_compiler_environment() {
    let native = Features::native().unwrap();
    let mask = (1u32 << native.names().len()) - 1;
    let mut random = 0x739a_81cdu32;
    let samples = (0..32)
        .map(|_| {
            random = random.wrapping_mul(1664525).wrapping_add(1013904223);
            random & mask
        })
        .collect::<Vec<_>>();
    for bits in [0, mask, native.bits]
        .into_iter()
        .chain((0..native.names().len()).map(|index| 1 << index))
        .chain(samples)
    {
        let features = Features { bits, ..native };
        let request = FeatureBytes(features.encode(false, allocator()).unwrap());
        with_target(&request, || {
            let module = reference();
            assert_eq!(
                &*environment_in(allocator()).unwrap(),
                &*environment_of(&module, allocator()).unwrap()
            );
            let flags = module.isa().isa_flags();
            for (index, flag) in flags.iter().enumerate() {
                assert_eq!(flag.as_bool(), Some(features.value(index)));
            }
            assert_eq!(&*target_features_in(allocator()).unwrap(), &*request);
        })
        .unwrap();
    }
    assert!(REQUESTED.get().is_none());
}

#[test]
fn bounded_identity_forbids_compiler_construction_and_reclaims_budget_refusals() {
    assert_compiler_allowed();
    assert!(
        std::panic::catch_unwind(|| identity(|| native_builder(Err("identity guard probe"))))
            .is_err()
    );
    assert_compiler_allowed();
    let denied = allocator();
    denied.0.fail_after(0);
    assert_eq!(
        target_features_in(denied.clone()).err(),
        Some(Error::Allocation)
    );
    assert_eq!(
        environment_in(denied.clone()).err(),
        Some(Error::Allocation)
    );
    assert_eq!(denied.0.current(), 0);
    assert!(BudgetVec::<u8, _>::new_in(denied)
        .try_reserve_exact(1)
        .is_err());
    assert_compiler_allowed();
    assert!(with_target("invalid", || ()).is_err());
    assert_compiler_allowed();
    assert!(REQUESTED.get().is_none());
}
