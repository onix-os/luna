use super::*;
use crate::jit::resources::Ledger;

fn allocator() -> BudgetAllocator {
    BudgetAllocator(Ledger::new(32 << 20))
}
fn limits() -> Limits {
    Limits {
        bytes: 2 << 20,
        modules: 16,
        declarations: 256,
        functions: 128,
        relocations: 8192,
    }
}
fn source(text: &[u8]) -> Snapshot {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, text).unwrap();
        Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap()
    })
}
fn assembly<'a>(
    source: &'a Snapshot,
    route: Route,
    total: MappingCounter,
    metadata: BudgetAllocator,
    limit: usize,
    failure: Failure,
) -> Assembly<'a> {
    Assembly {
        source,
        route,
        resume: true,
        total,
        limit,
        metadata,
        work: work::Limits::from(&crate::JitConfig::default()),
        failure,
    }
}

#[test]
fn real_code_and_optional_shared_owners_outlive_request_and_source() {
    for (text, route) in [
        (
            &b"local s=0 for i=1,20 do s=s+i end return s"[..],
            Route::Ordinary,
        ),
        (
            &b"local t={} for i=1,20 do t[i]=i end return t"[..],
            Route::Arrays,
        ),
        (
            &b"local sum=0 for i=1,80 do sum=sum+host_increment(i) end return sum"[..],
            Route::Readonly,
        ),
    ] {
        let source = source(text);
        let workspace = source.operations.allocator().0.clone();
        let baseline = workspace.current();
        let packet = crate::jit::compiler_process::request_packet(
            &source,
            route,
            true,
            work::Limits::from(&crate::JitConfig::default()),
        )
        .unwrap();
        let output = allocator();
        let total = MappingCounter::new(output.0.clone());
        let code = assemble(
            &packet.bytes,
            packet.binding(),
            limits(),
            assembly(
                &source,
                route,
                total.clone(),
                output.clone(),
                4 << 20,
                Failure::None,
            ),
        )
        .unwrap();
        assert_eq!(code.array_kernels.is_some(), route == Route::Arrays);
        assert_eq!(code.readonly_lookup.is_some(), route == Route::Readonly);
        if let Some(child) = &code.readonly_lookup {
            assert!(child.integer_resume.is_some());
        }
        drop(packet);
        assert_eq!(workspace.current(), baseline);
        drop(source);
        assert_eq!(workspace.current(), 0);
        let owner = code.into_shared(output.clone()).unwrap();
        let lease = owner.clone();
        drop(owner);
        assert!(total.load(Ordering::Relaxed) > 0);
        drop(lease);
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(total.requested(), 0);
        assert_eq!(output.0.current(), 0);
    }
}

#[test]
fn optional_mapping_and_owner_refusals_keep_ordinary_code() {
    for (text, route) in [
        (
            &b"local t={} for i=1,20 do t[i]=i end return t"[..],
            Route::Arrays,
        ),
        (
            &b"local sum=0 for i=1,80 do sum=sum+host_increment(i) end return sum"[..],
            Route::Readonly,
        ),
    ] {
        let source = source(text);
        let packet = crate::jit::compiler_process::request_packet(
            &source,
            route,
            true,
            work::Limits::from(&crate::JitConfig::default()),
        )
        .unwrap();
        let output = allocator();
        let total = MappingCounter::new(output.0.clone());
        let complete = assemble(
            &packet.bytes,
            packet.binding(),
            limits(),
            assembly(
                &source,
                route,
                total.clone(),
                output.clone(),
                4 << 20,
                Failure::None,
            ),
        )
        .unwrap();
        let ordinary_bytes = complete
            ._memory
            .allocations
            .iter()
            .map(|segment| segment.bytes)
            .sum();
        assert!(total.load(Ordering::Relaxed) > ordinary_bytes);
        drop(complete);
        let ordinary = assemble(
            &packet.bytes,
            packet.binding(),
            limits(),
            assembly(
                &source,
                route,
                total.clone(),
                output.clone(),
                ordinary_bytes,
                Failure::None,
            ),
        )
        .unwrap();
        assert!(ordinary.array_kernels.is_none() && ordinary.readonly_lookup.is_none());
        assert_eq!(total.load(Ordering::Relaxed), ordinary_bytes);
        drop(ordinary);
        if route == Route::Readonly {
            for failure in [
                Failure::RefuseReadonlyOwnerStorage,
                Failure::RefuseReadonlyOwnerAllocation,
            ] {
                let code = assemble(
                    &packet.bytes,
                    packet.binding(),
                    limits(),
                    assembly(
                        &source,
                        route,
                        total.clone(),
                        output.clone(),
                        4 << 20,
                        failure,
                    ),
                )
                .unwrap();
                assert!(code.readonly_lookup.is_none());
                drop(code);
            }
        }
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(output.0.current(), 0);
    }
}

#[test]
fn metadata_refusal_at_every_stage_reclaims_images_without_resetting_failures() {
    let source = source(b"local sum=0 for i=1,80 do sum=sum+host_increment(i) end return sum");
    let packet = crate::jit::compiler_process::request_packet(
        &source,
        Route::Readonly,
        true,
        work::Limits::from(&crate::JitConfig::default()),
    )
    .unwrap();
    let mut completed = 0;
    let mut refused = 0;
    let mut fallback = 0;
    for allowance in 0..128 {
        let output = allocator();
        output.0.fail_after(allowance);
        let total = MappingCounter::new(output.0.clone());
        match assemble(
            &packet.bytes,
            packet.binding(),
            limits(),
            assembly(
                &source,
                Route::Readonly,
                total.clone(),
                output.clone(),
                4 << 20,
                Failure::None,
            ),
        ) {
            Ok(code) => {
                if code.readonly_lookup.is_none() {
                    fallback += 1;
                }
                completed += 1;
                drop(code);
            }
            Err(InstallError::Native(CompileError::ResourceLimit(_))) => {
                refused += 1;
            }
            Err(error) => panic!("unexpected refusal {error:?}"),
        }
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(total.requested(), 0);
        assert_eq!(output.0.current(), 0);
        if output.0.refusals() > 0 {
            let mut probe = BudgetVec::<u8, _>::new_in(output);
            assert!(probe.try_reserve_exact(1).is_err());
        }
    }
    assert!(completed > 0 && refused > 0 && fallback > 0);
}

#[test]
fn source_or_route_mismatch_refuses_before_native_metadata_and_mapping() {
    let source = source(b"local sum=0 for i=1,80 do sum=sum+host_increment(i) end return sum");
    let packet = crate::jit::compiler_process::request_packet(
        &source,
        Route::Readonly,
        true,
        work::Limits::from(&crate::JitConfig::default()),
    )
    .unwrap();
    let different = self::source(b"return 17");
    for (snapshot, route) in [(&different, Route::Readonly), (&source, Route::Ordinary)] {
        let output = allocator();
        output.0.fail_after(0);
        let total = MappingCounter::new(output.0.clone());
        assert!(matches!(
            assemble(
                &packet.bytes,
                packet.binding(),
                limits(),
                assembly(
                    snapshot,
                    route,
                    total.clone(),
                    output.clone(),
                    4 << 20,
                    Failure::None
                )
            ),
            Err(InstallError::Packet(_))
        ));
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(output.0.current(), 0);
    }
}
