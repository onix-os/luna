use super::*;

#[test]
fn region_releases_registration_before_gc_and_source_loading_callback() {
    for source in [
        &b"local n=0 local function f(v) n=n+v end f(3) cb() return n"[..],
        &b"local n=0 local function f(v) n=n+v end f(3) local a,b=7,3 local q=a%b cb() return n+q"
            [..],
    ] {
        let mut lua = Lua::empty();
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            ..Default::default()
        })
        .unwrap();
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let executor = lua.enter(|ctx| {
            let calls = calls.clone();
            let callback = crate::Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                assert!(ctx.jit_registry().try_borrow_mut(&ctx).is_ok());
                let loaded = Closure::load(ctx, None, b"return 42").unwrap();
                assert!(ctx
                    .jit_registry()
                    .borrow()
                    .identity(ctx, loaded.prototype())
                    .is_some());
                calls.set(calls.get() + 1);
                stack.clear();
                Ok(crate::CallbackReturn::Return)
            });
            ctx.globals().set(ctx, "cb", callback).unwrap();
            let closure = Closure::load(ctx, None, source).unwrap();
            let (region, start) = build_region(ctx, closure);
            let thread = crate::Thread::new(ctx);
            thread.start(ctx, closure.into(), ()).unwrap();
            crate::thread::activation::with_test_existing_thread(
                ctx,
                thread,
                &mut Fuel::with(10000),
                |host| {
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                    host.run(ctx, 1, start as u32, 4).result.unwrap();
                    ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                    let outcome = region.run(ctx, host, 64, 64).unwrap();
                    outcome.result.unwrap();
                    assert_eq!(outcome.pairs, 1);
                    assert!(!host.lua_ready());
                    assert!(ctx.jit_registry().try_borrow_mut(&ctx).is_ok());
                },
            );
            ctx.stash(crate::Executor::run(&ctx, thread).unwrap())
        });
        assert_eq!(calls.get(), 0);
        assert_eq!(lua.jit_stats().native_region_entries, 1);
        lua.gc_collect();
        lua.gc_collect();
        let expected = if source.ends_with(b"return n") { 3 } else { 4 };
        assert_eq!(lua.execute::<i64>(&executor).unwrap(), expected);
        assert_eq!(calls.get(), 1);
    }
}
