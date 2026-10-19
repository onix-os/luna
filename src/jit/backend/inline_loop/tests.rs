use super::*;
use crate::{
    jit::{
        owner::Shared,
        resources::{BudgetAllocator, Ledger},
    },
    Closure, Fuel, JitConfig, JitMode, Lua,
};

fn fixture(test: impl for<'gc> FnOnce(Context<'gc>, Closure<'gc>, Snapshot, Snapshot, Key)) {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        ..Default::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            b"local x=0 local function f(v) x=x+v end for i=1,20 do f(i) end return x",
        )
        .unwrap();
        let caller = closure.prototype();
        let callee = caller.prototypes[0];
        let allocator = ctx.jit().0.borrow().snapshots.clone();
        let caller_source = Snapshot::new_in(&caller, 4096, allocator.clone()).unwrap();
        let callee_source = Snapshot::new_in(&callee, 4096, allocator).unwrap();
        let registry = ctx.jit_registry().borrow();
        let key = Key {
            caller: registry.identity(ctx, caller).unwrap(),
            callee: registry.identity(ctx, callee).unwrap(),
            pc: caller_source
                .operations
                .iter()
                .position(|op| matches!(op, Operation::Call { .. }))
                .unwrap(),
        };
        drop(registry);
        test(ctx, closure, caller_source, callee_source, key);
    });
}

#[test]
fn charged_image_executes_after_snapshot_release_until_final_lease_drop() {
    fixture(|ctx, closure, caller, callee, key| {
        let (metadata, mappings, workspace) = {
            let manager = ctx.jit().0.borrow();
            (
                manager.metadata.clone(),
                manager.memory.clone(),
                manager.snapshots.0.clone(),
            )
        };
        let before = (
            mappings.load(Ordering::Relaxed),
            mappings.requested(),
            metadata.0.current(),
        );
        let code = Code::compile(
            key,
            &caller,
            &callee,
            mappings.clone(),
            1 << 20,
            metadata.clone(),
            Limits::from(&JitConfig::default()),
            Failure::None,
        )
        .unwrap();
        let owner = Shared::try_new(code, metadata.clone()).unwrap();
        let lease = owner.clone();
        drop(owner);
        drop(caller);
        drop(callee);
        assert_eq!(workspace.current(), 0);
        assert!(mappings.load(Ordering::Relaxed) > before.0);
        crate::thread::activation::with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
            ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
            for _ in 0..64 {
                if host.with_registers(|_, r| *r.pc) == key.pc {
                    break;
                }
                host.run(ctx, 1, 1, 0).result.unwrap();
            }
            ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
            let before = ctx.jit().stats().native_entries;
            let progress = lease.run(ctx, host, 16, 64, 0).unwrap();
            assert!(progress.calls > 1);
            assert_eq!(ctx.jit().stats().native_entries, before + 1);
        });
        drop(lease);
        assert_eq!(
            (
                mappings.load(Ordering::Relaxed),
                mappings.requested(),
                metadata.0.current()
            ),
            before
        );
        assert_eq!(workspace.current(), 0);
    });
}

#[test]
fn code_workspace_mapping_and_protection_refusals_release_all_charges() {
    for fault in 0..7 {
        fixture(|ctx, _, caller, callee, key| {
            let (metadata, mappings, workspace) = {
                let manager = ctx.jit().0.borrow();
                (
                    manager.metadata.clone(),
                    manager.memory.clone(),
                    manager.snapshots.0.clone(),
                )
            };
            let before = (
                mappings.load(Ordering::Relaxed),
                mappings.requested(),
                metadata.0.current(),
                workspace.current(),
            );
            let mut limits = Limits::from(&JitConfig::default());
            let mut failure = Failure::None;
            let mut limit = 1 << 20;
            match fault {
                0 => failure = Failure::Allocate,
                1 => failure = Failure::Protect,
                2 => limit = 0,
                3 => metadata.0.fail_after(0),
                4 => workspace.set_limit(workspace.current()),
                5 => limits.instructions = 0,
                6 => limits.relocations = 0,
                _ => unreachable!(),
            }
            assert!(Code::compile(
                key,
                &caller,
                &callee,
                mappings.clone(),
                limit,
                metadata.clone(),
                limits,
                failure
            )
            .is_err());
            assert_eq!(
                (
                    mappings.load(Ordering::Relaxed),
                    mappings.requested(),
                    metadata.0.current(),
                    workspace.current()
                ),
                before
            );
        });
    }
}

#[test]
fn retained_image_rejects_reset_generations_before_effects() {
    fixture(|ctx, closure, caller, callee, key| {
        let manager = ctx.jit().0.borrow();
        let code = Code::compile(
            key,
            &caller,
            &callee,
            manager.memory.clone(),
            1 << 20,
            manager.metadata.clone(),
            Limits::from(&manager.config),
            Failure::None,
        )
        .unwrap();
        drop(manager);
        let sources = (closure.prototype(), closure.prototype().prototypes[0]);
        crate::thread::activation::with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
            ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
            for _ in 0..64 {
                if host.with_registers(|_, r| *r.pc) == key.pc {
                    break;
                }
                host.run(ctx, 1, 1, 0).result.unwrap();
            }
            assert_eq!(host.with_registers(|_, r| *r.pc), key.pc);
            ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
            ctx.jit_registry().borrow_mut(&ctx).reset(ctx);
            for registered in [false, true] {
                if registered {
                    let mut registry = ctx.jit_registry().borrow_mut(&ctx);
                    registry.register(ctx, sources.0);
                    registry.register(ctx, sources.1);
                    assert_ne!(registry.identity(ctx, sources.0), Some(key.caller));
                    assert_ne!(registry.identity(ctx, sources.1), Some(key.callee));
                }
                let fuel = host.fuel().remaining();
                let entries = ctx.jit().stats().native_entries;
                assert!(code.run(ctx, host, 16, 64, 0).is_none());
                assert_eq!(host.fuel().remaining(), fuel);
                assert_eq!(host.with_registers(|_, r| *r.pc), key.pc);
                assert_eq!(ctx.jit().stats().native_entries, entries);
            }
        });
    });
}

#[test]
fn foreign_metadata_workspace_and_mapping_roots_refuse_before_allocation() {
    for fault in 0..3 {
        fixture(|ctx, closure, caller, callee, key| {
            let manager = ctx.jit().0.borrow();
            let foreign = Ledger::new(1 << 20);
            let metadata = if fault == 0 {
                BudgetAllocator(foreign.clone())
            } else {
                manager.metadata.clone()
            };
            let mappings = if fault == 1 {
                MappingCounter::new(foreign.clone())
            } else {
                manager.memory.clone()
            };
            let callee = if fault == 2 {
                Snapshot::new_in(
                    &closure.prototype().prototypes[0],
                    4096,
                    BudgetAllocator(foreign.clone()),
                )
                .unwrap()
            } else {
                callee
            };
            let before = (
                metadata.0.current(),
                mappings.requested(),
                foreign.current(),
            );
            assert!(matches!(
                Code::compile(
                    key,
                    &caller,
                    &callee,
                    mappings.clone(),
                    1 << 20,
                    metadata.clone(),
                    Limits::from(&manager.config),
                    Failure::None
                ),
                Err(JitError::Compilation(_))
            ));
            assert_eq!(
                (
                    metadata.0.current(),
                    mappings.requested(),
                    foreign.current()
                ),
                before
            );
        });
    }
}
