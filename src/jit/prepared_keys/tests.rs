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
                let results = [false, true].map(|prepared| {
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
                            let status = unsafe {
                                if prepared {
                                    helpers::call_prehashed(
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
                            assert!(frame.panic.is_none());
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
                assert_eq!(results[0].0, results[1].0);
                for (&a, &b) in results[0].1.iter().zip(&results[1].1) {
                    equal(a, b);
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
fn changed_hash_halves_are_rejected_before_native_installation() {
    use crate::jit::{backend, helper_flow::Fault, resources::MappingCounter, work};
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return target").unwrap();
        let snapshot = Snapshot::new(&closure.prototype(), 4096, 1 << 20).unwrap();
        for fault in [Fault::PreparedLow, Fault::PreparedHigh] {
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
