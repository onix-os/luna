use super::*;
use crate::jit::preds::Predecessors;

fn allocator(limit: usize) -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(limit))
}

fn check(f: &Function, read: &Read, slots: IrValue) -> Result<(), JitError> {
    cranelift_codegen::verify_function(f, &settings::Flags::new(settings::builder())).unwrap();
    let predecessors = Predecessors::new(f, allocator(1 << 20)).unwrap();
    verify_contract(f, read, slots, Contract::FrameBound, Some(&predecessors))
}

fn fixture(ty: cranelift_codegen::ir::Type) -> (Function, Read, IrValue) {
    let (mut f, inst, slots) = typed_source_fixture(ty);
    let read = emit_contract(&mut f, inst, slots, 0, Contract::FrameBound);
    check(&f, &read, slots).unwrap();
    (f, read, slots)
}

#[test]
fn bound_scalar_reads_keep_contracts_expansion_and_quota_cleanup() {
    for ty in [types::I64, types::F64] {
        let (f, read, slots) = fixture(ty);
        assert!(verify(&f, &read, slots).is_err());
        assert!(verify_contract(&f, &read, slots, Contract::FrameBound, None).is_err());
        let (checked, checked_read, checked_slots) = typed_fixture(ty);
        assert!(check(&checked, &checked_read, checked_slots).is_err());
        assert_eq!(size(&f), (size(&checked).0 - 2, size(&checked).1));
        let (source, _, slots) = typed_source_fixture(ty);
        for contract in [Contract::Checked, Contract::FrameBound] {
            let lower = |f: &mut Function, memory| {
                lower_contract(
                    f,
                    slots,
                    slots,
                    1,
                    &[],
                    memory,
                    crate::jit::work::Expansion {
                        instructions: 4096,
                        blocks: 4096,
                    },
                    contract,
                )
            };
            let mut f = source.clone();
            let memory = allocator(1 << 20);
            lower(&mut f, memory.clone()).unwrap();
            let extra = if contract == Contract::FrameBound {
                17
            } else {
                19
            };
            assert_eq!(
                size(&f),
                (
                    size(&source).0 + extra + usize::from(ty == types::F64),
                    size(&source).1 + 5
                )
            );
            assert_eq!(memory.0.current(), 0);
            let peak = memory.0.peak();
            for limit in [0, peak - 1, peak] {
                let memory = allocator(limit);
                let mut candidate = source.clone();
                assert_eq!(lower(&mut candidate, memory.clone()).is_ok(), limit == peak);
                assert_eq!(memory.0.current(), 0);
                if limit < peak {
                    assert_eq!(candidate, source);
                }
            }
        }
    }
}

#[test]
fn bound_scalar_checker_rejects_guard_width_pointer_and_incoming_edge_mutations() {
    for ty in [types::I64, types::F64] {
        let (f, read, slots) = fixture(ty);
        let [before, after, wide, other, boolean, zero] = read.blocks;
        let head: Vec<_> = f.layout.block_insts(before).collect();
        let integer = f.dfg.first_result(head[3]);
        assert_eq!(head.len(), 6);
        assert_eq!(
            head.iter()
                .filter(|&&inst| f.dfg.insts[inst].opcode() == Opcode::Load)
                .count(),
            1
        );
        let mut rejected = 0;
        let mut reject = |changed: Function| {
            assert!(check(&changed, &read, slots).is_err());
            rejected += 1;
        };
        for block in [before, wide, other, boolean, zero] {
            for inst in f.layout.block_insts(block) {
                let mut changed = f.clone();
                let dfg = &mut changed.dfg;
                match &mut dfg.insts[inst] {
                    InstructionData::Load { offset, .. } => {
                        *offset = (i32::from(*offset) + 1).into()
                    }
                    InstructionData::UnaryImm { imm, .. } => *imm = (i64::from(*imm) ^ 1).into(),
                    InstructionData::IntCompare { cond, .. } => *cond = IntCC::NotEqual,
                    InstructionData::Binary { opcode, .. } => *opcode = Opcode::Bxor,
                    InstructionData::Brif { blocks, .. } => blocks.swap(0, 1),
                    InstructionData::Unary { opcode, .. } => *opcode = Opcode::Sextend,
                    InstructionData::Jump { destination, .. } => {
                        destination.update_args(&mut dfg.value_lists, |_| BlockArg::Value(integer));
                    }
                    _ => panic!("unhandled bound read instruction"),
                }
                reject(changed);
                if let InstructionData::Load { .. } = f.dfg.insts[inst] {
                    let mut changed = f.clone();
                    let mut flags = MemFlagsData::new();
                    flags.set_readonly();
                    let flags = changed.dfg.mem_flags.insert(flags).unwrap();
                    let InstructionData::Load { flags: actual, .. } = &mut changed.dfg.insts[inst]
                    else {
                        unreachable!()
                    };
                    *actual = flags;
                    reject(changed);
                }
            }
        }
        for block in [wide, boolean] {
            let body: Vec<_> = f.layout.block_insts(block).collect();
            for inst in &body[..2] {
                let mut changed = f.clone();
                changed.dfg.inst_args_mut(*inst)[0] = integer;
                reject(changed);
            }
        }
        let body: Vec<_> = f.layout.block_insts(boolean).collect();
        let mut changed = f.clone();
        let byte = changed.dfg.first_result(body[1]);
        let wide_value = changed.dfg.replace_result(byte, types::I64);
        changed.replace(body[2]).iadd(wide_value, integer);
        reject(changed);
        let mut changed = f.clone();
        let end = changed.layout.last_inst(wide).unwrap();
        let mut c = FuncCursor::new(&mut changed);
        c.goto_inst(end);
        c.ins().store(MemFlagsData::new(), integer, slots, 0);
        reject(changed);
        let mut changed = f.clone();
        changed.replace(head[1]).iconst(types::I64, -6);
        reject(changed);
        let mut changed = f.clone();
        changed.dfg.inst_args_mut(head[2])[0] = slots;
        reject(changed);
        for target in [wide, other, boolean, zero, after] {
            let mut changed = f.clone();
            let end = changed.layout.last_inst(after).unwrap();
            if target == after {
                changed.replace(end).jump(target, &[read.result.into()]);
            } else {
                changed.replace(end).jump(target, &[]);
            }
            reject(changed);
        }
        if let Some((cast, _)) = read.cast {
            let mut changed = f.clone();
            changed
                .replace(cast)
                .fcvt_from_sint(types::F64, read.result);
            reject(changed);
        }
        assert_eq!(rejected, 36 + usize::from(ty == types::F64));
    }
}

#[cfg(not(miri))]
#[test]
fn bound_scalar_native_reads_preserve_every_canonical_value_kind() {
    for ty in [types::I64, types::F64] {
        let (f, _, _) = fixture(ty);
        crate::jit::backend::projection_probe::with_projection_probe(f, |pointer| {
            let entry = unsafe {
                std::mem::transmute::<
                    *const u8,
                    unsafe extern "C" fn(*const crate::jit::abi::payload::Payload) -> u64,
                >(pointer)
            };
            crate::jit::abi::payload::runtime::check_native_bound_scalar_read(entry);
        })
        .unwrap();
    }
}
