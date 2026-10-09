use super::*;

#[test]
fn rooted_native_calls_match_physical_calls_with_pending_values() {
    let mut completed = 0;
    for source in [
        ADD,
        &b"local n=7 local function f(v) n=n-v end f(2) return n"[..],
        &b"local n=7 local function f(v) n=n*v end f(2) return n"[..],
        &b"local n=7 local function f() n=n+2 end f() return n"[..],
        &b"local n=7 local function f(v,w) n=n+v end f(2,{}) return n"[..],
        &b"local n=7 local function f(v,...) n=n+v end f(2,3) return n"[..],
    ] {
        fixture(source, |lua, closure, site, code| {
            lua.enter(|ctx| {
                let caller = ctx.fetch(&closure);
                let callee = ctx.jit_registry().borrow().resolve(ctx, site.callee).unwrap();
                for budget in [0, 3, 4, 64] {
                    for available in [0, 9, 10, 64, 10000] {
                        for prefix in [0, 4, 64] {
                            for pending in [false, true] {
                                let mut run = |virtual_call| with_test_thread(ctx, caller, &mut Fuel::with(10000), |host| {
                                    position(host, ctx, site.pc);
                                    host.test_fuel(Fuel::with(available));
                                    let (mut roots, mut slots) = host.with_registers(|_, registers| {
                                        let mut roots = vec![Value::Nil; registers.stack_frame.len()];
                                        let mut slots = vec![Slot::from_value(Value::Nil); roots.len()];
                                        assert!(abi::roots::capture(&mut roots, &mut slots, registers.stack_frame));
                                        (roots, slots)
                                    });
                                    if pending {
                                        slots[0] = Slot::from_value(Value::Integer(i64::MAX));
                                        if site.arguments != 0 { slots[usize::from(site.function.0) + 1] = Slot::from_value(Value::Integer(3)); }
                                    }
                                    let before = stats(ctx);
                                    let before_counts = ctx.jit().0.borrow().stats;
                                    let mut success = false;
                                    if virtual_call {
                                        let prior = trace(ctx, host, before);
                                        let mut snapshot = abi::roots::call::Snapshot::new(&roots, &mut slots).unwrap();
                                        success = super::super::rooted::invoke(ctx, host, &site, &code, budget, prefix, (caller, callee), &mut snapshot);
                                        if success {
                                            completed += 1;
                                            host.with_registers(|_, registers| assert!(matches!(registers.stack_frame[0], Value::Integer(7))));
                                        } else { assert_eq!(trace(ctx, host, before), prior); }
                                    }
                                    host.with_registers(|_, registers| {
                                        assert!(abi::roots::materialize(&roots, &slots, registers.stack_frame));
                                        if !success { *registers.pc = site.pc; }
                                    });
                                    if !success {
                                        let mut scratch = [MaybeUninit::uninit(); 256];
                                        let mut session = Session::new(ctx, host, &site, &mut scratch);
                                        session.prefix = prefix;
                                        session.invoke_compact(&code, budget);
                                        session.finish().result.unwrap();
                                    }
                                    let after = ctx.jit().0.borrow().stats;
                                    roots.clear();
                                    (trace(ctx, host, before), after.interpreted_slices - before_counts.interpreted_slices,
                                        after.native_entries - before_counts.native_entries,
                                        after.native_upvalue_reads - before_counts.native_upvalue_reads,
                                        after.native_upvalue_writes - before_counts.native_upvalue_writes)
                                });
                                assert_eq!(run(true), run(false), "budget={budget},fuel={available},prefix={prefix},pending={pending},source={source:?}");
                            }
                        }
                    }
                }
            });
        });
    }
    assert!(completed > 50, "no rooted native coverage: {completed}");
}
