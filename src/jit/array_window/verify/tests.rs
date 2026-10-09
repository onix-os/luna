use super::*;
use crate::{Closure, Lua};
use cranelift_codegen::{
    cursor::{Cursor, FuncCursor},
    ir::{ArgumentExtension, InstBuilder},
    settings,
};

const PROGRAMS: &[&str] = &[
    "local t={} for i=1,100 do t[i]=i end return t",
    "local t={} local sum=0 for i=1,100 do sum=sum+t[i] end return sum",
    "local t={} for i=1,100 do t[i]=(t[i]+3)*2-1 end return t",
    "local t={} for i=1,100 do t[i]=false end return t",
    "local t={} for i=1,100 do t[i]=t[i]+0.5 end return t",
    "local t={} for i=1,100 do local value=i t[i]=value end return t",
    "local t={} for i=1,100 do local value=2147483647 t[i]=value end return t",
];

fn fixtures(mut test: impl FnMut(&mut Snapshot, Plan, &Function, &dyn TargetIsa)) {
    let isa = cranelift_native::builder()
        .unwrap()
        .finish(settings::Flags::new(settings::builder()))
        .unwrap();
    for text in PROGRAMS {
        Lua::empty().enter(|ctx| {
            let closure = Closure::load(ctx, None, text.as_bytes()).unwrap();
            let mut source = Snapshot::new(&closure.prototype(), 4096, 1024 * 1024).unwrap();
            let plan = (0..source.operations.len())
                .find_map(|end| Plan::new(&source, end))
                .unwrap();
            let function = super::super::kernel::program(&source, plan, isa.as_ref())
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            test(&mut source, plan, &function, isa.as_ref());
        });
    }
}

#[test]
fn source_bound_checker_accepts_supported_loop_shapes() {
    fixtures(|source, plan, function, isa| verify(source, plan, function, isa).unwrap());
}

#[test]
fn rejects_well_typed_changes_to_guards_effects_arithmetic_and_control() {
    let mut mutations = 0;
    fixtures(|source, plan, original, isa| {
        let mut reject = |changed: Function| {
            cranelift_codegen::verify_function(&changed, isa)
                .unwrap_or_else(|error| panic!("invalid mutation {mutations}: {error}"));
            assert!(
                verify(source, plan, &changed, isa).is_err(),
                "accepted mutation {mutations}: {}",
                changed.display()
            );
            mutations += 1;
        };
        for block in original.layout.blocks() {
            for inst in original.layout.block_insts(block) {
                let mut changed = original.clone();
                let mutated = match &mut changed.dfg.insts[inst] {
                    InstructionData::UnaryImm { imm, .. } => {
                        *imm = (imm.bits() ^ 1).into();
                        true
                    }
                    InstructionData::Load { offset, .. }
                    | InstructionData::Store { offset, .. } => {
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
                    InstructionData::Unary {
                        opcode: opcode @ Opcode::FcvtFromSint,
                        ..
                    } => {
                        *opcode = Opcode::FcvtFromUint;
                        true
                    }
                    InstructionData::Binary { opcode, .. } => {
                        *opcode = match *opcode {
                            Opcode::Iadd => Opcode::Isub,
                            Opcode::Isub | Opcode::Imul => Opcode::Iadd,
                            Opcode::Band => Opcode::Bor,
                            Opcode::Bor | Opcode::Bxor => Opcode::Band,
                            Opcode::Ishl => Opcode::Ushr,
                            Opcode::Umin => Opcode::Umax,
                            Opcode::SaddOverflow => Opcode::UaddOverflow,
                            Opcode::Fadd => Opcode::Fsub,
                            Opcode::Fsub | Opcode::Fmul => Opcode::Fadd,
                            other => panic!("untested binary opcode {other}"),
                        };
                        true
                    }
                    InstructionData::Ternary {
                        opcode: Opcode::Select,
                        args,
                    } => {
                        args.swap(1, 2);
                        assert_ne!(args[1], args[2]);
                        true
                    }
                    _ => false,
                };
                if mutated {
                    reject(changed);
                }

                if let InstructionData::Load { flags, .. }
                | InstructionData::Store { flags, .. }
                | InstructionData::LoadNoOffset { flags, .. } = original.dfg.insts[inst]
                {
                    let mut changed = original.clone();
                    let mut data = changed.dfg.mem_flags[flags];
                    if original.dfg.insts[inst].opcode() == Opcode::Bitcast {
                        data = data.with_endianness(cranelift_codegen::ir::Endianness::Big);
                    } else {
                        data.set_notrap();
                    }
                    let replacement = changed.dfg.mem_flags.insert(data).unwrap();
                    match &mut changed.dfg.insts[inst] {
                        InstructionData::Load { flags, .. }
                        | InstructionData::Store { flags, .. }
                        | InstructionData::LoadNoOffset { flags, .. } => *flags = replacement,
                        _ => unreachable!(),
                    }
                    reject(changed);
                }
                if original.dfg.insts[inst].opcode() == Opcode::Store {
                    let mut changed = original.clone();
                    changed.layout.remove_inst(inst);
                    reject(changed);
                }
                let arguments = original.dfg.inst_args(inst);
                for (index, &value) in arguments.iter().enumerate() {
                    let replacement = arguments.iter().copied().find(|&other| {
                        other != value
                            && original.dfg.value_type(other) == original.dfg.value_type(value)
                    });
                    if let Some(replacement) = replacement {
                        let mut changed = original.clone();
                        changed.dfg.inst_args_mut(inst)[index] = replacement;
                        reject(changed);
                    }
                }
                let destinations = original.dfg.insts[inst]
                    .branch_destination(&original.dfg.jump_tables, &original.dfg.exception_tables);
                for (edge, call) in destinations.iter().enumerate() {
                    let args: Vec<_> = call.args(&original.dfg.value_lists).collect();
                    for (index, argument) in args.iter().enumerate() {
                        let value = argument.as_value().unwrap();
                        let entry = original.layout.entry_block().unwrap();
                        let zero = original
                            .dfg
                            .first_result(original.layout.first_inst(entry).unwrap());
                        let replacement =
                            args.iter()
                                .copied()
                                .chain([BlockArg::Value(zero)])
                                .find(|other| {
                                    let other = other.as_value().unwrap();
                                    other != value
                                        && original.dfg.value_type(other)
                                            == original.dfg.value_type(value)
                                });
                        if let Some(replacement) = replacement {
                            let mut changed = original.clone();
                            let mut call = changed.dfg.insts[inst].branch_destination(
                                &changed.dfg.jump_tables,
                                &changed.dfg.exception_tables,
                            )[edge];
                            let mut position = 0;
                            call.update_args(&mut changed.dfg.value_lists, |old| {
                                let result = if position == index { replacement } else { old };
                                position += 1;
                                result
                            });
                            reject(changed);
                        }
                    }
                }
            }
        }
        let mut changed = original.clone();
        changed.signature.params[2].extension = ArgumentExtension::Uext;
        reject(changed);
        let mut changed = original.clone();
        let block = changed.dfg.make_block();
        changed.layout.append_block(block);
        let mut cursor = FuncCursor::new(&mut changed);
        cursor.goto_bottom(block);
        cursor.ins().return_(&[]);
        reject(changed);
        let mut changed = original.clone();
        let last = changed.layout.last_block().unwrap();
        let before = changed.layout.last_inst(last).unwrap();
        let mut cursor = FuncCursor::new(&mut changed);
        cursor.goto_inst(before);
        cursor.ins().iconst(types::I64, 99);
        reject(changed);
    });
    assert!(mutations > 2000, "only {mutations} mutations");
    eprintln!("array_translation_well_typed_mutations={mutations}");
}

#[test]
fn rejects_changed_source_and_forged_plans() {
    fixtures(|source, plan, function, isa| {
        for forged in [
            Plan {
                start: plan.start + 1,
                ..plan
            },
            Plan {
                end: plan.end - 1,
                ..plan
            },
            Plan {
                base: plan.base.wrapping_add(1),
                ..plan
            },
            Plan {
                table: plan.table.wrapping_add(1),
                ..plan
            },
            Plan {
                access: if plan.access == Access::Read {
                    Access::Write
                } else {
                    Access::Read
                },
                ..plan
            },
            Plan {
                start: usize::MAX,
                ..plan
            },
            Plan {
                end: usize::MAX,
                ..plan
            },
        ] {
            assert!(verify(source, forged, function, isa).is_err());
        }
        let Operation::NumericForLoop { jump, .. } = &mut source.operations[plan.end] else {
            unreachable!()
        };
        *jump += 1;
        assert!(verify(source, plan, function, isa).is_err());
    });
}
