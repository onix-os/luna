use super::*;
use crate::jit::{
    entry_bytes::EntryBytes,
    resources::{BudgetAllocator, Ledger},
};

#[test]
fn packed_operands_preserve_full_pc_and_decline_null_hosts() {
    for packed in [
        0,
        1,
        255,
        256,
        65535,
        65536,
        0x00ff_ffff,
        0x0100_0000,
        u32::MAX,
    ] {
        for pc in [0, 1, 255, 65536, u32::MAX] {
            let operands = u64::from(packed) | (u64::from(pc) << 32);
            assert_eq!(operands as u32, packed);
            assert_eq!((operands >> 32) as u32, pc);
            assert_eq!(
                unsafe { call_hashless(std::ptr::null_mut(), std::ptr::null_mut(), operands) },
                abi::HELPER_DECLINED
            );
        }
    }
}

#[test]
fn packed_helper_matches_prepared_pc_declines_and_panic_state() {
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return target").unwrap();
        closure.upvalues()[0].get().set(
            &ctx,
            crate::closure::UpValueState::Closed(Value::Table(ctx.globals())),
        );
        ctx.globals().set(ctx, "target", -19).unwrap();
        let hash = crate::table::PreparedStringHash::new(ctx.intern(b"target")).bits();
        let hints = EntryBytes::try_new([true, false].into_iter(), BudgetAllocator(Ledger::new(2)))
            .unwrap();
        for packed in [
            0,
            1,
            255,
            256,
            65535,
            65536,
            0x00ff_ffff,
            0x0100_0000,
            u32::MAX,
        ] {
            for source_pc in [0, 1, 255, 65536, u32::MAX] {
                let observe = |route| {
                    let mut values = [Value::Integer(73), Value::Number(-0.0)];
                    let mut slots = values.map(Slot::from_value);
                    let mut pc = 9;
                    let state = LuaRegisters::with_test_frame(
                        ctx,
                        &mut pc,
                        &mut values,
                        |mut registers| {
                            let mut frame = Frame {
                                ctx,
                                closure,
                                registers: &mut registers,
                                count: Counts::default(),
                                slot_count: slots.len(),
                                panic: None,
                                projection: None,
                                read_hints: (route == 2).then_some(&hints),
                            };
                            let mut host = abi::Host {
                                data: std::ptr::from_mut(&mut frame).cast(),
                                projection: std::ptr::null_mut(),
                            };
                            let status = unsafe {
                                if route == 0 {
                                    PREHASHED_ENTRY(
                                        &mut host,
                                        slots.as_mut_ptr(),
                                        packed,
                                        hash as u32,
                                        (hash >> 32) as u32,
                                        source_pc,
                                    )
                                } else {
                                    call_hashless(
                                        &mut host,
                                        slots.as_mut_ptr(),
                                        u64::from(packed) | (u64::from(source_pc) << 32),
                                    )
                                }
                            };
                            let c = &frame.count;
                            (
                                status,
                                [
                                    c.calls,
                                    c.completed,
                                    c.declined,
                                    c.table_reads,
                                    c.table_writes,
                                    c.upvalue_reads,
                                    c.upvalue_writes,
                                    c.allocations,
                                ],
                                frame.panic.as_deref().map(std::any::Any::type_id),
                            )
                        },
                    );
                    (
                        state,
                        pc,
                        values.map(|v| {
                            let s = Slot::from_value(v);
                            (s.tag, s.bits)
                        }),
                        slots.map(|s| (s.tag, s.bits)),
                    )
                };
                assert_eq!(observe(0), observe(1));
                assert_eq!(observe(0), observe(2));
            }
        }
        assert!(hints.hint(0).unwrap().load().is_some());
        assert_eq!(hints.get(0), Some(&true));
        assert_eq!(hints.get(1), Some(&false));
    });
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn independent_flow_verifier_refuses_hashless_call_corruption() {
    use crate::jit::{
        backend, helper_flow::Fault, ir::Snapshot, resources::MappingCounter, work, JitError,
    };
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"target=target return target").unwrap();
        let snapshot = Snapshot::new(&closure.prototype(), 4096, 1 << 20).unwrap();
        let compile = |fault| {
            let memory = MappingCounter::new(Ledger::new(usize::MAX));
            let result = backend::compile_in(
                &snapshot,
                memory.clone(),
                8 << 20,
                BudgetAllocator(Ledger::new(2 << 20)),
                work::Limits::from(&crate::JitConfig::default()),
                backend::Failure::HashlessFlow(fault),
            );
            (memory, result)
        };
        let (memory, code) = compile(None);
        drop(code.unwrap());
        assert_eq!(memory.load(std::sync::atomic::Ordering::Relaxed), 0);
        for fault in [
            Fault::Symbol,
            Fault::Pointer,
            Fault::Operand,
            Fault::ConstantFlag,
            Fault::SourcePc,
            Fault::Signature,
            Fault::SignatureArity,
            Fault::SignatureWidth,
            Fault::CompletedTest,
            Fault::SuccessTarget,
            Fault::SuccessCount,
            Fault::PanicTest,
            Fault::ExitTarget,
            Fault::ExitPc,
            Fault::ExitCount,
            Fault::Missing,
            Fault::ProgramCounter,
            Fault::Growth,
            Fault::ExtraCall,
            Fault::ExtraStore,
        ] {
            let (memory, result) = compile(Some(fault));
            assert!(
                matches!(result, Err(JitError::Compilation(ref message))
                if message == "invalid helper call data flow"),
                "{fault:?}"
            );
            assert_eq!(memory.load(std::sync::atomic::Ordering::Relaxed), 0);
        }
    });
}
