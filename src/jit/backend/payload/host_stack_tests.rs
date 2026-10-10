use super::*;

fn allocator(limit: usize) -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(limit))
}

fn fixture() -> (Function, IrValue) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature.params.push(AbiParam::new(types::I64));
    f.signature.returns.push(AbiParam::new(types::I64));
    let signature = f.signature.clone();
    let signature = f.import_signature(signature);
    let block = f.dfg.make_block();
    f.layout.append_block(block);
    let host = f.dfg.append_block_param(block, types::I64);
    let mut c = FuncCursor::new(&mut f);
    c.goto_bottom(block);
    let target = c.ins().load(types::I64, MemFlagsData::new(), host, 0);
    let call = c.ins().call_indirect(signature, target, &[host]);
    let result = c.func.dfg.first_result(call);
    c.ins().return_(&[result]);
    (f, host)
}

fn expansion() -> crate::jit::work::Expansion {
    crate::jit::work::Expansion {
        instructions: 4096,
        blocks: 4096,
    }
}

#[test]
fn host_copy_is_private_initialized_and_exactly_accounted() {
    let (mut f, host) = fixture();
    let original = size(&f);
    let allocator = allocator(workspace(2));
    let copy = lower(&mut f, host, allocator.clone(), expansion())
        .unwrap()
        .unwrap();
    verify(&f, &copy).unwrap();
    cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder())).unwrap();
    assert_eq!(size(&f), (original.0 + 6, original.1));
    assert_eq!(allocator.0.current(), workspace(2));
    assert_eq!(allocator.0.peak(), workspace(2));
    drop(copy);
    assert_eq!(allocator.0.current(), 0);
}

#[test]
fn host_copy_refuses_quota_and_expansion_before_mutation() {
    let (source, host) = fixture();
    for (bytes, instructions) in [
        (workspace(2) - 1, 4096),
        (workspace(2), size(&source).0 + 5),
    ] {
        let mut f = source.clone();
        let allocator = allocator(bytes);
        assert!(lower(
            &mut f,
            host,
            allocator.clone(),
            crate::jit::work::Expansion {
                instructions,
                ..expansion()
            }
        )
        .is_err());
        assert_eq!(f, source);
        assert_eq!(allocator.0.current(), 0);
        assert_eq!(allocator.0.peak(), 0);
    }
}

#[test]
fn host_copy_verifier_rejects_mutated_storage_and_escaping_values() {
    for fault in 0..9 {
        let (mut f, host) = fixture();
        let mut copy = lower(&mut f, host, allocator(4096), expansion())
            .unwrap()
            .unwrap();
        let r = copy.reloads[0];
        match fault {
            0 => f.sized_stack_slots[copy.slot].size = 16,
            1 => f.dfg.inst_args_mut(copy.store)[0] = r.value,
            2 => f.dfg.inst_args_mut(r.load)[0] = host,
            3 => f.dfg.inst_args_mut(r.user)[0] = host,
            4 => copy.reloads.swap(0, 1),
            _ => {
                let last = f.layout.last_inst(f.layout.entry_block().unwrap()).unwrap();
                let mut c = FuncCursor::new(&mut f);
                c.goto_inst(last);
                match fault {
                    5 => {
                        c.ins().stack_addr(types::I64, copy.slot, 0);
                    }
                    6 => {
                        c.ins().store(flags(), host, r.address_value, 0);
                    }
                    7 => {
                        c.ins().iadd(r.value, host);
                    }
                    _ => {
                        c.ins().load(types::I64, flags(), copy.address_value, 0);
                    }
                }
            }
        }
        assert!(verify(&f, &copy).is_err(), "fault {fault}");
    }
}

#[test]
fn host_copy_rejects_unsupported_host_uses_without_mutation() {
    let (mut f, host) = fixture();
    let last = f.layout.last_inst(f.layout.entry_block().unwrap()).unwrap();
    let mut c = FuncCursor::new(&mut f);
    c.goto_inst(last);
    c.ins().iadd(host, host);
    let source = f.clone();
    assert!(lower(&mut f, host, allocator(4096), expansion()).is_err());
    assert_eq!(f, source);
}
