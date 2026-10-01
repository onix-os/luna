use cranelift_codegen::ir::{
    types, AbiParam, Block, BlockArg, Function, InstructionData, MemFlagsData, Opcode,
};
#[cfg(test)]
use cranelift_codegen::ir::{Inst, Value};

use super::{abi::Exit, JitError};

fn invalid() -> JitError {
    JitError::Compilation("invalid shared exit data flow".into())
}

pub(super) fn verify(function: &Function, handlers: &[Block; 4]) -> Result<(), JitError> {
    let entry = function.layout.entry_block().ok_or_else(invalid)?;
    let parameters = function.dfg.block_params(entry);
    if parameters.len() != 5
        || function.signature.params.iter().copied().ne([
            types::I64,
            types::I64,
            types::I32,
            types::I64,
            types::I64,
        ]
        .map(AbiParam::new))
        || !function.signature.returns.is_empty()
    {
        return Err(invalid());
    }
    let output = function.dfg.resolve_aliases(parameters[3]);
    let mut returns = [None; 4];
    for (reason, &block) in handlers.iter().enumerate() {
        if block == entry || handlers[..reason].contains(&block) {
            return Err(invalid());
        }
        let params = function.dfg.block_params(block);
        if params.len() != 2
            || function.dfg.value_type(params[0]) != types::I64
            || function.dfg.value_type(params[1]) != types::I32
        {
            return Err(invalid());
        }
        let mut instructions = function.layout.block_insts(block);
        let reason_inst = instructions.next().ok_or_else(invalid)?;
        let InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } = function.dfg.insts[reason_inst]
        else {
            return Err(invalid());
        };
        let reason_value = function.dfg.inst_results(reason_inst);
        if imm.bits() != reason as i64
            || reason_value.len() != 1
            || function.dfg.value_type(reason_value[0]) != types::I32
        {
            return Err(invalid());
        }
        for (value, offset) in [
            (params[0], std::mem::offset_of!(Exit, pc)),
            (params[1], std::mem::offset_of!(Exit, instructions)),
            (reason_value[0], std::mem::offset_of!(Exit, reason)),
        ] {
            let inst = instructions.next().ok_or_else(invalid)?;
            let InstructionData::Store {
                opcode: Opcode::Store,
                args,
                flags,
                offset: actual,
            } = function.dfg.insts[inst]
            else {
                return Err(invalid());
            };
            if function.dfg.resolve_aliases(args[0]) != function.dfg.resolve_aliases(value)
                || function.dfg.resolve_aliases(args[1]) != output
                || i32::from(actual) != offset as i32
                || function.dfg.mem_flags[flags] != MemFlagsData::new()
            {
                return Err(invalid());
            }
        }
        let terminal = instructions.next().ok_or_else(invalid)?;
        if function.dfg.insts[terminal].opcode() != Opcode::Return
            || !function.dfg.inst_args(terminal).is_empty()
            || instructions.next().is_some()
        {
            return Err(invalid());
        }
        returns[reason] = Some(terminal);
    }
    let mut found = 0;
    for block in function.layout.blocks() {
        for inst in function.layout.block_insts(block) {
            if function.dfg.insts[inst].opcode() == Opcode::Return {
                if !returns.contains(&Some(inst)) {
                    return Err(invalid());
                }
                found += 1;
            }
            if function
                .dfg
                .inst_args(inst)
                .iter()
                .any(|&value| function.dfg.resolve_aliases(value) == output)
                && !handlers.contains(&block)
            {
                return Err(invalid());
            }
            for branch in function.dfg.insts[inst]
                .branch_destination(&function.dfg.jump_tables, &function.dfg.exception_tables)
            {
                if branch.args(&function.dfg.value_lists).any(|arg| {
                    matches!(arg, BlockArg::Value(value)
                        if function.dfg.resolve_aliases(value) == output)
                }) {
                    return Err(invalid());
                }
            }
        }
    }
    if found != 4 {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Fault {
    Pc,
    PcWidth,
    Count,
    CountWidth,
    Reason,
    Base,
    Offset,
    Flags,
    ExtraStore,
    MissingStore,
    StoreOrder,
    Return,
    ExtraReturn,
    OutputUse,
    OutputBranch,
    DuplicateHandler,
    Signature,
}

#[cfg(test)]
pub(super) fn corrupt(function: &mut Function, handlers: &mut [Block; 4], fault: Fault) {
    use cranelift_codegen::{
        cursor::{Cursor, FuncCursor},
        ir::{BlockCall, InstBuilder},
    };
    use Fault::*;
    let entry = function.layout.entry_block().unwrap();
    let output = function.dfg.block_params(entry)[3];
    let slots = function.dfg.block_params(entry)[0];
    let entry_pc = function.dfg.block_params(entry)[1];
    let block = handlers[0];
    let params: [Value; 2] = function.dfg.block_params(block).try_into().unwrap();
    let instructions: [Inst; 5] = function
        .layout
        .block_insts(block)
        .collect::<std::vec::Vec<_>>()
        .try_into()
        .unwrap();
    match fault {
        Pc | PcWidth | Count | CountWidth | Base | Offset | Flags => {
            let inst = instructions[if matches!(fault, Count | CountWidth) {
                2
            } else {
                1
            }];
            let reason = function.dfg.inst_results(instructions[0])[0];
            let flags = (fault == Flags).then(|| {
                function
                    .dfg
                    .mem_flags
                    .insert(MemFlagsData::trusted())
                    .unwrap()
            });
            let InstructionData::Store {
                args,
                offset,
                flags: current,
                ..
            } = &mut function.dfg.insts[inst]
            else {
                unreachable!()
            };
            match fault {
                Pc => args[0] = entry_pc,
                PcWidth => args[0] = params[1],
                Count => args[0] = reason,
                CountWidth => args[0] = params[0],
                Base => args[1] = slots,
                Offset => *offset = 1.into(),
                Flags => *current = flags.unwrap(),
                _ => unreachable!(),
            }
        }
        Reason => {
            let InstructionData::UnaryImm { imm, .. } = &mut function.dfg.insts[instructions[0]]
            else {
                unreachable!()
            };
            *imm = 99.into();
        }
        ExtraStore => {
            let mut cursor = FuncCursor::new(function);
            cursor.goto_inst(instructions[4]);
            cursor
                .ins()
                .store(MemFlagsData::new(), params[0], output, 0);
        }
        MissingStore => {
            function.layout.remove_inst(instructions[1]);
        }
        StoreOrder => {
            let first = function.dfg.insts[instructions[1]];
            function.dfg.insts[instructions[1]] = function.dfg.insts[instructions[2]];
            function.dfg.insts[instructions[2]] = first;
        }
        Return => {
            let destination = BlockCall::new(
                handlers[1],
                [params[0].into(), params[1].into()],
                &mut function.dfg.value_lists,
            );
            function.dfg.insts[instructions[4]] = InstructionData::Jump {
                opcode: Opcode::Jump,
                destination,
            };
        }
        ExtraReturn => {
            let block = function.dfg.make_block();
            function.layout.append_block(block);
            let mut cursor = FuncCursor::new(function);
            cursor.goto_bottom(block);
            cursor.ins().return_(&[]);
        }
        OutputUse => {
            let terminal = function.layout.last_inst(entry).unwrap();
            let mut cursor = FuncCursor::new(function);
            cursor.goto_inst(terminal);
            cursor
                .ins()
                .load(types::I64, MemFlagsData::new(), output, 0);
        }
        OutputBranch => {
            let terminal = function.layout.last_inst(entry).unwrap();
            let destination = BlockCall::new(
                handlers[0],
                [output.into(), function.dfg.block_params(entry)[2].into()],
                &mut function.dfg.value_lists,
            );
            function.dfg.insts[terminal] = InstructionData::Jump {
                opcode: Opcode::Jump,
                destination,
            };
        }
        DuplicateHandler => handlers[1] = handlers[0],
        Signature => {
            function.signature.params[2] = AbiParam::new(types::I32).uext();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::{ir::InstBuilder, isa::CallConv};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

    fn fixture(fault: Option<Fault>) -> Result<(), JitError> {
        let mut function = Function::new();
        function.signature.call_conv = CallConv::SystemV;
        function.signature.params.extend(
            [types::I64, types::I64, types::I32, types::I64, types::I64].map(AbiParam::new),
        );
        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut function, &mut context);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let parameters: [Value; 5] = builder.block_params(entry).try_into().unwrap();
        let mut handlers = std::array::from_fn(|_| builder.create_block());
        for &block in &handlers {
            builder.append_block_param(block, types::I64);
            builder.append_block_param(block, types::I32);
        }
        builder
            .ins()
            .jump(handlers[0], &[parameters[1].into(), parameters[2].into()]);
        for (reason, &block) in handlers.iter().enumerate() {
            builder.switch_to_block(block);
            let pc = builder.block_params(block)[0];
            let count = builder.block_params(block)[1];
            let reason = builder.ins().iconst(types::I32, reason as i64);
            for (value, offset) in [(pc, 0), (count, 8), (reason, 12)] {
                builder
                    .ins()
                    .store(MemFlagsData::new(), value, parameters[3], offset);
            }
            builder.ins().return_(&[]);
        }
        builder.seal_all_blocks();
        builder.finalize(
            cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
                .unwrap()
                .finish(cranelift_codegen::settings::Flags::new(
                    cranelift_codegen::settings::builder(),
                ))
                .unwrap()
                .frontend_config(),
        );
        if let Some(fault) = fault {
            corrupt(&mut function, &mut handlers, fault);
        }
        cranelift_codegen::verify_function(
            &function,
            &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
        )
        .unwrap();
        verify(&function, &handlers)
    }

    #[test]
    fn canonical_handlers_match_entry_abi_and_exit_fields() {
        fixture(None).unwrap();
    }

    macro_rules! rejected {
        ($name:ident, $fault:ident) => {
            #[test]
            fn $name() {
                let result = fixture(Some(Fault::$fault));
                assert!(matches!(result, Err(JitError::Compilation(ref message))
                    if message == "invalid shared exit data flow"), "{result:?}");
            }
        };
    }

    rejected!(pc_value_is_checked, Pc);
    rejected!(pc_width_is_checked, PcWidth);
    rejected!(count_value_is_checked, Count);
    rejected!(count_width_is_checked, CountWidth);
    rejected!(reason_value_is_checked, Reason);
    rejected!(output_pointer_is_checked, Base);
    rejected!(field_offset_is_checked, Offset);
    rejected!(memory_flags_are_checked, Flags);
    rejected!(extra_stores_are_refused, ExtraStore);
    rejected!(missing_stores_are_refused, MissingStore);
    rejected!(store_order_is_checked, StoreOrder);
    rejected!(handler_must_return, Return);
    rejected!(unrecorded_returns_are_refused, ExtraReturn);
    rejected!(output_access_outside_handlers_is_refused, OutputUse);
    rejected!(
        output_pointer_cannot_escape_in_branch_arguments,
        OutputBranch
    );
    rejected!(duplicate_handler_identity_is_refused, DuplicateHandler);
    rejected!(entry_signature_extensions_are_checked, Signature);
}
