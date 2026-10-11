use super::*;
use cranelift_codegen::ir::{AbiParam, InstructionData, Opcode};

pub(in crate::jit::backend) fn fixture(
    floating: bool,
) -> (Function, Snapshot, Vec<Block>, Checkpoints) {
    let source = tests::snapshot(if floating {
        b"local s=0.0 for i=1,20 do s=s+0.5 end return s"
    } else {
        b"local s=0 for i=1,20 do s=s+i end return s"
    });
    let mut f = Function::new();
    f.dfg.mem_flags.insert(MemFlagsData::new()).unwrap();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature
        .params
        .extend([types::I64, types::I64, types::I32, types::I64, types::I64].map(AbiParam::new));
    let entry = block(&mut f);
    for ty in [types::I64, types::I64, types::I32, types::I64, types::I64] {
        f.dfg.append_block_param(entry, ty);
    }
    let headers: Vec<_> = source.operations.iter().map(|_| block(&mut f)).collect();
    for &header in &headers {
        f.dfg.append_block_param(header, types::I32);
    }
    let exhausted = block(&mut f);
    f.dfg.append_block_param(exhausted, types::I64);
    f.dfg.append_block_param(exhausted, types::I32);
    let mut cursor = FuncCursor::new(&mut f);
    cursor.goto_bottom(entry);
    let zero = cursor.ins().iconst(types::I32, 0);
    cursor.ins().jump(headers[0], &[zero.into()]);
    for &header in &headers {
        cursor.goto_bottom(header);
        cursor.ins().return_(&[]);
    }
    cursor.goto_bottom(exhausted);
    cursor.ins().return_(&[]);
    let checkpoints = augment_checkpoints(
        &mut f,
        &source,
        &headers,
        exhausted,
        super::super::super::work::Expansion {
            instructions: 100_000,
            blocks: 10_000,
        },
    )
    .unwrap()
    .unwrap();
    (f, source, headers, checkpoints)
}

#[test]
fn integer_checkpoints_bind_both_flushes_and_reject_changed_source_fields() {
    for floating in [false, true] {
        let (f, _, _, checkpoints) = fixture(floating);
        let r = &checkpoints.region;
        let mut checked = 0;
        for block in [r.done, r.budget_exit] {
            let params = f.dfg.block_params(block);
            let expected = if block == r.done {
                Checkpoint {
                    pc: CheckpointPc::Constant((r.plan.end + 1) as u64),
                    count: params[0],
                }
            } else {
                Checkpoint {
                    pc: CheckpointPc::Dynamic(params[0]),
                    count: params[1],
                }
            };
            let stores: Vec<_> = f.layout.block_insts(block).filter(|&inst| matches!(f.dfg.insts[inst], InstructionData::Store { offset, .. } if i32::from(offset) % 16 == 0)).collect();
            assert_eq!(stores.len(), r.plan.written.count_ones() as usize);
            for store in stores {
                assert_eq!(checkpoints.source(&f, store).unwrap(), Some(expected));
                checked += 1;
                for inst in f.layout.block_insts(block) {
                    let mut changed = f.clone();
                    let mut data = changed.dfg.insts[inst];
                    match &mut data {
                        InstructionData::UnaryImm { imm, .. } => {
                            *imm = (i64::from(*imm) ^ 1).into()
                        }
                        InstructionData::Store { offset, .. } => {
                            *offset = (i32::from(*offset) + 8).into()
                        }
                        InstructionData::Jump { destination, .. } => {
                            let entry = f.layout.entry_block().unwrap();
                            let budget = f.dfg.block_params(entry)[2];
                            let count = expected.count;
                            destination.update_args(&mut changed.dfg.value_lists, |arg| {
                                if arg.as_value() == Some(count) {
                                    budget.into()
                                } else {
                                    arg
                                }
                            });
                        }
                        _ => panic!("unexpected checkpoint instruction"),
                    }
                    changed.dfg.insts[inst] = data;
                    cranelift_codegen::verify_function(
                        &changed,
                        &cranelift_codegen::settings::Flags::new(
                            cranelift_codegen::settings::builder(),
                        ),
                    )
                    .unwrap();
                    assert!(checkpoints.source(&changed, store).is_err());
                }
                let mut changed = f.clone();
                changed.layout.remove_inst(store);
                assert!(checkpoints.source(&changed, store).is_err());
            }
            let non_store = f.layout.first_inst(block).unwrap();
            assert_ne!(f.dfg.insts[non_store].opcode(), Opcode::Store);
            assert!(checkpoints.source(&f, non_store).is_err());
        }
        assert!(checked > 0);
        let original = f.layout.first_inst(r.generic).unwrap();
        assert_eq!(checkpoints.source(&f, original).unwrap(), None);
    }
}
