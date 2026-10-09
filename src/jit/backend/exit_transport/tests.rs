use super::*;
use cranelift_codegen::{ir::Signature, isa::CallConv, settings};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

fn fixture() -> Function {
    let mut function = Function::new();
    function.signature = Signature::new(CallConv::SystemV);
    function
        .signature
        .params
        .extend([types::I64, types::I64, types::I32, types::I64, types::I64].map(AbiParam::new));
    let mut context = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut function, &mut context);
    let entry = builder.create_block();
    builder.append_block_params_for_function_params(entry);
    builder.switch_to_block(entry);
    let params: [_; 5] = builder.block_params(entry).try_into().unwrap();
    let reason = builder
        .ins()
        .load(types::I32, MemFlagsData::new(), params[0], 0);
    for (value, offset) in [(params[1], 0), (params[2], 8), (reason, 12)] {
        builder
            .ins()
            .store(MemFlagsData::new(), value, params[3], offset);
    }
    builder.ins().return_(&[]);
    builder.seal_all_blocks();
    let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
        .unwrap()
        .finish(settings::Flags::new(settings::builder()))
        .unwrap();
    builder.finalize(isa.frontend_config());
    function
}

fn verify_ir(function: &Function) {
    cranelift_codegen::verify_function(function, &settings::Flags::new(settings::builder()))
        .unwrap();
}

#[test]
fn lowering_retains_values_host_and_calling_convention() {
    let mut function = fixture();
    verify_ir(&function);
    let entry = function.layout.entry_block().unwrap();
    let params = function.dfg.block_params(entry).to_vec();
    let (_, values) = tail(&function, entry, params[3]).unwrap();
    lower(&mut function).unwrap();
    verify_ir(&function);
    assert_eq!(
        function.dfg.block_params(entry),
        [params[0], params[1], params[2], params[4]]
    );
    assert_eq!(function.signature.call_conv, CallConv::SystemV);
    assert_eq!(function.signature.returns, [AbiParam::new(types::I64); 2]);
    assert!(function
        .layout
        .block_insts(entry)
        .all(|inst| function.dfg.insts[inst].opcode() != Opcode::Store));
    verify_tail(&function, entry, values).unwrap();
    let previous = function.clone();
    assert!(lower(&mut function).is_err());
    assert_eq!(function, previous);
}

#[test]
fn malformed_output_stores_and_escaping_pointer_are_rejected_without_changes() {
    let original = fixture();
    let entry = original.layout.entry_block().unwrap();
    let params = original.dfg.block_params(entry).to_vec();
    let (stores, _) = tail(&original, entry, params[3]).unwrap();
    for fault in 0..10 {
        let mut function = original.clone();
        match fault {
            0 => function.signature.params[2] = AbiParam::new(types::I64),
            1 => function.signature.returns.push(AbiParam::new(types::I64)),
            2 => function.layout.remove_inst(stores[1]),
            3..=6 => {
                let InstructionData::Store { args, offset, .. } =
                    &mut function.dfg.insts[stores[0]]
                else {
                    unreachable!()
                };
                match fault {
                    3 => args[1] = params[0],
                    4 => *offset = 8.into(),
                    5 => args[0] = params[2],
                    6 => args[0] = params[3],
                    _ => unreachable!(),
                }
            }
            7 => {
                let mut cursor = FuncCursor::new(&mut function);
                cursor.goto_inst(stores[0]);
                cursor
                    .ins()
                    .load(types::I64, MemFlagsData::new(), params[3], 0);
            }
            8 => {
                let mut cursor = FuncCursor::new(&mut function);
                cursor.goto_inst(stores[0]);
                cursor
                    .ins()
                    .store(MemFlagsData::new(), params[1], params[3], 0);
            }
            9 => {
                let flags = function
                    .dfg
                    .mem_flags
                    .insert(MemFlagsData::trusted())
                    .unwrap();
                let InstructionData::Store { flags: actual, .. } =
                    &mut function.dfg.insts[stores[0]]
                else {
                    unreachable!()
                };
                *actual = flags;
            }
            _ => unreachable!(),
        }
        let previous = function.clone();
        assert!(lower(&mut function).is_err(), "accepted fault {fault}");
        assert_eq!(function, previous, "modified fault {fault}");
    }
}

#[test]
fn return_checker_rejects_signed_extensions_packing_and_field_mutations() {
    let mut original = fixture();
    let entry = original.layout.entry_block().unwrap();
    let output = verify_input(&original).unwrap();
    let (_, values) = tail(&original, entry, output).unwrap();
    lower(&mut original).unwrap();
    let instructions: Vec<_> = original.layout.block_insts(entry).rev().take(6).collect();
    for fault in 0..7 {
        let mut function = original.clone();
        match fault {
            0 | 1 => {
                let InstructionData::Unary { opcode, .. } =
                    &mut function.dfg.insts[instructions[4 + fault]]
                else {
                    unreachable!()
                };
                *opcode = Opcode::Sextend;
            }
            2 => {
                let InstructionData::UnaryImm { imm, .. } =
                    &mut function.dfg.insts[instructions[3]]
                else {
                    unreachable!()
                };
                *imm = 31.into();
            }
            3 => {
                let InstructionData::Binary { opcode, .. } =
                    &mut function.dfg.insts[instructions[1]]
                else {
                    unreachable!()
                };
                *opcode = Opcode::Band;
            }
            4 => {
                let InstructionData::Binary { args, .. } = &mut function.dfg.insts[instructions[1]]
                else {
                    unreachable!()
                };
                args.swap(0, 1);
            }
            5 => function.dfg.inst_args_mut(instructions[0]).swap(0, 1),
            6 => {
                let InstructionData::Unary { arg, .. } = &mut function.dfg.insts[instructions[5]]
                else {
                    unreachable!()
                };
                *arg = values[2];
            }
            _ => unreachable!(),
        }
        assert!(
            verify_tail(&function, entry, values).is_err(),
            "accepted fault {fault}"
        );
    }
}
