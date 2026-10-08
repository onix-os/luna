use super::*;
use cranelift_codegen::ir::{InstructionData, Opcode};

pub(in crate::jit::backend) fn run(
    original: &Function,
    source: &Snapshot,
    headers: &[Block],
    exhausted: Block,
    expansion: super::super::super::work::Expansion,
) -> Result<(), JitError> {
    let plan = Plan::new(source).expect("eligible audit source");
    let mut candidate = original.clone();
    let region = emit(&mut candidate, source, headers, exhausted, false, plan)?;
    verify::check(
        original, &candidate, source, headers, exhausted, &region, false,
    )?;
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    let mut mutations = 0;
    let mut well_formed = 0;
    let mut reject = |changed: Function| {
        mutations += 1;
        well_formed += usize::from(cranelift_codegen::verify_function(&changed, &flags).is_ok());
        assert!(
            verify::check(original, &changed, source, headers, exhausted, &region, false).is_err(),
            "accepted mutated translation {mutations}"
        );
        let mut retained = original.clone();
        assert!(commit(
            &mut retained,
            changed,
            source,
            headers,
            exhausted,
            &region,
            false,
            expansion
        )
        .is_err());
        assert_eq!(&retained, original);
    };
    for block in candidate.layout.blocks() {
        for inst in candidate.layout.block_insts(block) {
            let mut changed = candidate.clone();
            let mutated = match &mut changed.dfg.insts[inst] {
                InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    imm,
                } => {
                    *imm = (i64::from(*imm) ^ 1).into();
                    true
                }
                InstructionData::UnaryIeee64 {
                    opcode: Opcode::F64const,
                    imm,
                } => {
                    *imm = cranelift_codegen::ir::immediates::Ieee64::with_bits(imm.bits() ^ 1);
                    true
                }
                InstructionData::Unary {
                    opcode: Opcode::FcvtFromSint,
                    arg,
                } => {
                    let arg = *arg;
                    changed.dfg.insts[inst] = InstructionData::Unary {
                        opcode: Opcode::FcvtFromUint,
                        arg,
                    };
                    true
                }
                InstructionData::Load { offset, .. } | InstructionData::Store { offset, .. } => {
                    *offset = (i32::from(*offset) + 8).into();
                    true
                }
                InstructionData::IntCompare { cond, .. } => {
                    *cond = if *cond == IntCC::Equal {
                        IntCC::NotEqual
                    } else {
                        IntCC::Equal
                    };
                    true
                }
                InstructionData::Brif { blocks, .. } => {
                    blocks.swap(0, 1);
                    true
                }
                InstructionData::Binary { opcode, .. } => {
                    let replacement = match *opcode {
                        Opcode::Iadd => Some(Opcode::Isub),
                        Opcode::Isub => Some(Opcode::Iadd),
                        Opcode::Imul => Some(Opcode::Iadd),
                        Opcode::Band => Some(Opcode::Bor),
                        Opcode::Bor => Some(Opcode::Band),
                        Opcode::Bxor => Some(Opcode::Band),
                        Opcode::SaddOverflow => Some(Opcode::UaddOverflow),
                        Opcode::Fadd => Some(Opcode::Fsub),
                        Opcode::Fsub | Opcode::Fmul => Some(Opcode::Fadd),
                        _ => None,
                    };
                    if let Some(replacement) = replacement {
                        *opcode = replacement;
                        true
                    } else {
                        false
                    }
                }
                InstructionData::Ternary {
                    opcode: Opcode::Select,
                    args,
                } => {
                    args.swap(1, 2);
                    args[1] != args[2]
                }
                _ => false,
            };
            if mutated {
                reject(changed);
            }
            if let InstructionData::Load { flags, .. } | InstructionData::Store { flags, .. } =
                candidate.dfg.insts[inst]
            {
                let mut changed = candidate.clone();
                let mut data = changed.dfg.mem_flags[flags];
                data.set_readonly();
                let replacement = changed.dfg.mem_flags.insert(data).unwrap();
                match &mut changed.dfg.insts[inst] {
                    InstructionData::Load { flags, .. } | InstructionData::Store { flags, .. } => {
                        *flags = replacement
                    }
                    _ => unreachable!(),
                }
                reject(changed);
            }
            if candidate.dfg.insts[inst].opcode() == Opcode::Store {
                let mut changed = candidate.clone();
                changed.layout.remove_inst(inst);
                reject(changed);
                let mut changed = candidate.clone();
                let InstructionData::Store { args, .. } = &mut changed.dfg.insts[inst] else {
                    unreachable!()
                };
                args.swap(0, 1);
                reject(changed);
            }
            if let InstructionData::BranchTable { table, .. } = candidate.dfg.insts[inst] {
                let mut changed = candidate.clone();
                let branches = changed.dfg.jump_tables[table].all_branches_mut();
                if branches.len() > 2 {
                    branches.swap(1, 2);
                    reject(changed);
                }
            }
            let destinations = candidate.dfg.insts[inst]
                .branch_destination(&candidate.dfg.jump_tables, &candidate.dfg.exception_tables);
            for (edge, call) in destinations.iter().enumerate() {
                let args: Vec<_> = call.args(&candidate.dfg.value_lists).collect();
                let pair = args.iter().enumerate().find_map(|(a, left)| {
                    args.iter().enumerate().skip(a + 1).find_map(|(b, right)| {
                        let (Some(l), Some(r)) = (left.as_value(), right.as_value()) else {
                            return None;
                        };
                        (l != r && candidate.dfg.value_type(l) == candidate.dfg.value_type(r))
                            .then_some((a, b))
                    })
                });
                if let Some((a, b)) = pair {
                    let mut changed = candidate.clone();
                    let mut call = changed.dfg.insts[inst].branch_destination(
                        &changed.dfg.jump_tables,
                        &changed.dfg.exception_tables,
                    )[edge];
                    let mut index = 0;
                    call.update_args(&mut changed.dfg.value_lists, |old| {
                        let value = if index == a {
                            args[b]
                        } else if index == b {
                            args[a]
                        } else {
                            old
                        };
                        index += 1;
                        value
                    });
                    reject(changed);
                }
            }
        }
    }
    let mut changed = candidate.clone();
    changed.signature.params[1].extension = cranelift_codegen::ir::ArgumentExtension::Sext;
    reject(changed);
    for (id, _) in candidate.dfg.ext_funcs.iter() {
        let mut changed = candidate.clone();
        changed.dfg.ext_funcs[id].colocated = !changed.dfg.ext_funcs[id].colocated;
        reject(changed);
    }
    let mut changed = candidate.clone();
    let target = block(&mut changed);
    let mut cursor = FuncCursor::new(&mut changed);
    cursor.goto_bottom(target);
    cursor.ins().return_(&[]);
    reject(changed);
    let mut changed = candidate.clone();
    let before = changed.layout.last_inst(region.done).unwrap();
    let mut cursor = FuncCursor::new(&mut changed);
    cursor.goto_inst(before);
    cursor.ins().iconst(types::I64, 99);
    reject(changed);
    assert!(
        mutations > 100 && well_formed > 100,
        "mutations={mutations} well_formed={well_formed}"
    );

    let length = plan.end - plan.start + 1;
    let instructions: usize = original
        .layout
        .blocks()
        .map(|b| original.layout.block_insts(b).count())
        .sum();
    let bound = super::super::super::work::Expansion {
        instructions: instructions * 2
            + 32
            + 9 * (plan.used.count_ones() as usize + length)
            + usize::from(plan.numbers != 0) * (2 + 2 * length),
        blocks: original.dfg.num_blocks() * 2 + 5 + length * 2,
    };
    for limits in [
        super::super::super::work::Expansion {
            instructions: bound.instructions - 1,
            ..bound
        },
        super::super::super::work::Expansion {
            blocks: bound.blocks - 1,
            ..bound
        },
    ] {
        let mut unchanged = original.clone();
        assert!(!augment(
            &mut unchanged,
            source,
            headers,
            exhausted,
            false,
            limits
        )?);
        assert_eq!(&unchanged, original);
    }
    let mut accepted = original.clone();
    assert!(augment(
        &mut accepted,
        source,
        headers,
        exhausted,
        false,
        bound
    )?);
    assert_eq!(accepted, candidate);
    for fault in 0..8 {
        let mut invalid_region = region.clone();
        match fault {
            0 => invalid_region.plan.written ^= 1 << plan.base,
            1 => invalid_region.plan.end = usize::MAX,
            2 => invalid_region.plan.start = usize::MAX,
            3 => invalid_region.plan.base = usize::MAX,
            4 => invalid_region.fast[0] = region.generic,
            5 => invalid_region.plan.used ^= 1 << plan.base,
            6 => invalid_region.plan.numbers ^= 1 << plan.base,
            _ => {
                invalid_region.plan.numbers ^=
                    1 << (plan.used & !(15 << plan.base)).trailing_zeros()
            }
        }
        assert!(verify::check(
            original,
            &candidate,
            source,
            headers,
            exhausted,
            &invalid_region,
            false
        )
        .is_err());
    }
    let mut changed_source = Snapshot {
        operations: super::super::super::resources::owned(&source.operations),
        constants: super::super::super::resources::owned(&source.constants),
        registers: source.registers,
        upvalues: source.upvalues,
        prototypes: source.prototypes,
    };
    let pc = (plan.start..plan.end)
        .find(|&pc| {
            matches!(
                source.operations[pc],
                Operation::Add { .. } | Operation::Sub { .. } | Operation::Mul { .. }
            )
        })
        .unwrap();
    changed_source.operations[pc] = match source.operations[pc] {
        Operation::Add { dest, left, right } => Operation::Sub { dest, left, right },
        Operation::Sub { dest, left, right } | Operation::Mul { dest, left, right } => {
            Operation::Add { dest, left, right }
        }
        _ => unreachable!(),
    };
    assert!(verify::check(
        original,
        &candidate,
        &changed_source,
        headers,
        exhausted,
        &region,
        false
    )
    .is_err());
    changed_source.operations[pc] = source.operations[pc];
    for operation in &source.operations[plan.start..plan.end] {
        let constant = |input| match input {
            RCIndex::Constant(index) => Some(usize::from(index.0)),
            RCIndex::Register(_) => None,
        };
        let inputs = match *operation {
            Operation::Add { left, right, .. }
            | Operation::Sub { left, right, .. }
            | Operation::Mul { left, right, .. } => [constant(left), constant(right)],
            Operation::LoadConstant { constant, .. } => [Some(usize::from(constant.0)), None],
            _ => [None, None],
        };
        for index in inputs.into_iter().flatten() {
            changed_source.constants[index].bits ^= 1;
            assert!(verify::check(
                original,
                &candidate,
                &changed_source,
                headers,
                exhausted,
                &region,
                false
            )
            .is_err());
            changed_source.constants[index] = source.constants[index];
        }
    }
    eprintln!("integer_loop_mutations={mutations} well_formed={well_formed}");
    Ok(())
}
