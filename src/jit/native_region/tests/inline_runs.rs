use super::*;

#[test]
fn inline_regions_match_original_regions_and_interpreter_at_slice_boundaries() {
    for source in [
        &b"local x=0 local function f(v) x=x+v end for i=1,20 do f(i) end return x"[..],
        &b"local x=7 local function f(v) x=x-v end for i=31,-10,-3 do f(i) end return x"[..],
        &b"local x=3 local function f() x=x*2 end for i=1,20 do f() end return x"[..],
        &b"local x=9223372036854775800 local function f(v) x=x+v end for i=1,20 do f(i) end return x"[..],
        &b"local x=0.5 local function f(v) x=x+v end for i=1,20 do f(i) end return x"[..],
    ] {
        fixture_source(source, |ctx, closure, mut region, start| {
            let inline = region.inline.take().expect("source should admit an inline image");
            for limit in [1, 2, 3, 4, 8, 17, 64] {
                for budget in [1, 2, 3, 4, 5, 8, 16, 64] {
                    for fuel in [-1, 0, 1, 8, 9, 19, 20, 21, 39, 40, 10000] {
                        for selected in [false, true] {
                            region.inline = selected.then(|| inline.clone());
                            let (native, slices, error) = with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                                host.run(ctx, 1, start as u32, 4).result.unwrap();
                                host.test_fuel(Fuel::with(fuel));
                                ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                                let before = stats(ctx);
                                let pairs = ctx.jit().stats().native_pair_returns;
                                let outcome = region.run(ctx, host, limit, budget).unwrap();
                                assert!(outcome.slices > 0 && outcome.slices <= limit);
                                assert_eq!(ctx.jit().stats().native_pair_returns - pairs, outcome.pairs as u64);
                                assert!(ctx.jit().0.borrow().resume_scope.is_none());
                                (trace(ctx, host, before), outcome.slices, outcome.result.err().map(|e| e.to_string()))
                            });
                            let canonical = with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                                host.run(ctx, 1, start as u32, 4).result.unwrap();
                                host.test_fuel(Fuel::with(fuel));
                                let before = stats(ctx);
                                let result = host.run(ctx, slices, budget, 4).result;
                                (trace(ctx, host, before), result.err().map(|e| e.to_string()))
                            });
                            assert_eq!((native, error), canonical,
                                "inline={selected} limit={limit} budget={budget} fuel={fuel} source={}", String::from_utf8_lossy(source));
                        }
                    }
                }
            }
        });
    }
}

#[test]
fn later_inline_batch_unwind_keeps_published_state_and_code_lease() {
    fixture(|ctx, closure, region, start| {
        assert!(region.inline.is_some());
        for skip in [0, 1] {
            let before_memory = ctx.jit().0.borrow().memory.requested();
            let (actual, calls) = with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                host.run(ctx, 1, start as u32, 4).result.unwrap();
                ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
                let before = stats(ctx);
                ROOTED_COMPLETIONS.with(|value| value.set(0));
                ROOTED_PANIC_SKIP.with(|value| value.set(skip));
                ROOTED_PANIC.with(|value| value.set(2));
                assert!(catch_unwind(AssertUnwindSafe(|| region.run(ctx, host, 64, 64))).is_err());
                assert_eq!(ROOTED_PANIC.with(|value| value.replace(0)), 0);
                assert_eq!(ROOTED_PANIC_SKIP.with(|value| value.replace(0)), 0);
                let calls = ROOTED_COMPLETIONS.with(|value| value.get());
                assert!(calls > 1);
                (trace(ctx, host, before), calls)
            });
            let expected = with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
                host.run(ctx, 1, start as u32, 4).result.unwrap();
                let before = stats(ctx);
                host.run(ctx, 2 * calls, 64, 4).result.unwrap();
                trace(ctx, host, before)
            });
            assert_eq!(actual, expected);
            assert_eq!(ctx.jit().0.borrow().memory.requested(), before_memory);
        }
    });
}

#[test]
fn inline_ir_refusal_preserves_original_region_and_releases_workspace() {
    fixture(|ctx, closure, original, start| {
        assert!(original.inline.is_some());
        let allocator = ctx.jit().0.borrow().snapshots.clone();
        let source =
            crate::jit::ir::Snapshot::new_in(&closure.prototype(), 4096, allocator.clone())
                .unwrap();
        let limits = crate::jit::work::Limits::from(&ctx.jit().0.borrow().config);
        let expansion = crate::jit::work::Expansion::admit(&source, limits).unwrap();
        drop(source);
        ctx.jit().0.borrow_mut().config.max_ir_instructions = expansion.instructions + 64;
        let pair = PreparedPair {
            program: original.pair.program.clone(),
        };
        let region = Request::prepare(ctx, closure.prototype(), pair)
            .unwrap()
            .compile(ctx.jit())
            .unwrap();
        assert!(region.inline.is_none());
        assert_eq!(allocator.0.current(), 0);
        with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
            ctx.jit().0.borrow_mut().config.mode = JitMode::Off;
            host.run(ctx, 1, start as u32, 4).result.unwrap();
            ctx.jit().0.borrow_mut().config.mode = JitMode::Auto;
            let result = region.run(ctx, host, 64, 64).unwrap();
            result.result.unwrap();
            assert_eq!(result.pairs, 20);
            assert!(matches!(
                host.test_trace().1.as_slice(),
                [Value::Integer(210)]
            ));
        });
    });
}
