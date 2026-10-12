use crate::{Callback, CallbackReturn, JitConfig, JitMode, Lua, Value};

#[test]
fn builtin_identity_is_instance_based_and_survives_registry_reset() {
    let mut lua = Lua::core();
    let original = lua.enter(|ctx| {
        let callback: Callback = ctx.get_global("setmetatable").unwrap();
        let replacement = Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return));
        assert!(ctx.jit_registry().borrow().is_set_metatable(ctx, callback));
        assert!(!ctx
            .jit_registry()
            .borrow()
            .is_set_metatable(ctx, replacement));
        ctx.set_global("setmetatable", replacement);
        ctx.stash(callback)
    });
    for mode in [JitMode::Auto, JitMode::Off, JitMode::Auto] {
        lua.set_jit_config(JitConfig {
            mode,
            ..Default::default()
        })
        .unwrap();
        lua.clear_jit_cache();
        lua.gc_collect();
        lua.enter(|ctx| {
            let original = ctx.fetch(&original);
            let current = ctx.get_global("setmetatable").unwrap();
            assert!(ctx.jit_registry().borrow().is_set_metatable(ctx, original));
            assert!(!ctx.jit_registry().borrow().is_set_metatable(ctx, current));
            ctx.jit_registry().borrow_mut(&ctx).reset(ctx);
            assert!(ctx.jit_registry().borrow().is_set_metatable(ctx, original));
        });
    }
}

#[test]
fn intrinsic_identity_does_not_keep_callback_alive_or_match_new_allocations() {
    let mut lua = Lua::core();
    lua.set_gc_pacing(false);
    lua.enter(|ctx| {
        ctx.set_global("setmetatable", Value::Nil);
    });
    lua.gc_collect();
    lua.gc_collect();
    lua.enter(|ctx| {
        assert!(ctx
            .jit_registry()
            .borrow()
            .set_metatable
            .unwrap()
            .upgrade(&ctx)
            .is_none());
        for _ in 0..1024 {
            let callback = Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return));
            assert!(!ctx.jit_registry().borrow().is_set_metatable(ctx, callback));
        }
    });
}
