use std::mem::offset_of;

use cranelift_codegen::ir::{
    condcodes::IntCC, types, Block, InstBuilder, MemFlagsData, Value as IrValue,
};
use cranelift_frontend::FunctionBuilder;

use super::{native::View, *};

pub(super) fn guard(b: &mut FunctionBuilder<'_>, condition: IrValue, decline: Block) {
    let next = b.create_block();
    b.ins().brif(condition, next, &[], decline, &[]);
    b.switch_to_block(next);
}

pub(super) fn emit_access(
    b: &mut FunctionBuilder<'_>,
    access: Access,
    view: IrValue,
    key: IrValue,
    slot: IrValue,
    decline: Block,
) {
    emit(
        b,
        view,
        key,
        if access == Access::Read {
            Input::ReadTo(slot)
        } else {
            Input::WriteFrom(slot)
        },
        decline,
    );
}

pub(super) enum Input {
    ReadTo(IrValue),
    WriteFrom(IrValue),
    WriteValue(IrValue, IrValue),
}

pub(super) fn emit(
    b: &mut FunctionBuilder<'_>,
    view: IrValue,
    key: IrValue,
    input: Input,
    decline: Block,
) {
    let access = if matches!(input, Input::ReadTo(_)) {
        Access::Read
    } else {
        Access::Write
    };
    let flags = MemFlagsData::new();
    let valid = b.ins().icmp_imm_u(IntCC::NotEqual, view, 0);
    guard(b, valid, decline);
    if let Input::ReadTo(pointer) | Input::WriteFrom(pointer) = input {
        let valid = b.ins().icmp_imm_u(IntCC::NotEqual, pointer, 0);
        guard(b, valid, decline);
    }
    let version = b
        .ins()
        .load(types::I64, flags, view, offset_of!(View, version) as i32);
    let valid = b
        .ins()
        .icmp_imm_u(IntCC::Equal, version, native::VERSION as i64);
    guard(b, valid, decline);
    let length = b
        .ins()
        .load(types::I64, flags, view, offset_of!(View, length) as i32);
    let valid = b
        .ins()
        .icmp_imm_u(IntCC::UnsignedLessThanOrEqual, length, LIMIT as i64);
    guard(b, valid, decline);
    let valid = b.ins().icmp_imm_u(IntCC::NotEqual, length, 0);
    guard(b, valid, decline);
    let first = b
        .ins()
        .load(types::I64, flags, view, offset_of!(View, first) as i32);
    let valid = b.ins().icmp_imm_s(IntCC::SignedGreaterThan, first, 0);
    guard(b, valid, decline);
    let valid = b.ins().icmp(IntCC::SignedGreaterThanOrEqual, key, first);
    guard(b, valid, decline);
    let index = b.ins().isub(key, first);
    let valid = b.ins().icmp(IntCC::UnsignedLessThan, index, length);
    guard(b, valid, decline);
    let writable = b
        .ins()
        .load(types::I64, flags, view, offset_of!(View, writable) as i32);
    let valid = b.ins().icmp_imm_u(
        if access == Access::Write {
            IntCC::Equal
        } else {
            IntCC::UnsignedLessThanOrEqual
        },
        writable,
        1,
    );
    guard(b, valid, decline);
    let cells = b
        .ins()
        .load(types::I64, flags, view, offset_of!(View, slots) as i32);
    let dirty = b
        .ins()
        .load(types::I64, flags, view, offset_of!(View, dirty) as i32);
    let counts = b
        .ins()
        .load(types::I64, flags, view, offset_of!(View, counts) as i32);
    for pointer in [cells, dirty, counts] {
        let valid = b.ins().icmp_imm_u(IntCC::NotEqual, pointer, 0);
        guard(b, valid, decline);
    }
    let offset = b.ins().ishl_imm_u(index, 4);
    let cell = b.ins().iadd(cells, offset);
    let (tag, bits, destination) = match input {
        Input::WriteValue(tag, bits) => (tag, bits, cell),
        Input::ReadTo(slot) | Input::WriteFrom(slot) => {
            let (source, destination) = if access == Access::Write {
                (slot, cell)
            } else {
                (cell, slot)
            };
            let tag = b
                .ins()
                .load(types::I64, flags, source, offset_of!(Slot, tag) as i32);
            let bits = b
                .ins()
                .load(types::I64, flags, source, offset_of!(Slot, bits) as i32);
            (tag, bits, destination)
        }
    };
    guard_scalar(b, tag, bits, decline);
    b.ins()
        .store(flags, tag, destination, offset_of!(Slot, tag) as i32);
    b.ins()
        .store(flags, bits, destination, offset_of!(Slot, bits) as i32);
    if access == Access::Write {
        let previous = b.ins().load(types::I64, flags, dirty, 0);
        let one = b.ins().iconst(types::I64, 1);
        let bit = b.ins().ishl(one, index);
        let updated = b.ins().bor(previous, bit);
        b.ins().store(flags, updated, dirty, 0);
    }
    let offset = if access == Access::Write {
        offset_of!(Counts, writes)
    } else {
        offset_of!(Counts, reads)
    } as i32;
    let count = b.ins().load(types::I32, flags, counts, offset);
    let saturated = b.ins().icmp_imm_s(IntCC::Equal, count, -1);
    let incremented = b.ins().iadd_imm_u(count, 1);
    let updated = b.ins().select(saturated, count, incremented);
    b.ins().store(flags, updated, counts, offset);
}

pub(super) fn guard_scalar(
    b: &mut FunctionBuilder<'_>,
    tag: IrValue,
    bits: IrValue,
    decline: Block,
) {
    let valid_tag = b
        .ins()
        .icmp_imm_u(IntCC::UnsignedLessThanOrEqual, tag, abi::NUMBER as i64);
    guard(b, valid_tag, decline);
    let numeric = b
        .ins()
        .icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, tag, abi::INTEGER as i64);
    let nil = b.ins().icmp_imm_u(IntCC::Equal, tag, abi::NIL as i64);
    let empty = b.ins().icmp_imm_u(IntCC::Equal, bits, 0);
    let nil = b.ins().band(nil, empty);
    let boolean = b.ins().icmp_imm_u(IntCC::Equal, tag, abi::BOOLEAN as i64);
    let boolean_bits = b.ins().icmp_imm_u(IntCC::UnsignedLessThanOrEqual, bits, 1);
    let boolean = b.ins().band(boolean, boolean_bits);
    let valid = b.ins().bor(nil, boolean);
    let valid = b.ins().bor(valid, numeric);
    guard(b, valid, decline);
}

#[cfg(test)]
mod tests;
