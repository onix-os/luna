use super::*;
use cranelift_codegen::ir::Type;

const _: () = {
    use crate::jit::abi::payload::packed::*;
    assert!(
        MASK == 3 && INT == 0 && FLOAT == 1 && FALSE == 2 && TRUE == 3 && EMPTY == 4 && OPAQUE == 5
    );
    assert!(
        abi::NIL == 0
            && abi::BOOLEAN == 1
            && abi::INTEGER == 2
            && abi::NUMBER == 3
            && abi::REFERENCE == 4
    );
    assert!(std::mem::size_of::<Packed>() == 8);
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Tag,
    Bits(Type),
}

#[derive(Clone, Copy)]
pub(super) struct Input {
    inst: Inst,
    slots: IrValue,
    index: u32,
    result: IrValue,
    kind: Kind,
}

pub(super) enum Read {
    Tag(Input),
    Bits {
        input: Input,
        blocks: [Block; 4],
        parameter: IrValue,
    },
}

pub(super) fn admit(
    f: &Function,
    inst: Inst,
    slots: IrValue,
    registers: usize,
) -> Result<Input, JitError> {
    let InstructionData::Load {
        opcode: Opcode::Load,
        arg,
        offset,
        ..
    } = f.dfg.insts[inst]
    else {
        return Err(invalid());
    };
    let offset = i32::from(offset);
    if f.dfg.value_type(slots) != types::I64
        || registers > 256
        || offset < 0
        || offset % 8 != 0
        || offset as usize / 16 >= registers
        || arg != slots
    {
        return Err(invalid());
    }
    let result = f.dfg.first_result(inst);
    let ty = f.dfg.value_type(result);
    let kind = match (offset % 16, ty) {
        (0, types::I64) => Kind::Tag,
        (8, types::I64 | types::F64) => Kind::Bits(ty),
        _ => return Err(invalid()),
    };
    reads::load(f, inst, slots, offset, ty)?;
    Ok(Input {
        inst,
        slots,
        index: offset as u32 / 16,
        result,
        kind,
    })
}

pub(super) fn emit(f: &mut Function, input: Input) -> Read {
    match input.kind {
        Kind::Tag => {
            let mut c = FuncCursor::new(f);
            c.goto_inst(input.inst);
            let packed = c.ins().load(
                types::I64,
                MemFlagsData::new(),
                input.slots,
                input.index as i32 * 8,
            );
            let mask = c.ins().iconst(types::I64, 3);
            let low = c.ins().band(packed, mask);
            let top = c.ins().iconst(types::I64, 5);
            let above = c.ins().icmp(IntCC::UnsignedGreaterThan, packed, top);
            let one = c.ins().iconst(types::I64, 1);
            let low_valid = c.ins().icmp(IntCC::UnsignedLessThanOrEqual, low, one);
            let numeric = c.ins().band(above, low_valid);
            let two = c.ins().iconst(types::I64, 2);
            let numeric_tag = c.ins().iadd(low, two);
            let four = c.ins().iconst(types::I64, 4);
            let nil = c.ins().icmp(IntCC::Equal, packed, four);
            let zero = c.ins().iconst(types::I64, 0);
            let immediate = c.ins().select(nil, zero, one);
            let opaque = c.ins().icmp(IntCC::Equal, packed, top);
            let immediate = c.ins().select(opaque, four, immediate);
            let offset = c.ins().isub(packed, two);
            let immediate_valid = c.ins().icmp(IntCC::UnsignedLessThanOrEqual, offset, mask);
            let invalid = c.ins().iconst(types::I64, -1);
            let immediate = c.ins().select(immediate_valid, immediate, invalid);
            c.func
                .replace(input.inst)
                .select(numeric, numeric_tag, immediate);
            Read::Tag(input)
        }
        Kind::Bits(ty) => {
            let before = f.layout.inst_block(input.inst).unwrap();
            let after = f.dfg.make_block();
            f.layout.split_block(after, input.inst);
            let parameter = if ty == types::F64 {
                let parameter = f.dfg.append_block_param(after, types::I64);
                f.replace(input.inst)
                    .bitcast(types::F64, MemFlagsData::new(), parameter);
                parameter
            } else {
                f.dfg.detach_inst_results(input.inst);
                f.dfg.attach_block_param(after, input.result);
                f.layout.remove_inst(input.inst);
                input.result
            };
            let numeric = reads::block(f);
            let immediate = reads::block(f);
            let mut c = FuncCursor::new(f);
            c.goto_bottom(before);
            let packed = c.ins().load(
                types::I64,
                MemFlagsData::new(),
                input.slots,
                input.index as i32 * 8,
            );
            let top = c.ins().iconst(types::I64, 5);
            let above = c.ins().icmp(IntCC::UnsignedGreaterThan, packed, top);
            let mask = c.ins().iconst(types::I64, 3);
            let low = c.ins().band(packed, mask);
            let one = c.ins().iconst(types::I64, 1);
            let low_valid = c.ins().icmp(IntCC::UnsignedLessThanOrEqual, low, one);
            let valid = c.ins().band(above, low_valid);
            c.ins().brif(valid, numeric, &[], immediate, &[]);
            c.goto_bottom(numeric);
            let mask = c.ins().iconst(types::I64, !3);
            let pointer = c.ins().band(packed, mask);
            let bits = c.ins().load(types::I64, MemFlagsData::new(), pointer, 0);
            c.ins().jump(after, &[bits.into()]);
            c.goto_bottom(immediate);
            let true_value = c.ins().iconst(types::I64, 3);
            let is_true = c.ins().icmp(IntCC::Equal, packed, true_value);
            let bits = c.ins().uextend(types::I64, is_true);
            c.ins().jump(after, &[bits.into()]);
            Read::Bits {
                input,
                blocks: [before, after, numeric, immediate],
                parameter,
            }
        }
    }
}

pub(super) fn verify_source(
    f: &Function,
    read: &Read,
    slots: IrValue,
    registers: usize,
) -> Result<(), JitError> {
    let input = match read {
        Read::Tag(input) | Read::Bits { input, .. } => input,
    };
    let actual = admit(f, input.inst, slots, registers)?;
    if actual.slots != input.slots
        || actual.index != input.index
        || actual.result != input.result
        || actual.kind != input.kind
    {
        return Err(invalid());
    }
    Ok(())
}

fn operation(
    f: &Function,
    inst: Inst,
    opcode: Opcode,
    args: &[IrValue],
    ty: Type,
) -> Result<IrValue, JitError> {
    reads::value(f, inst, opcode, args, ty)
}

pub(super) fn verify(f: &Function, read: &Read) -> Result<(), JitError> {
    let input = match read {
        Read::Tag(input) | Read::Bits { input, .. } => input,
    };
    if input.index >= 256 || f.dfg.value_type(input.slots) != types::I64 {
        return Err(invalid());
    }
    match read {
        Read::Tag(input) => {
            if input.kind != Kind::Tag {
                return Err(invalid());
            }
            let mut sequence = shape::<21>(
                std::iter::successors(Some(input.inst), |&inst| f.layout.prev_inst(inst)).take(21),
            )?;
            sequence.reverse();
            let [packed, mask, low, top, above, one, low_valid, numeric, two, numeric_tag, four, nil, zero, immediate, opaque, selected, offset, immediate_valid, invalid_tag, admitted, result] =
                sequence;
            let packed = reads::load(f, packed, input.slots, input.index as i32 * 8, types::I64)?;
            let mask = reads::constant(f, mask, 3)?;
            let low = operation(f, low, Opcode::Band, &[packed, mask], types::I64)?;
            let top = reads::constant(f, top, 5)?;
            let above = reads::compare(f, above, IntCC::UnsignedGreaterThan, [packed, top])?;
            let one = reads::constant(f, one, 1)?;
            let low_valid =
                reads::compare(f, low_valid, IntCC::UnsignedLessThanOrEqual, [low, one])?;
            let numeric = operation(f, numeric, Opcode::Band, &[above, low_valid], types::I8)?;
            let two = reads::constant(f, two, 2)?;
            let numeric_tag = operation(f, numeric_tag, Opcode::Iadd, &[low, two], types::I64)?;
            let four = reads::constant(f, four, 4)?;
            let nil = reads::compare(f, nil, IntCC::Equal, [packed, four])?;
            let zero = reads::constant(f, zero, 0)?;
            let immediate = operation(f, immediate, Opcode::Select, &[nil, zero, one], types::I64)?;
            let opaque = reads::compare(f, opaque, IntCC::Equal, [packed, top])?;
            let selected = operation(
                f,
                selected,
                Opcode::Select,
                &[opaque, four, immediate],
                types::I64,
            )?;
            let offset = operation(f, offset, Opcode::Isub, &[packed, two], types::I64)?;
            let immediate_valid = reads::compare(
                f,
                immediate_valid,
                IntCC::UnsignedLessThanOrEqual,
                [offset, mask],
            )?;
            let invalid_tag = reads::constant(f, invalid_tag, -1)?;
            let admitted = operation(
                f,
                admitted,
                Opcode::Select,
                &[immediate_valid, selected, invalid_tag],
                types::I64,
            )?;
            if operation(
                f,
                result,
                Opcode::Select,
                &[numeric, numeric_tag, admitted],
                types::I64,
            )? != input.result
            {
                return Err(invalid());
            }
        }
        Read::Bits {
            input,
            blocks: [before, after, numeric, immediate],
            parameter,
        } => {
            let Kind::Bits(ty) = input.kind else {
                return Err(invalid());
            };
            if !matches!(ty, types::I64 | types::F64)
                || f.dfg.block_params(*after) != [*parameter]
                || f.dfg.value_type(*parameter) != types::I64
                || !f.dfg.block_params(*numeric).is_empty()
                || !f.dfg.block_params(*immediate).is_empty()
            {
                return Err(invalid());
            }
            if ty == types::F64 {
                if f.layout.first_inst(*after) != Some(input.inst)
                    || reads::bitcast(f, input.inst, *parameter, types::F64)? != input.result
                {
                    return Err(invalid());
                }
            } else if *parameter != input.result {
                return Err(invalid());
            }
            let [branch, valid, low_valid, one, low, mask, above, top, packed] =
                shape(f.layout.block_insts(*before).rev().take(9))?;
            let packed = reads::load(f, packed, input.slots, input.index as i32 * 8, types::I64)?;
            let top = reads::constant(f, top, 5)?;
            let above = reads::compare(f, above, IntCC::UnsignedGreaterThan, [packed, top])?;
            let mask = reads::constant(f, mask, 3)?;
            let low = operation(f, low, Opcode::Band, &[packed, mask], types::I64)?;
            let one = reads::constant(f, one, 1)?;
            let low_valid =
                reads::compare(f, low_valid, IntCC::UnsignedLessThanOrEqual, [low, one])?;
            let valid = operation(f, valid, Opcode::Band, &[above, low_valid], types::I8)?;
            reads::branch(f, branch, valid, *numeric, *immediate)?;
            let [mask, pointer, bits, jump] = shape(f.layout.block_insts(*numeric))?;
            let mask = reads::constant(f, mask, !3)?;
            let pointer = operation(f, pointer, Opcode::Band, &[packed, mask], types::I64)?;
            let bits = reads::load(f, bits, pointer, 0, types::I64)?;
            reads::jump(f, jump, *after, bits)?;
            let [true_value, is_true, bits, jump] = shape(f.layout.block_insts(*immediate))?;
            let true_value = reads::constant(f, true_value, 3)?;
            let is_true = reads::compare(f, is_true, IntCC::Equal, [packed, true_value])?;
            let bits = operation(f, bits, Opcode::Uextend, &[is_true], types::I64)?;
            reads::jump(f, jump, *after, bits)?;
        }
    }
    Ok(())
}

#[path = "packed_read_tests.rs"]
mod tests;
