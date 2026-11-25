use cranelift_codegen::{
    cursor::{Cursor, FuncCursor},
    ir::BlockCall,
};

use super::*;

#[test]
fn typed_ir_checker_rejects_well_formed_semantic_mutations() {
    let source = super::super::super::tests::sample();
    let plan = Plan::new(&source, 0, 6).unwrap();
    let mut rejected = 0;
    Code::inspect(&source, &plan, |function, isa| {
        let mut reject = |changed: Function| {
            cranelift_codegen::verify_function(&changed, isa).unwrap();
            assert!(
                verify(&source, &plan, &changed, isa).is_err(),
                "accepted mutated IR:\n{changed}"
            );
            rejected += 1;
        };
        let entry = function.layout.entry_block().unwrap();
        let arguments = function.dfg.block_params(entry);
        for block in function.layout.blocks() {
            for inst in function.layout.block_insts(block) {
                let mut changed = function.clone();
                match &mut changed.dfg.insts[inst] {
                    InstructionData::UnaryImm { imm, .. } => *imm = (i64::from(*imm) ^ 1).into(),
                    InstructionData::Load { offset, .. }
                    | InstructionData::Store { offset, .. } => {
                        *offset = (i32::from(*offset) + 8).into();
                    }
                    InstructionData::IntCompare { cond, .. } => {
                        *cond = IntCC::SignedLessThanOrEqual
                    }
                    InstructionData::Binary { opcode, .. } => *opcode = Opcode::Bxor,
                    InstructionData::Unary {
                        opcode: Opcode::Ineg,
                        arg,
                    } => *arg = arguments[0],
                    InstructionData::Brif { blocks, .. } => blocks.swap(0, 1),
                    InstructionData::CallIndirect { .. } => {
                        changed.dfg.inst_args_mut(inst)[0] = arguments[0]
                    }
                    InstructionData::Jump { .. } => continue,
                    InstructionData::MultiAry {
                        opcode: Opcode::Return,
                        ..
                    } => continue,
                    other => panic!("missing mutation: {other:?}"),
                }
                reject(changed);
                match function.dfg.insts[inst] {
                    InstructionData::Store { .. } => {
                        let mut changed = function.clone();
                        changed.dfg.inst_args_mut(inst)[1] = arguments[0];
                        reject(changed);
                        let mut changed = function.clone();
                        changed.dfg.inst_args_mut(inst)[0] = arguments[1];
                        reject(changed);
                    }
                    InstructionData::CallIndirect { sig_ref, .. } => {
                        for position in [1, 2, 3] {
                            let mut changed = function.clone();
                            changed.dfg.inst_args_mut(inst)[position] = arguments[0];
                            reject(changed);
                        }
                        let mut changed = function.clone();
                        changed.dfg.signatures[sig_ref].call_conv =
                            cranelift_codegen::isa::CallConv::Fast;
                        reject(changed);
                    }
                    InstructionData::Load { .. } => {
                        let mut changed = function.clone();
                        changed.dfg.inst_args_mut(inst)[0] = arguments[2];
                        reject(changed);
                    }
                    InstructionData::IntCompare { .. } => {
                        let mut changed = function.clone();
                        changed.dfg.inst_args_mut(inst).swap(0, 1);
                        reject(changed);
                    }
                    _ => {}
                }
            }
        }
        let blocks: std::vec::Vec<_> = function.layout.blocks().collect();
        for block in &blocks[1..blocks.len() - 2] {
            let mut changed = function.clone();
            let inst = changed.layout.last_inst(*block).unwrap();
            let target = BlockCall::new(*block, [], &mut changed.dfg.value_lists);
            let InstructionData::Brif {
                blocks: branches, ..
            } = &mut changed.dfg.insts[inst]
            else {
                panic!()
            };
            branches[1] = target;
            reject(changed);
        }
        let mut changed = function.clone();
        let first = changed.layout.first_inst(entry).unwrap();
        let mut cursor = FuncCursor::new(&mut changed);
        cursor.goto_inst(first);
        cursor
            .ins()
            .load(types::I64, MemFlagsData::new(), arguments[0], 0);
        reject(changed);
        let mut changed = function.clone();
        let block = blocks[1];
        let store = changed
            .layout
            .block_insts(block)
            .find(|&inst| {
                matches!(
                    changed.dfg.insts[inst].opcode(),
                    Opcode::Store | Opcode::CallIndirect
                )
            })
            .unwrap();
        let mut cursor = FuncCursor::new(&mut changed);
        cursor.goto_inst(store);
        cursor
            .ins()
            .store(MemFlagsData::new(), arguments[0], arguments[0], 0);
        reject(changed);
    })
    .unwrap();
    assert!(rejected >= 70, "only {rejected} mutations");
}

#[test]
fn typed_ir_checker_refuses_changed_source_abi_and_unbounded_ir() {
    let mut source = super::super::super::tests::sample();
    let plan = Plan::new(&source, 0, 6).unwrap();
    Code::inspect(&source, &plan, |function, isa| {
        let mut changed = function.clone();
        changed.signature.call_conv = cranelift_codegen::isa::CallConv::Fast;
        assert!(verify(&source, &plan, &changed, isa).is_err());
        let mut changed = function.clone();
        changed.signature.params[3].value_type = types::I64;
        assert!(verify(&source, &plan, &changed, isa).is_err());
        let mut changed = function.clone();
        changed.dfg.make_block();
        assert!(verify(&source, &plan, &changed, isa).is_err());
        let mut changed = function.clone();
        let first = changed
            .layout
            .first_inst(changed.layout.entry_block().unwrap())
            .unwrap();
        let mut cursor = FuncCursor::new(&mut changed);
        cursor.goto_inst(first);
        for _ in 0..160 {
            cursor.ins().iconst(types::I64, 0);
        }
        assert!(verify(&source, &plan, &changed, isa).is_err());
    })
    .unwrap();
    let mut saved = None;
    Code::inspect(&source, &plan, |function, _| {
        saved = Some(function.clone());
    })
    .unwrap();
    source.constants[0].bits ^= 1;
    let changed = Plan::new(&source, 0, 6).unwrap();
    Code::inspect(&source, &changed, |_, isa| {
        assert!(verify(&source, &changed, saved.as_ref().unwrap(), isa).is_err());
    })
    .unwrap();
}
