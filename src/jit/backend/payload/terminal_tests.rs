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
