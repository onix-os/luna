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
fn signature_refusal_preserves_peer_and_reservation_outlives_compiler_owners() {
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
    let retained = (
        total.load(Ordering::Relaxed),
        metadata.0.current(),
        total.requested(),
    );
    assert!(matches!(
        compile(&source, Failure::RefuseSignatures),
        Err(JitError::ResourceLimit("native signatures"))
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
