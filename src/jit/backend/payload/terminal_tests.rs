use super::*;
use cranelift_codegen::{
    ir::{ExtFuncData, ExternalName},
    settings,
};

fn fixture(
    read: bool,
    overwrite: bool,
    helper: u32,
    cycle: bool,
) -> (Function, IrValue, IrValue, Inst, [(u32, FuncRef); 1]) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature
        .params
        .extend([types::I64, types::I64, types::I32].map(AbiParam::new));
    let mut signature = Signature::new(f.signature.call_conv);
    signature.params.extend(
        [
            types::I64,
            types::I64,
            types::I32,
            types::I32,
            types::I32,
            types::I32,
        ]
        .map(AbiParam::new),
    );
    signature.returns.push(AbiParam::new(types::I32));
    let signature = f.import_signature(signature);
    let reference = f.import_function(ExtFuncData {
        name: ExternalName::testcase("terminal_helper"),
        signature,
        colocated: false,
        patchable: false,
    });
    let mut frontend = FunctionBuilderContext::new();
    let (slots, host, store);
    {
        let mut b = FunctionBuilder::new(&mut f, &mut frontend);
        let entry = b.create_block();
        let left = b.create_block();
        let right = b.create_block();
        let done = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let condition;
        [slots, host, condition] = b.block_params(entry).try_into().unwrap();
        let tag = b.ins().iconst(types::I64, abi::INTEGER as i64);
        let bits = b.ins().iconst(types::I64, 7);
        b.ins().store(MemFlagsData::new(), tag, slots, 0);
        store = b.ins().store(MemFlagsData::new(), bits, slots, 8);
        b.ins().brif(condition, left, &[], right, &[]);
        b.switch_to_block(left);
        if helper != 0 {
            let zero = b.ins().iconst(types::I32, 0);
            b.ins()
                .call(reference, &[host, slots, zero, zero, zero, zero]);
        }
        if read {
            b.ins().load(types::I64, MemFlagsData::new(), slots, 8);
        }
        if overwrite {
            b.ins().store(MemFlagsData::new(), tag, slots, 0);
            b.ins().store(MemFlagsData::new(), bits, slots, 8);
        }
        if cycle {
            b.ins().brif(condition, left, &[], done, &[]);
        } else {
            b.ins().jump(done, &[]);
        }
        b.switch_to_block(right);
        b.ins().load(types::I64, MemFlagsData::new(), slots, 24);
        b.ins().jump(done, &[]);
        b.switch_to_block(done);
        b.ins().return_(&[]);
        b.seal_all_blocks();
        b.finalize(
            cranelift_codegen::isa::lookup_by_name("x86_64")
                .unwrap()
                .finish(settings::Flags::new(settings::builder()))
                .unwrap()
                .frontend_config(),
        );
    }
    cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder())).unwrap();
    (f, slots, host, store, [(helper.max(1), reference)])
}

#[test]
fn terminal_liveness_respects_all_paths_loops_and_helper_effects() {
    for read in [false, true] {
        for overwrite in [false, true] {
            for helper in 0..=10 {
                for cycle in [false, true] {
                    let (f, slots, host, store, helpers) = fixture(read, overwrite, helper, cycle);
                    let allocator = BudgetAllocator(crate::jit::resources::Ledger::new(4096));
                    let mut analysis =
                        Analysis::new(&f, slots, host, 2, &helpers, allocator.clone()).unwrap();
                    let refreshes = helper != 0 && helper != abi::HELPER_GET_UP_TABLE;
                    assert_eq!(
                        analysis.admits(store, 0),
                        refreshes || !(read || overwrite),
                        "read={read} overwrite={overwrite} helper={helper} cycle={cycle}"
                    );
                    assert!(!analysis.admits(store, 1));
                    analysis.remaining = 0;
                    assert!(!analysis.admits(store, 0));
                    drop(analysis);
                    assert_eq!(allocator.0.current(), 0);
                }
            }
        }
    }
}

#[test]
fn terminal_liveness_workspace_refusal_does_not_mutate_ir() {
    let (f, slots, host, _, helpers) = fixture(false, false, 0, false);
    let bytes = f.dfg.num_blocks() * (std::mem::size_of::<bool>() + std::mem::size_of::<Block>());
    for limit in [0, bytes - 1, bytes] {
        let allocator = BudgetAllocator(crate::jit::resources::Ledger::new(limit));
        let result = Analysis::new(&f, slots, host, 2, &helpers, allocator.clone());
        assert_eq!(result.is_ok(), limit == bytes);
        if result.is_ok() {
            assert_eq!(allocator.0.current(), bytes);
        }
        drop(result);
        assert_eq!(allocator.0.current(), 0);
    }
}

#[test]
fn terminal_gateway_selection_follows_original_graph_liveness() {
    for (read, helper, expected) in [(false, 0, 1), (true, 0, 0), (true, 6, 0), (true, 1, 1)] {
        let (mut f, slots, host, _, helpers) = fixture(read, false, helper, false);
        lower(
            &mut f,
            slots,
            host,
            2,
            &helpers,
            BudgetAllocator(crate::jit::resources::Ledger::new(1 << 20)),
            crate::jit::work::Expansion {
                instructions: 4096,
                blocks: 4096,
            },
        )
        .unwrap();
        cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder())).unwrap();
        let gateways = f
            .layout
            .blocks()
            .flat_map(|block| f.layout.block_insts(block))
            .filter(|&inst| {
                matches!(f.dfg.insts[inst], InstructionData::Load { offset, .. }
                if i32::from(offset) == std::mem::offset_of!(Bridge, terminal) as i32)
            })
            .count();
        assert_eq!(gateways, expected, "read={read} helper={helper}");
    }
}

#[cfg(not(miri))]
#[test]
fn native_terminal_stores_preserve_canonical_values_at_budget_and_call_exits() {
    use crate::{
        jit::{resources::Ledger, JitConfig},
        thread::LuaRegisters,
        Value,
    };
    crate::Lua::empty().enter(|ctx| {
        for source in [b"return 7".as_slice(), b"return host_increment(7)"] {
            let closure = crate::Closure::load(ctx, None, source).unwrap();
            let snapshot = Snapshot::new(&closure.prototype(), 4096, 2 * 1024 * 1024).unwrap();
            let (pc, destination) = snapshot
                .operations
                .iter()
                .enumerate()
                .find_map(|(pc, op)| match op {
                    Operation::LoadConstant { dest, constant }
                        if snapshot.constants[usize::from(constant.0)].tag == abi::INTEGER =>
                    {
                        Some((pc, usize::from(dest.0)))
                    }
                    _ => None,
                })
                .unwrap();
            let total = MappingCounter::new(Ledger::new(8 * 1024 * 1024));
            let control = compile(&snapshot, total.clone(), 1024 * 1024).unwrap();
            let code = compile_in(
                &snapshot,
                total.clone(),
                1024 * 1024,
                BudgetAllocator(Ledger::new(2 * 1024 * 1024)),
                crate::jit::work::Limits::from(&JitConfig::default()),
                Failure::RequirePayload,
            )
            .unwrap();
            for budget in [0, 1, 2, 64] {
                let mut expected = vec![Value::Nil; snapshot.registers];
                let (expected_exit, counts) =
                    super::super::run(&control, None, (ctx, closure), &mut expected, pc, budget);
                assert_eq!(counts, [0; 8]);
                let mut actual = vec![Value::Nil; snapshot.registers];
                let mut helper_pc = pc;
                let mut exit = Exit::default();
                LuaRegisters::with_test_frame(ctx, &mut helper_pc, &mut actual, |mut registers| {
                    let mut frame = helpers::Frame {
                        ctx,
                        closure,
                        registers: &mut registers,
                        count: helpers::Counts::default(),
                        slot_count: snapshot.registers,
                        panic: None,
                        projection: None,
                    };
                    type Entry =
                        unsafe extern "C" fn(*mut Payload, u64, u32, *mut Exit, *mut Bridge);
                    let entry: Entry = unsafe { std::mem::transmute(code.entry) };
                    crate::jit::abi::payload::runtime::with_bridge::<256>(
                        &mut frame,
                        |slots, host| unsafe {
                            entry(slots, pc as u64, budget, &mut exit, host);
                            crate::jit::abi::payload::runtime::check_retired_descriptor(
                                slots.add(destination).read(),
                            );
                        },
                    );
                    assert!(frame.panic.is_none());
                    assert_eq!(frame.count.calls, 0);
                });
                assert_eq!(
                    (exit.pc, exit.instructions, exit.reason),
                    (
                        expected_exit.pc,
                        expected_exit.instructions,
                        expected_exit.reason
                    )
                );
                for (actual, expected) in actual.iter().copied().zip(expected) {
                    let (actual, expected) = (Slot::from_value(actual), Slot::from_value(expected));
                    assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
                }
                if budget != 0 {
                    assert!(matches!(actual[destination], Value::Integer(7)));
                }
            }
            drop((code, control));
            assert_eq!(total.load(Ordering::Relaxed), 0);
        }
    });
}
