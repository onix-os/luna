use super::*;
use crate::{
    jit::{resources::Ledger, work::Limits},
    Closure, Executor, JitConfig, JitMode, Lua,
};

fn snapshot(source: &[u8]) -> Snapshot {
    Lua::empty().enter(|ctx| {
        let prototype = crate::FunctionPrototype::compile(ctx, "workspace", source).unwrap();
        Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
    })
}

fn result(code: &Code, source: &Snapshot) -> u64 {
    let mut slots = vec![Slot::from_value(crate::Value::Nil); code.registers];
    let mut pc = 0;
    let mut instructions = 0;
    for _ in 0..1000 {
        let exit = code.invoke(&mut slots, pc, 64);
        instructions += exit.instructions;
        pc = exit.pc as usize;
        if let Operation::Return { start, .. } = source.operations[pc] {
            assert!(instructions > 0);
            assert_eq!(slots[usize::from(start.0)].tag, abi::INTEGER);
            return slots[usize::from(start.0)].bits;
        }
        assert!(exit.instructions > 0);
    }
    panic!("native slice did not return");
}

#[test]
fn host_isa_refusal_is_typed_and_success_preserves_pinned_builder_settings() {
    for reason in ["x86 support requires SSE2", "unsupported architecture"] {
        assert!(
            matches!(native_builder(Err(reason)), Err(JitError::Unavailable(actual)) if actual == reason)
        );
    }
    let actual = JITModule::new(native_builder(cranelift_native::builder()).unwrap());
    let expected = JITModule::new(
        JITBuilder::with_flags(
            &[("opt_level", "speed"), ("enable_verifier", "true")],
            default_libcall_names(),
        )
        .unwrap(),
    );
    assert_eq!(actual.isa().triple(), expected.isa().triple());
    assert_eq!(
        actual.isa().default_call_conv(),
        expected.isa().default_call_conv()
    );
    assert_eq!(
        actual.isa().flags().to_string(),
        expected.isa().flags().to_string()
    );
    let flags = |module: &JITModule| {
        module
            .isa()
            .isa_flags()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };
    assert_eq!(flags(&actual), flags(&expected));
    assert_eq!(actual.isa().flags().opt_level(), settings::OptLevel::Speed);
    assert!(actual.isa().flags().enable_verifier());
    assert!(!actual.isa().flags().use_colocated_libcalls());
    assert_eq!(actual.isa().flags().is_pic(), cfg!(target_arch = "x86_64"));
}

#[test]
fn host_isa_refusal_preserves_public_execution_cached_peer_and_recovery() {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        hot_threshold: 1,
        ..JitConfig::default()
    })
    .unwrap();
    let peer = lua
        .try_enter(|ctx| Ok(ctx.stash(Closure::load(ctx, None, b"return 42")?)))
        .unwrap();
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    let retained = lua.jit_stats();
    let source = lua
        .try_enter(|ctx| {
            Ok(ctx.stash(Closure::load(
                ctx,
                None,
                b"local sum=0 for i=1,10000 do sum=sum+i end return sum",
            )?))
        })
        .unwrap();
    lua.enter(|ctx| ctx.jit().0.borrow_mut().memory_failure = Failure::NativeIsaUnavailable);
    assert!(matches!(
        lua.prepare_jit(),
        Err(JitError::Unavailable(
            "injected unsupported host instruction set"
        ))
    ));
    let executor = lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&source).into(), ())));
    assert_eq!(lua.execute::<i64>(&executor).unwrap(), 50_005_000);
    let refused = lua.jit_stats();
    assert_eq!(refused.compilation_failures, 2);
    assert_eq!(refused.native_instructions, 0);
    assert_eq!(refused.installed_regions, 1);
    assert_eq!(refused.cache_evictions, 0);
    assert_eq!(refused.snapshot_bytes, 0);
    assert_eq!(refused.code_bytes, retained.code_bytes);
    assert_eq!(refused.code_requested_bytes, retained.code_requested_bytes);
    let peer_executor =
        lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&peer).into(), ())));
    assert_eq!(lua.execute::<i64>(&peer_executor).unwrap(), 42);
    assert!(lua.jit_stats().native_instructions > 0);
    lua.enter(|ctx| ctx.jit().0.borrow_mut().memory_failure = Failure::None);
    lua.clear_jit_cache();
    assert_eq!(lua.prepare_jit().unwrap(), 2);
    let native_before = lua.jit_stats().native_instructions;
    let recovered = lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&source).into(), ())));
    assert_eq!(lua.execute::<i64>(&recovered).unwrap(), 50_005_000);
    assert!(lua.jit_stats().native_instructions > native_before);
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
}

#[test]
fn symbol_layout_counts_registration_declarations_and_lookup_temporary() {
    let names = helpers::SYMBOLS.map(|(_, name, _)| name);
    let scoped = super::super::scoped_helpers::SYMBOLS.map(|(_, name, _)| name);
    assert_eq!(scoped.len(), names.len());
    assert_eq!(
        symbol_storage_bytes(scoped.map(str::len)).unwrap(),
        3 * scoped.iter().map(|name| name.len()).sum::<usize>()
            + scoped.iter().map(|name| name.len()).max().unwrap()
    );
    assert_eq!(
        symbol_storage_bytes(names.map(str::len)).unwrap(),
        3 * names.iter().map(|name| name.len()).sum::<usize>()
            + names.iter().map(|name| name.len()).max().unwrap()
    );
    assert_eq!(symbol_storage_bytes([]).unwrap(), 0);
    assert_eq!(symbol_storage_bytes([0]).unwrap(), 0);
    assert_eq!(symbol_storage_bytes([1, 2, 3]).unwrap(), 21);
    for lengths in [[usize::MAX, 0], [isize::MAX as usize, 1]] {
        assert!(matches!(
            symbol_storage_bytes(lengths),
            Err(JitError::ResourceLimit("native symbol size"))
        ));
    }
    for name in names.into_iter().chain(["", "λx"]) {
        let owned = owned_symbol(name).unwrap();
        assert_eq!(owned, name);
        assert_eq!(owned.capacity(), name.len());
        assert_eq!(name.to_owned().capacity(), name.len());
    }
}

#[test]
fn source_helper_selection_preserves_all_semantic_kinds_and_empty_sets() {
    use crate::types::{ConstantIndex16, RegisterIndex, VarCount};
    let selected = |source: &Snapshot| {
        helpers::SYMBOLS
            .iter()
            .filter_map(|(kind, _, _)| helper_needed(*kind, source).then_some(*kind))
            .collect::<Vec<_>>()
    };
    let mut source = snapshot(b"return 42");
    assert!(selected(&source).is_empty());
    assert_eq!(selected(&snapshot(b"return 'key'")), [2]);
    assert!(!helper_needed(
        abi::HELPER_SET_LIST,
        &snapshot(b"return {...}")
    ));
    source.constants = super::super::resources::owned(&[
        Slot {
            tag: abi::INTEGER,
            bits: 42,
        },
        Slot {
            tag: abi::REFERENCE,
            bits: 0,
        },
    ]);
    source.registers = 4;
    source.upvalues = 1;
    for (index, operation) in super::helper_flow_tests::operations()
        .into_iter()
        .enumerate()
    {
        source.operations = super::super::resources::owned(&[
            operation,
            Operation::Return {
                start: RegisterIndex(0),
                count: VarCount::constant(0),
            },
        ]);
        source.verify().unwrap();
        assert_eq!(selected(&source), [index as u32 + 1]);
    }
    source.operations = super::super::resources::owned(&[
        Operation::LoadConstant {
            dest: RegisterIndex(0),
            constant: ConstantIndex16(0),
        },
        Operation::Return {
            start: RegisterIndex(0),
            count: VarCount::constant(1),
        },
    ]);
    source.verify().unwrap();
    assert!(selected(&source).is_empty());
}

#[test]
fn signature_layout_counts_initial_declaration_and_import_vectors() {
    let element = std::mem::size_of::<AbiParam>();
    for helpers in [0, 1, helpers::SYMBOLS.len()] {
        assert_eq!(
            signature_storage_bytes(5, 6, 1, helpers).unwrap(),
            (2 * 5 + (1 + 2 * helpers) * 7) * element
        );
    }
    assert_eq!(signature_storage_bytes(0, 0, 0, 0).unwrap(), 0);
    for counts in [
        (usize::MAX, 6, 1, 9),
        (5, usize::MAX, 1, 9),
        (5, 6, usize::MAX, 9),
        (5, 6, 1, usize::MAX),
    ] {
        assert!(matches!(
            signature_storage_bytes(counts.0, counts.1, counts.2, counts.3),
            Err(JitError::ResourceLimit("native signature size"))
        ));
    }
    let mut signature =
        cranelift_codegen::ir::Signature::new(cranelift_codegen::isa::CallConv::SystemV);
    fill_signature(&mut signature.params, [types::I64; 6]).unwrap();
    fill_signature(&mut signature.returns, [types::I32]).unwrap();
    let cloned = signature.clone();
    assert_eq!(
        (signature.params.capacity(), signature.returns.capacity()),
        (6, 1)
    );
    assert_eq!(
        (cloned.params.capacity(), cloned.returns.capacity()),
        (6, 1)
    );
    assert!(fill_signature(&mut signature.params, [types::I64]).is_err());
    assert_eq!(signature, cloned);
}

#[test]
fn declaration_refusals_preserve_peer_and_reservations_outlive_compiler_owners() {
    let source = snapshot(b"local sum=0 for i=1,100 do sum=sum+i end local copy=sum return copy");
    assert!(helper_needed(abi::HELPER_MOVE, &source));
    let snapshots = source.operations.allocator().0.clone();
    let baseline = snapshots.current();
    let total = MappingCounter::new(Ledger::new(usize::MAX));
    let metadata = BudgetAllocator(Ledger::new(2 * 1024 * 1024));
    let compile = |source, failure| {
        compile_in(
            source,
            total.clone(),
            8 * 1024 * 1024,
            metadata.clone(),
            Limits::from(&JitConfig::default()),
            failure,
        )
    };
    let peer_source = snapshot(b"return 42");
    let peer = compile(&peer_source, Failure::None).unwrap();
    let retained = (
        total.load(Ordering::Relaxed),
        metadata.0.current(),
        total.requested(),
    );
    assert!(matches!(
        compile(&source, Failure::NativeIsaUnavailable),
        Err(JitError::Unavailable(
            "injected unsupported host instruction set"
        ))
    ));
    assert_eq!(snapshots.current(), baseline);
    assert_eq!(
        (
            total.load(Ordering::Relaxed),
            metadata.0.current(),
            total.requested()
        ),
        retained
    );
    assert_eq!(result(&peer, &peer_source), 42);
    for (failure, expected) in [
        (Failure::RefuseSymbols, "native symbols"),
        (Failure::RefuseSignatures, "native signatures"),
    ] {
        assert!(matches!(
            compile(&source, failure),
            Err(JitError::ResourceLimit(reason)) if reason == expected
        ));
        snapshots.set_limit(2 * 1024 * 1024);
        assert_eq!(snapshots.current(), baseline);
        assert_eq!(
            (
                total.load(Ordering::Relaxed),
                metadata.0.current(),
                total.requested()
            ),
            retained
        );
        assert_eq!(result(&peer, &peer_source), 42);
    }
    for failure in [Failure::Allocate, Failure::Protect] {
        assert!(matches!(
            compile(&source, failure),
            Err(JitError::Unavailable(_))
        ));
        assert_eq!(snapshots.current(), baseline);
        assert_eq!(
            (
                total.load(Ordering::Relaxed),
                metadata.0.current(),
                total.requested()
            ),
            retained
        );
        assert_eq!(result(&peer, &peer_source), 42);
    }
    let code = compile(&source, Failure::RequireSignatures(baseline)).unwrap();
    assert_eq!(result(&code, &source), 5050);
    assert_eq!(snapshots.current(), baseline);
    drop(code);
    assert_eq!(
        (
            total.load(Ordering::Relaxed),
            metadata.0.current(),
            total.requested()
        ),
        retained
    );
    assert_eq!(result(&peer, &peer_source), 42);
    drop(peer);
    assert_eq!(
        (
            total.load(Ordering::Relaxed),
            metadata.0.current(),
            total.requested()
        ),
        (0, 0, 0)
    );
}

#[test]
fn verification_workspace_is_released_before_codegen_and_failures_preserve_peer() {
    let source = snapshot(b"local sum=0 for i=1,100 do sum=sum+i end return sum");
    let snapshots = source.operations.allocator().0.clone();
    let baseline = snapshots.current();
    let total = MappingCounter::new(Ledger::new(usize::MAX));
    let metadata = BudgetAllocator(Ledger::new(2 * 1024 * 1024));
    let compile = |source, failure| {
        compile_in(
            source,
            total.clone(),
            8 * 1024 * 1024,
            metadata.clone(),
            Limits::from(&JitConfig::default()),
            failure,
        )
    };
    let peer_source = snapshot(b"return 42");
    let peer = compile(&peer_source, Failure::None).unwrap();
    let retained = (total.load(Ordering::Relaxed), metadata.0.current());
    for failure in [Failure::Allocate, Failure::Protect] {
        let outcome = compile(&source, failure);
        assert!(matches!(outcome, Err(JitError::Unavailable(_))));
        assert_eq!(snapshots.current(), baseline);
        assert_eq!(
            (total.load(Ordering::Relaxed), metadata.0.current()),
            retained
        );
        assert_eq!(result(&peer, &peer_source), 42);
    }
    let code = compile(&source, Failure::RequireReleasedWorkspace(baseline)).unwrap();
    assert_eq!(result(&code, &source), 5050);
    assert_eq!(snapshots.current(), baseline);
    drop(code);
    assert_eq!(
        (total.load(Ordering::Relaxed), metadata.0.current()),
        retained
    );
    drop(peer);
    assert_eq!(
        (total.load(Ordering::Relaxed), metadata.0.current()),
        (0, 0)
    );
}

#[test]
fn source_snapshot_is_released_before_cache_owner_installation() {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        ..Default::default()
    })
    .unwrap();
    let executor = lua.enter(|ctx| {
        ctx.jit().0.borrow_mut().memory_failure = Failure::RequireReleasedSnapshot;
        let closure = Closure::load(ctx, None, b"local x=40 return x+2").unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    assert_eq!(lua.prepare_jit().unwrap(), 1);
    assert_eq!(lua.jit_stats().snapshot_bytes, 0);
    assert_eq!(lua.execute::<i64>(&executor).unwrap(), 42);
    assert!(lua.jit_stats().native_instructions > 0);
    drop(executor);
    lua.gc_collect();
    lua.gc_collect();
    let stats = lua.jit_stats();
    assert_eq!(stats.code_bytes, 0);
    assert_eq!(stats.snapshot_bytes, 0);
    assert_eq!(stats.accounted_jit_bytes, stats.bootstrap_bytes);
}
