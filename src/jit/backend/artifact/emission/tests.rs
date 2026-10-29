use super::*;

const BUILD: [u8; 32] = [0x51; 32];

fn allocator() -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(32 << 20))
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

fn snapshot(text: &[u8]) -> Snapshot {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, text).unwrap();
        Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap()
    })
}

fn source_packet(source: &Snapshot) -> BudgetVec<u8, BudgetAllocator> {
    crate::jit::compiler_wire::encode(
        source,
        crate::jit::compiler_wire::Limits {
            bytes: 1 << 20,
            operations: 65536,
            constants: 65536,
        },
        allocator(),
    )
    .unwrap()
}

fn original(source: &Snapshot, route: Route, resume: bool, limits: work::Limits) -> Bundle {
    let metadata = allocator();
    let total = MappingCounter::new(metadata.0.clone());
    let (code, mut bundle) = capture(allocator(), || match route {
        Route::Ordinary => compile_in(
            source,
            total.clone(),
            4 << 20,
            metadata.clone(),
            limits,
            Failure::None,
        ),
        Route::Arrays => array::compile_pair(
            source,
            total.clone(),
            4 << 20,
            metadata.clone(),
            limits,
            Failure::None,
        ),
        Route::Readonly => {
            read::binding::integer_resume::with_enabled(resume, || {
                readonly_lookup::compile_pair(
                    source,
                    total.clone(),
                    4 << 20,
                    metadata.clone(),
                    limits,
                    Failure::None,
                )
            })
            .0
        }
    })
    .unwrap();
    let code = code.unwrap();
    let mut routes = 1;
    if code.array_kernels.is_some() {
        routes |= 2;
    }
    if let Some(child) = &code.readonly_lookup {
        routes |= 4;
        if child.integer_resume.is_some() {
            routes |= 8;
        }
    }
    bundle.retain_routes(routes);
    drop(code);
    assert_eq!(metadata.0.current(), 0);
    assert_eq!(total.load(Ordering::Relaxed), 0);
    bundle
}

fn compare(source: &Snapshot, route: Route, resume: bool, work: work::Limits) -> u64 {
    let original = original(source, route, resume, work);
    let metadata = allocator();
    let baseline = source.operations.allocator().0.current();
    let emitted = emit(source, route, resume, work, metadata.clone(), allocator()).unwrap();
    assert_eq!(metadata.0.current(), 0);
    assert_eq!(source.operations.allocator().0.current(), baseline);
    let source = source_packet(source);
    let environment = environment().unwrap();
    let binding = Binding {
        request: 7,
        build: &BUILD,
        source: &source,
        environment: &environment,
    };
    assert_eq!(
        &*encode(&emitted, binding, limits(), allocator()).unwrap(),
        &*encode(&original, binding, limits(), allocator()).unwrap(),
    );
    emitted.routes()
}

#[test]
fn emission_matches_installed_compiler_bytes_for_production_routes() {
    mod workloads {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/jit_support/workloads.rs"
        ));
    }
    let work = work::Limits::from(&crate::JitConfig::default());
    let mut routes = 0;
    for workload in workloads::WORKLOADS {
        let source = snapshot(workload.source);
        routes |= compare(&source, Route::Ordinary, false, work);
        routes |= compare(&source, Route::Arrays, false, work);
        if read::Plan::recognize(&source).is_some() {
            for resume in [false, true] {
                routes |= compare(&source, Route::Readonly, resume, work);
            }
        }
    }
    assert_eq!(routes, 15);
    let source = snapshot(
        b"local t={} for i=1,20 do t[i]=i end local u={} for i=1,20 do u[i]=t[i]+1 end return u",
    );
    assert_eq!(compare(&source, Route::Arrays, false, work), 3);
}

#[test]
fn optional_emission_work_refusal_preserves_ordinary_artifact() {
    for (text, route) in [
        (
            &b"local sum=0 for i=1,80 do sum=sum+host_increment(i) end return sum"[..],
            Route::Readonly,
        ),
        (
            &b"local t={} for i=1,20 do t[i]=i end return t"[..],
            Route::Arrays,
        ),
    ] {
        let source = snapshot(text);
        let mut limits = work::Limits::from(&crate::JitConfig::default());
        let bound = work::Expansion::admit(&source, limits).unwrap();
        limits.instructions = bound.instructions;
        limits.blocks = bound.blocks;
        assert_eq!(compare(&source, route, true, limits), 1);
    }
}

#[test]
fn emission_output_refusal_is_fallible_and_does_not_reset_denial() {
    let source = snapshot(b"local sum=0 for i=1,80 do sum=sum+host_increment(i) end return sum");
    let workspace = source.operations.allocator().0.clone();
    let baseline = workspace.current();
    let mut successes = 0;
    let mut failures = 0;
    for allowance in 0..96 {
        let output = allocator();
        output.0.fail_after(allowance);
        let metadata = allocator();
        match emit(
            &source,
            Route::Readonly,
            true,
            work::Limits::from(&crate::JitConfig::default()),
            metadata.clone(),
            output.clone(),
        ) {
            Ok(bundle) => {
                successes += 1;
                drop(bundle);
            }
            Err(CompileError::ResourceLimit(_)) => {
                failures += 1;
                assert!(BudgetVec::<u8, _>::new_in(output.clone())
                    .try_reserve_exact(1)
                    .is_err());
            }
            Err(error) => panic!("unexpected emission refusal: {error:?}"),
        }
        assert_eq!(output.0.current(), 0);
        assert_eq!(metadata.0.current(), 0);
        assert_eq!(workspace.current(), baseline);
    }
    assert!(successes > 0 && failures > 0);
}

#[test]
fn emission_memory_guard_restores_nested_and_panicking_scopes() {
    assert_native_memory_allowed();
    without_native_memory(|| {
        without_native_memory(|| assert!(EMITTING.get()));
        assert!(EMITTING.get());
        assert!(std::panic::catch_unwind(assert_native_memory_allowed).is_err());
        assert!(EMITTING.get());
    });
    assert_native_memory_allowed();
    assert!(
        std::panic::catch_unwind(|| without_native_memory(assert_native_memory_allowed)).is_err()
    );
    assert_native_memory_allowed();
}
