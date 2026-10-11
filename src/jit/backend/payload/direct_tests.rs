use super::*;
use cranelift_codegen::ir::{ExtFuncData, ExternalName, UserExternalName};

fn fixture(index: usize) -> (Function, IrValue, Targets) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature.params = [
        types::I64,
        types::I64,
        types::I32,
        types::I32,
        types::I32,
        types::I32,
    ]
    .map(AbiParam::new)
    .to_vec();
    f.signature.returns = vec![AbiParam::new(types::I32)];
    let signature = f.signature.clone();
    let call_conv = signature.call_conv;
    let helper = f.import_signature(signature);
    let writer = f.import_signature(Signature {
        params: [types::I64, types::I64, types::I32, types::I64, types::I64]
            .map(AbiParam::new)
            .to_vec(),
        returns: vec![],
        call_conv,
    });
    let targets = std::array::from_fn(|index| {
        let name = f.declare_imported_user_function(UserExternalName::new(0, index as u32));
        f.import_function(ExtFuncData {
            name: ExternalName::user(name),
            signature: if index < 10 { helper } else { writer },
            colocated: false,
            patchable: false,
        })
    });
    let block = f.dfg.make_block();
    f.layout.append_block(block);
    let args = [
        types::I64,
        types::I64,
        types::I32,
        types::I32,
        types::I32,
        types::I32,
    ]
    .map(|ty| f.dfg.append_block_param(block, ty));
    let mut c = FuncCursor::new(&mut f);
    c.goto_bottom(block);
    let table = c.ins().load(
        types::I64,
        MemFlagsData::new(),
        args[0],
        std::mem::offset_of!(Bridge, functions) as i32,
    );
    let target = c
        .ins()
        .load(types::I64, MemFlagsData::new(), table, target_offset(index));
    let result = if index < 10 {
        let call = c.ins().call_indirect(helper, target, &args);
        c.func.dfg.first_result(call)
    } else {
        c.ins().call_indirect(
            writer,
            target,
            &[args[0], args[1], args[2], args[0], args[1]],
        );
        c.ins().iconst(types::I32, 0)
    };
    c.ins().return_(&[result]);
    (f, args[0], targets)
}

fn allocator(limit: usize) -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(limit))
}

#[test]
fn all_static_targets_preserve_abi_and_remove_only_two_loads() {
    for index in 0..12 {
        let (mut f, host, targets) = fixture(index);
        let original = size(&f);
        let memory = allocator(1 << 20);
        assert_eq!(lower(&mut f, host, &targets, memory.clone()).unwrap(), 1);
        assert_eq!(size(&f), (original.0 - 2, original.1));
        assert_eq!(memory.0.current(), 0);
        cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder())).unwrap();
        assert!(f.layout.blocks().flat_map(|b| f.layout.block_insts(b)).any(|i| matches!(f.dfg.insts[i], InstructionData::Call {opcode: Opcode::Call, func_ref, ..} if func_ref == targets[index])));
    }
}

#[test]
fn static_dispatch_refuses_workspace_before_mutation() {
    let (source, host, targets) = fixture(0);
    let exact = std::mem::size_of::<Site>()
        + source.dfg.num_values() * std::mem::size_of::<Option<(Inst, usize)>>();
    for limit in [0, std::mem::size_of::<Site>() - 1, exact - 1, exact] {
        let memory = allocator(limit);
        let mut f = source.clone();
        let result = lower(&mut f, host, &targets, memory.clone());
        assert_eq!(result.is_ok(), limit == exact);
        if result.is_err() {
            assert_eq!(f, source);
        }
        assert_eq!(memory.0.current(), 0);
    }
}

#[test]
fn static_dispatch_refuses_changed_tables_signatures_and_escaping_targets() {
    for index in 0..12 {
        for fault in 0..8 {
            let (mut f, host, targets) = fixture(index);
            let block = f.layout.entry_block().unwrap();
            let table = f.layout.first_inst(block).unwrap();
            let target = f.layout.next_inst(table).unwrap();
            let call = f.layout.next_inst(target).unwrap();
            match fault {
                0 => {
                    if let InstructionData::Load { offset, .. } = &mut f.dfg.insts[table] {
                        *offset = 0.into();
                    }
                }
                1 => {
                    if let InstructionData::Load { offset, .. } = &mut f.dfg.insts[target] {
                        *offset = 1.into();
                    }
                }
                2 => {
                    let slots = f.dfg.block_params(block)[1];
                    f.dfg.inst_args_mut(table)[0] = slots;
                }
                3 => {
                    let slots = f.dfg.block_params(block)[1];
                    f.dfg.inst_args_mut(call)[1] = slots;
                }
                4 => {
                    let value = f.dfg.first_result(target);
                    let mut c = FuncCursor::new(&mut f);
                    c.goto_inst(call);
                    c.ins().iadd(value, value);
                }
                5 => {
                    let value = f.dfg.first_result(table);
                    let mut c = FuncCursor::new(&mut f);
                    c.goto_inst(call);
                    c.ins().iadd(value, value);
                }
                6 => {
                    let signature = f.dfg.ext_funcs[targets[index]].signature;
                    f.dfg.signatures[signature].call_conv =
                        cranelift_codegen::isa::CallConv::WindowsFastcall;
                }
                _ => {
                    let mut flags = MemFlagsData::new();
                    flags.set_notrap();
                    let flags = f.dfg.mem_flags.insert(flags).unwrap();
                    if let InstructionData::Load { flags: actual, .. } = &mut f.dfg.insts[target] {
                        *actual = flags;
                    }
                }
            }
            cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder()))
                .unwrap();
            let before = f.clone();
            let memory = allocator(1 << 20);
            assert!(
                lower(&mut f, host, &targets, memory.clone()).is_err(),
                "index={index} fault={fault}"
            );
            assert_eq!(f, before);
            assert_eq!(memory.0.current(), 0);
        }
    }
}

#[test]
fn static_rewrite_verifier_refuses_changed_targets_operands_and_surroundings() {
    for index in 0..12 {
        let (source, host, targets) = fixture(index);
        let block = source.layout.entry_block().unwrap();
        let call = source
            .layout
            .block_insts(block)
            .find(|&i| source.dfg.insts[i].opcode() == Opcode::CallIndirect)
            .unwrap();
        let record = site(&source, host, call, &targets).unwrap();
        let mut valid = source.clone();
        lower(&mut valid, host, &targets, allocator(1 << 20)).unwrap();
        verify(&source, &valid, &[record], &targets).unwrap();
        for fault in 0..5 {
            let mut f = valid.clone();
            match fault {
                0 => {
                    if let InstructionData::Call { func_ref, .. } = &mut f.dfg.insts[call] {
                        *func_ref = targets[if index < 10 {
                            (index + 1) % 10
                        } else {
                            21 - index
                        }];
                    }
                }
                1 => f.dfg.inst_args_mut(call)[0] = source.dfg.block_params(block)[1],
                2 => f.dfg.inst_args_mut(call)[1] = host,
                3 => {
                    let last = f.layout.last_inst(block).unwrap();
                    let mut c = FuncCursor::new(&mut f);
                    c.goto_inst(last);
                    let value = c.ins().iconst(types::I32, 77);
                    c.func.dfg.inst_args_mut(last)[0] = value;
                }
                _ => {
                    let mut c = FuncCursor::new(&mut f);
                    c.goto_inst(call);
                    c.ins().iconst(types::I32, 99);
                }
            }
            cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder()))
                .unwrap();
            assert!(
                verify(&source, &f, &[record], &targets).is_err(),
                "index={index} fault={fault}"
            );
        }
    }
}

#[test]
fn direct_calls_keep_host_stack_copies_and_account_for_removed_uses() {
    for index in 0..12 {
        let (mut f, host, targets) = fixture(index);
        let source = size(&f);
        let memory = allocator(1 << 20);
        lower(&mut f, host, &targets, memory.clone()).unwrap();
        let expansion = crate::jit::work::Expansion {
            instructions: 4096,
            blocks: 4096,
        };
        let copy = host_stack::lower(&mut f, host, memory.clone(), expansion)
            .unwrap()
            .unwrap();
        host_stack::verify(&f, &copy).unwrap();
        assert_eq!(size(&f), (source.0 + 2, source.1));
        drop(copy);
        assert_eq!(memory.0.current(), 0);
        cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder())).unwrap();
    }
}

#[test]
fn static_rewrite_checks_branch_pools_not_only_instruction_handles() {
    let (mut source, host, targets) = fixture(0);
    let entry = source.layout.entry_block().unwrap();
    let end = source.layout.last_inst(entry).unwrap();
    let result = source.dfg.inst_args(end)[0];
    let destination = source.dfg.make_block();
    source.layout.append_block(destination);
    let parameter = source.dfg.append_block_param(destination, types::I32);
    source.replace(end).jump(destination, &[result.into()]);
    let mut c = FuncCursor::new(&mut source);
    c.goto_bottom(destination);
    c.ins().return_(&[parameter]);
    let call = source
        .layout
        .block_insts(entry)
        .find(|&i| source.dfg.insts[i].opcode() == Opcode::CallIndirect)
        .unwrap();
    let record = site(&source, host, call, &targets).unwrap();
    let mut candidate = source.clone();
    lower(&mut candidate, host, &targets, allocator(1 << 20)).unwrap();
    let mut branch = candidate.dfg.insts[end]
        .branch_destination(&candidate.dfg.jump_tables, &candidate.dfg.exception_tables)[0];
    branch.update_args(&mut candidate.dfg.value_lists, |_| {
        BlockArg::Value(source.dfg.block_params(entry)[2])
    });
    assert_eq!(source.dfg.insts[end], candidate.dfg.insts[end]);
    cranelift_codegen::verify_function(&candidate, &settings::Flags::new(settings::builder()))
        .unwrap();
    assert!(verify(&source, &candidate, &[record], &targets).is_err());
}
