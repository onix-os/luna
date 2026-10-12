use super::*;
use crate::{
    jit::{
        abi, helpers,
        resources::{BudgetAllocator, Ledger},
    },
    thread::LuaRegisters,
    Closure, Value,
};

#[test]
fn source_keys_are_optional_charged_and_keep_reference_bits_zero() {
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return target").unwrap();
        let proto = closure.prototype();
        let bytes = proto.opcodes.len() * std::mem::size_of::<Operation>()
            + proto.constants.len() * std::mem::size_of::<abi::Slot>();
        for limit in [bytes, bytes + proto.constants.len() * 8] {
            let ledger = Ledger::new(limit);
            let snapshot = Snapshot::new_in(&proto, 4096, BudgetAllocator(ledger.clone())).unwrap();
            assert_eq!(snapshot.prepared_keys.is_some(), limit > bytes);
            assert!(snapshot
                .constants
                .iter()
                .filter(|slot| slot.tag == abi::REFERENCE)
                .all(|slot| slot.bits == 0));
            snapshot.verify().unwrap();
            drop(snapshot);
            assert_eq!(ledger.current(), 0);
        }
    });
}

#[test]
fn prepared_storage_covers_only_the_addressable_prefix() {
    crate::Lua::empty().enter(|ctx| {
        let mut source = std::string::String::from("local r=target\n");
        for i in 1000..1300 {
            source.push_str(&format!("r=r+{i}\n"));
        }
        source.push_str("return r");
        let closure = Closure::load(ctx, None, source.as_bytes()).unwrap();
        let proto = closure.prototype();
        assert!(proto.constants.len() > 256);
        let bytes = proto.opcodes.len() * std::mem::size_of::<Operation>()
            + proto.constants.len() * std::mem::size_of::<abi::Slot>();
        let ledger = Ledger::new(bytes + 8);
        let mut snapshot = Snapshot::new_in(&proto, 4096, BudgetAllocator(ledger.clone())).unwrap();
        assert_eq!(snapshot.prepared_keys.as_ref().unwrap().0.len(), 1);
        assert_eq!(ledger.current(), bytes + 8);
        snapshot.verify().unwrap();
        snapshot.prepared_keys.as_mut().unwrap().0.clear();
        assert!(snapshot.verify().is_err());
        drop(snapshot);
        assert_eq!(ledger.current(), 0);
    });
}

#[test]
fn optional_retry_releases_keys_and_preserves_other_errors() {
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return target").unwrap();
        for (enabled, resource_error) in [(true, true), (false, true), (true, false)] {
            let ledger = Ledger::new(1 << 20);
            let mut snapshot =
                Snapshot::new_in(&closure.prototype(), 4096, BudgetAllocator(ledger.clone()))
                    .unwrap();
            let before = ledger.current();
            let keys = snapshot.prepared_keys.as_ref().unwrap().0.len() * 8;
            let mut attempts = 0;
            let result = snapshot.with_prepared_fallback(enabled, |source| {
                attempts += 1;
                if attempts == 1 {
                    assert!(source.prepared_keys.is_some());
                    if resource_error {
                        Err(JitError::ResourceLimit("fixture"))
                    } else {
                        Err(JitError::Compilation("fixture".into()))
                    }
                } else {
                    assert!(source.prepared_keys.is_none());
                    assert_eq!(ledger.current(), before - keys);
                    Ok(())
                }
            });
            assert_eq!(result.is_ok(), enabled && resource_error);
            assert_eq!(attempts, if enabled && resource_error { 2 } else { 1 });
            drop(snapshot);
            assert_eq!(ledger.current(), 0);
        }
        let mut snapshot = Snapshot::new(&closure.prototype(), 4096, 1 << 20).unwrap();
        let mut attempts = 0;
        let result: Result<(), _> = snapshot.with_prepared_fallback(true, |_| {
            attempts += 1;
            Err(JitError::ResourceLimit("persistent"))
        });
        assert!(matches!(result, Err(JitError::ResourceLimit("persistent"))));
        assert_eq!(attempts, 2);
        assert!(snapshot.prepared_keys.is_none());
    });
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn exact_workspace_limit_retains_ordinary_compilation() {
    use crate::jit::{backend, resources::MappingCounter, work};
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return target").unwrap();
        let compile = |source: &Snapshot| {
            backend::compile_in(
                source,
                MappingCounter::new(Ledger::new(usize::MAX)),
                8 << 20,
                BudgetAllocator(Ledger::new(2 << 20)),
                work::Limits::from(&crate::JitConfig::default()),
                backend::Failure::None,
            )
        };
        let baseline = Ledger::new(2 << 20);
        let mut source = Snapshot::new_in(
            &closure.prototype(),
            4096,
            BudgetAllocator(baseline.clone()),
        )
        .unwrap();
        source.prepared_keys = None;
        drop(compile(&source).unwrap());
        let peak = baseline.peak();
        drop(source);
        assert_eq!(baseline.current(), 0);

        let ledger = Ledger::new(peak);
        let mut source =
            Snapshot::new_in(&closure.prototype(), 4096, BudgetAllocator(ledger.clone())).unwrap();
        assert!(source.prepared_keys.is_some());
        assert!(matches!(compile(&source), Err(JitError::ResourceLimit(_))));
        let mut attempts = 0;
        let code = source
            .with_prepared_fallback(true, |source| {
                attempts += 1;
                compile(source)
            })
            .unwrap();
        assert_eq!(attempts, 2);
        assert!(source.prepared_keys.is_none());
        assert!(ledger.peak() <= peak);
        drop(code);
        drop(source);
        assert_eq!(ledger.current(), 0);
    });
}

#[test]
fn register_keys_do_not_select_the_prepared_abi() {
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"local key=... return _ENV[key]").unwrap();
        let snapshot = Snapshot::new(&closure.prototype(), 4096, 1 << 20).unwrap();
        assert!(snapshot.operations.iter().any(|op| matches!(
            op,
            Operation::GetUpTable {
                key: RCIndex::Register(_),
                ..
            }
        )));
        assert!(snapshot.prepared_keys.is_none());
    });
}

fn equal<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
    match (actual, expected) {
        (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
        (Value::Nil, Value::Nil) => {}
        (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
        (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
        (Value::String(a), Value::String(b)) => assert_eq!(a, b),
        (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
        (a, b) => panic!("value mismatch: {a:?} != {b:?}"),
    }
}

#[test]
fn prepared_rust_helper_matches_generic_mutation_and_decline() {
    assert_eq!(
        unsafe { helpers::PREHASHED_ENTRY(std::ptr::null_mut(), std::ptr::null_mut(), 0, 0, 0, 0) },
        abi::HELPER_DECLINED
    );
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return target").unwrap();
        let snapshot = Snapshot::new(&closure.prototype(), 4096, 1 << 20).unwrap();
        let Operation::GetUpTable {
            dest,
            table,
            key: RCIndex::Constant(index),
        } = snapshot.operations[0]
        else {
            panic!("fixture opcode")
        };
        let packed = (u32::from(dest.0) << 16) | (u32::from(table.0) << 8) | u32::from(index.0);
        let hash = snapshot.prepared_keys.as_ref().unwrap().hash(index.0);
        for metamethod in [false, true] {
            let mt = crate::Table::new(&ctx);
            mt.set(ctx, "__index", crate::Table::new(&ctx)).unwrap();
            ctx.globals().set_metatable(ctx, metamethod.then_some(mt));
            for value in [
                Value::Nil,
                Value::Boolean(false),
                Value::Integer(i64::MAX),
                Value::Number(-0.0),
                Value::Number(f64::from_bits(0x7ff8_0000_0000_4321)),
                Value::String(ctx.intern(b"result")),
                Value::Table(crate::Table::new(&ctx)),
            ] {
                ctx.globals().set(ctx, "target", value).unwrap();
                for borrowed in [false, true] {
                    let results = [0, 1, 2, 3].map(|prepared| {
                        let mut canonical = std::vec![Value::Integer(73); snapshot.registers];
                        let mut pc = 0;
                        let result = LuaRegisters::with_test_frame(
                            ctx,
                            &mut pc,
                            &mut canonical,
                            |mut registers| {
                                let mut slots: std::vec::Vec<_> = registers
                                    .stack_frame
                                    .iter()
                                    .copied()
                                    .map(abi::Slot::from_value)
                                    .collect();
                                let mut frame = helpers::Frame {
                                    ctx,
                                    closure,
                                    registers: &mut registers,
                                    count: helpers::Counts::default(),
                                    slot_count: slots.len(),
                                    panic: None,
                                    projection: None,
                                };
                                let mut host = abi::Host {
                                    data: std::ptr::from_mut(&mut frame).cast(),
                                    projection: std::ptr::null_mut(),
                                };
                                let global_table = ctx.globals().into_inner();
                                let guard = borrowed.then(|| global_table.borrow_mut(&ctx));
                                let status = unsafe {
                                    if prepared >= 2 {
                                        let entry = if prepared == 2 {
                                            helpers::wide::call
                                        } else {
                                            helpers::wide::direct
                                        };
                                        entry(
                                            &mut host,
                                            slots.as_mut_ptr(),
                                            helpers::wide::operands(packed, 0),
                                            hash,
                                        )
                                    } else if prepared == 1 {
                                        helpers::PREHASHED_ENTRY(
                                            &mut host,
                                            slots.as_mut_ptr(),
                                            packed,
                                            hash as u32,
                                            (hash >> 32) as u32,
                                            0,
                                        )
                                    } else {
                                        helpers::call::<{ abi::HELPER_GET_UP_TABLE }>(
                                            &mut host,
                                            slots.as_mut_ptr(),
                                            u32::from(dest.0),
                                            u32::from(table.0),
                                            abi::CONSTANT_OPERAND | u32::from(index.0),
                                            0,
                                        )
                                    }
                                };
                                assert_eq!(frame.panic.is_some(), borrowed);
                                drop(guard);
                                (
                                    status,
                                    *frame.registers.pc,
                                    frame.count.calls,
                                    frame.count.completed,
                                    frame.count.declined,
                                    frame.count.table_reads,
                                    frame.count.upvalue_reads,
                                )
                            },
                        );
                        (result, canonical)
                    });
                    for result in &results[1..] {
                        assert_eq!(results[0].0, result.0);
                        for (&a, &b) in results[0].1.iter().zip(&result.1) {
                            equal(a, b);
                        }
                    }
                }
            }
        }
    });
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn changed_prepared_abi_is_rejected_before_native_installation() {
    use crate::jit::{backend, helper_flow::Fault, resources::MappingCounter, work};
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"target=target return target").unwrap();
        let snapshot = Snapshot::new(&closure.prototype(), 4096, 1 << 20).unwrap();
        assert!(snapshot.prepared_keys.is_some());
        for fault in [
            Fault::Symbol, Fault::Pointer, Fault::Operand, Fault::ConstantFlag,
            Fault::PreparedLow, Fault::PreparedHigh, Fault::SourcePc, Fault::Signature,
            Fault::CompletedTest, Fault::SuccessTarget, Fault::SuccessCount,
            Fault::PanicTest, Fault::ExitTarget, Fault::ExitPc, Fault::ExitCount,
            Fault::Missing, Fault::ProgramCounter, Fault::Growth, Fault::ExtraCall, Fault::ExtraStore,
        ] {
            let memory = MappingCounter::new(Ledger::new(usize::MAX));
            let result = backend::compile_in(&snapshot, memory.clone(), 8 << 20, BudgetAllocator(Ledger::new(2 << 20)), work::Limits::from(&crate::JitConfig::default()), backend::Failure::CorruptHelperFlow(fault));
            assert!(matches!(result, Err(JitError::Compilation(ref message)) if message == "invalid helper call data flow"));
            assert_eq!(memory.load(std::sync::atomic::Ordering::Relaxed), 0);
        }
    });
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn prepared_lookup_executes_in_the_real_runtime() {
    let mut lua = crate::Lua::core();
    lua.set_jit_config(crate::JitConfig {
        mode: crate::JitMode::Auto,
        hot_threshold: 1,
        ..Default::default()
    })
    .unwrap();
    let executor = lua.enter(|ctx| {
        ctx.globals().set(ctx, "target", 17).unwrap();
        let closure = Closure::load(
            ctx,
            None,
            b"local s=0 for i=1,50 do s=s+target end return s",
        )
        .unwrap();
        let snapshot = Snapshot::new(&closure.prototype(), 4096, 1 << 20).unwrap();
        assert!(snapshot.prepared_keys.is_some());
        ctx.stash(crate::Executor::start(ctx, closure.into(), ()))
    });
    assert!(lua.prepare_jit().unwrap() > 0);
    assert_eq!(lua.execute::<i64>(&executor).unwrap(), 850);
    assert_eq!(lua.jit_stats().native_table_reads, 50);
    assert_eq!(lua.jit_stats().native_upvalue_reads, 50);
    lua.gc_collect();
    lua.clear_jit_cache();
    assert_eq!(lua.jit_stats().code_bytes, 0);
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn prepared_sources_keep_distinct_keys_and_current_values() {
    let mut lua = crate::Lua::core();
    lua.set_jit_config(crate::JitConfig {
        mode: crate::JitMode::Auto,
        hot_threshold: 1,
        ..Default::default()
    })
    .unwrap();
    let closures = lua.enter(|ctx| {
        ["first", "other"].map(|key| {
            let source = format!("local s=0 for i=1,50 do s=s+{key} end return s");
            let closure = Closure::load(ctx, None, source.as_bytes()).unwrap();
            let snapshot = Snapshot::new(&closure.prototype(), 4096, 1 << 20).unwrap();
            assert!(snapshot.prepared_keys.is_some());
            ctx.stash(closure)
        })
    });
    for round in 1..=3 {
        lua.enter(|ctx| {
            ctx.globals().set(ctx, "first", 17 * round).unwrap();
            ctx.globals().set(ctx, "other", 29 * round).unwrap();
        });
        for (source, value) in closures.iter().zip([17, 29]) {
            let executor = lua
                .enter(|ctx| ctx.stash(crate::Executor::start(ctx, ctx.fetch(source).into(), ())));
            lua.prepare_jit().unwrap();
            let reads = lua.jit_stats().native_table_reads;
            assert_eq!(lua.execute::<i64>(&executor).unwrap(), value * round * 50);
            assert_eq!(lua.jit_stats().native_table_reads - reads, 50);
        }
        lua.gc_collect();
    }
    assert_eq!(lua.jit_stats().native_table_reads, 300);
    lua.clear_jit_cache();
    assert_eq!(lua.jit_stats().code_bytes, 0);
}
