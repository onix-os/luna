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
