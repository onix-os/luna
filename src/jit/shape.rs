use cranelift_codegen::ir::{
    condcodes::{FloatCC, IntCC},
    types, Function, InstructionData, MemFlagsData, Opcode, Type, Value, ValueDef,
};

use super::abi;

enum Expr<'a> {
    Value(Value),
    Int(Type, u64),
    Float(u64),
    Compare(IntCC, &'a Expr<'a>, &'a Expr<'a>),
    FloatCompare(FloatCC, &'a Expr<'a>, &'a Expr<'a>),
    Unary(Opcode, Type, &'a Expr<'a>),
    Cast(Type, &'a Expr<'a>),
    Binary(Opcode, Type, &'a Expr<'a>, &'a Expr<'a>),
    Select(Type, &'a Expr<'a>, &'a Expr<'a>, &'a Expr<'a>),
}

fn matches(
    function: &Function,
    value: Value,
    expression: &Expr<'_>,
    remaining: &mut usize,
) -> bool {
    let Some(next) = remaining.checked_sub(1) else {
        return false;
    };
    *remaining = next;
    let value = function.dfg.resolve_aliases(value);
    if let Expr::Value(expected) = expression {
        return value == function.dfg.resolve_aliases(*expected);
    }
    let ValueDef::Result(inst, 0) = function.dfg.value_def(value) else {
        return false;
    };
    let ty = function.dfg.value_type(value);
    match (expression, function.dfg.insts[inst]) {
        (
            Expr::Float(bits),
            InstructionData::UnaryIeee64 {
                opcode: Opcode::F64const,
                imm,
            },
        ) => ty == types::F64 && imm.bits() == *bits,
        (
            Expr::FloatCompare(expected, left, right),
            InstructionData::FloatCompare {
                opcode: Opcode::Fcmp,
                cond,
                args,
            },
        ) => {
            ty == types::I8
                && cond == *expected
                && matches(function, args[0], left, remaining)
                && matches(function, args[1], right, remaining)
        }
        (
            Expr::Cast(expected_ty, input),
            InstructionData::LoadNoOffset {
                opcode: Opcode::Bitcast,
                flags,
                arg,
            },
        ) => {
            ty == *expected_ty
                && function.dfg.mem_flags[flags] == MemFlagsData::new()
                && matches(function, arg, input, remaining)
        }
        (
            Expr::Select(expected_ty, condition, yes, no),
            InstructionData::Ternary {
                opcode: Opcode::Select,
                args,
            },
        ) => {
            ty == *expected_ty
                && matches(function, args[0], condition, remaining)
                && matches(function, args[1], yes, remaining)
                && matches(function, args[2], no, remaining)
        }
        (
            Expr::Int(expected, bits),
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            },
        ) => ty == *expected && imm.bits() as u64 == *bits,
        (
            Expr::Compare(expected, left, right),
            InstructionData::IntCompare {
                opcode: Opcode::Icmp,
                cond,
                args,
            },
        ) => {
            ty == types::I8
                && cond == *expected
                && matches(function, args[0], left, remaining)
                && matches(function, args[1], right, remaining)
        }
        (Expr::Unary(expected, expected_ty, input), InstructionData::Unary { opcode, arg }) => {
            ty == *expected_ty && opcode == *expected && matches(function, arg, input, remaining)
        }
        (
            Expr::Binary(expected, expected_ty, left, right),
            InstructionData::Binary { opcode, args },
        ) => {
            ty == *expected_ty
                && opcode == *expected
                && matches(function, args[0], left, remaining)
                && matches(function, args[1], right, remaining)
        }
        _ => false,
    }
}

pub(super) fn truth(
    function: &Function,
    result: Value,
    tag: Value,
    bits: Value,
    negated: bool,
    extended: bool,
) -> bool {
    let tag = Expr::Value(tag);
    let bits = Expr::Value(bits);
    let nil = Expr::Int(types::I64, abi::NIL);
    let boolean = Expr::Int(types::I64, abi::BOOLEAN);
    let zero = Expr::Int(types::I64, 0);
    let one = Expr::Int(types::I8, 1);
    let is_nil = Expr::Compare(IntCC::Equal, &tag, &nil);
    let is_boolean = Expr::Compare(IntCC::Equal, &tag, &boolean);
    let is_zero = Expr::Compare(IntCC::Equal, &bits, &zero);
    let false_boolean = Expr::Binary(Opcode::Band, types::I8, &is_boolean, &is_zero);
    let false_value = Expr::Binary(Opcode::Bor, types::I8, &is_nil, &false_boolean);
    let truth = Expr::Binary(Opcode::Bxor, types::I8, &false_value, &one);
    let opposite = Expr::Binary(Opcode::Bxor, types::I8, &truth, &one);
    let selected = if negated { &opposite } else { &truth };
    let extended_value = Expr::Unary(Opcode::Uextend, types::I64, selected);
    matches(
        function,
        result,
        if extended { &extended_value } else { selected },
        &mut 128,
    )
}

pub(super) fn increment(function: &Function, result: Value, base: Value) -> bool {
    let base = Expr::Value(base);
    let one = Expr::Int(types::I32, 1);
    let increment = Expr::Binary(Opcode::Iadd, types::I32, &base, &one);
    matches(function, result, &increment, &mut 128)
}

pub(super) fn polarity(function: &Function, result: Value, base: Value, inverted: bool) -> bool {
    let base = Expr::Value(base);
    let one = Expr::Int(types::I8, 1);
    let opposite = Expr::Binary(Opcode::Bxor, types::I8, &base, &one);
    matches(
        function,
        result,
        if inverted { &opposite } else { &base },
        &mut 128,
    )
}

pub(super) fn comparison(
    function: &Function,
    result: Value,
    inputs: [(Value, Value); 2],
    op: crate::opcode::Operation,
    mixed: bool,
) -> bool {
    use crate::opcode::Operation;
    let left_tag = Expr::Value(inputs[0].0);
    let left = Expr::Value(inputs[0].1);
    let right = Expr::Value(inputs[1].1);
    let integer_tag = Expr::Int(types::I64, abi::INTEGER);
    let left_integer = Expr::Compare(IntCC::Equal, &left_tag, &integer_tag);
    let (icc, fcc) = match op {
        Operation::Eq { .. } => (IntCC::Equal, FloatCC::Equal),
        Operation::Less { .. } => (IntCC::SignedLessThan, FloatCC::LessThan),
        Operation::LessEq { .. } => (IntCC::SignedLessThanOrEqual, FloatCC::LessThanOrEqual),
        _ => return false,
    };
    if !mixed {
        let int_result = Expr::Compare(icc, &left, &right);
        let left_float = Expr::Cast(types::F64, &left);
        let right_float = Expr::Cast(types::F64, &right);
        let float_result = Expr::FloatCompare(fcc, &left_float, &right_float);
        let selected = Expr::Select(types::I8, &left_integer, &int_result, &float_result);
        return matches(function, result, &selected, &mut 1024);
    }
    let integer = Expr::Select(types::I64, &left_integer, &left, &right);
    let raw = Expr::Select(types::I64, &left_integer, &right, &left);
    let float = Expr::Cast(types::F64, &raw);
    let truncated = Expr::Unary(Opcode::FcvtToSintSat, types::I64, &float);
    let integral = Expr::Unary(Opcode::FcvtFromSint, types::F64, &truncated);
    let tie = Expr::Compare(IntCC::Equal, &integer, &truncated);
    let upper = Expr::Float(9_223_372_036_854_775_808.0f64.to_bits());
    let below_upper = Expr::FloatCompare(FloatCC::LessThan, &float, &upper);
    let whole = Expr::FloatCompare(FloatCC::Equal, &float, &integral);
    let equal = Expr::Binary(Opcode::Band, types::I8, &tie, &whole);
    let bounded_equal = Expr::Binary(Opcode::Band, types::I8, &equal, &below_upper);
    if matches!(op, Operation::Eq { .. }) {
        return matches(function, result, &bounded_equal, &mut 1024);
    }
    let int_less = Expr::Compare(IntCC::SignedLessThan, &integer, &truncated);
    let fraction_above = Expr::FloatCompare(FloatCC::GreaterThan, &float, &integral);
    let tie_less = Expr::Binary(Opcode::Band, types::I8, &tie, &fraction_above);
    let less = Expr::Binary(Opcode::Bor, types::I8, &int_less, &tie_less);
    let at_upper = Expr::FloatCompare(FloatCC::GreaterThanOrEqual, &float, &upper);
    let bounded_less = Expr::Binary(Opcode::Bor, types::I8, &less, &at_upper);
    let int_greater = Expr::Compare(IntCC::SignedGreaterThan, &integer, &truncated);
    let fraction_below = Expr::FloatCompare(FloatCC::LessThan, &float, &integral);
    let tie_greater = Expr::Binary(Opcode::Band, types::I8, &tie, &fraction_below);
    let greater = Expr::Binary(Opcode::Bor, types::I8, &int_greater, &tie_greater);
    let lower = Expr::Float((-9_223_372_036_854_775_808.0f64).to_bits());
    let below_lower = Expr::FloatCompare(FloatCC::LessThan, &float, &lower);
    let bounded_greater = Expr::Binary(Opcode::Bor, types::I8, &greater, &below_lower);
    let forward = Expr::Select(types::I8, &left_integer, &bounded_less, &bounded_greater);
    let inclusive = Expr::Binary(Opcode::Bor, types::I8, &forward, &bounded_equal);
    let ordered = Expr::FloatCompare(FloatCC::Ordered, &float, &float);
    let selected = if matches!(op, Operation::LessEq { .. }) {
        &inclusive
    } else {
        &forward
    };
    let result_shape = Expr::Binary(Opcode::Band, types::I8, selected, &ordered);
    matches(function, result, &result_shape, &mut 1024)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::InstBuilder;
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

    #[test]
    fn typed_expression_matching_is_bounded_and_refuses_wrong_widths_and_polarities() {
        let mut function = Function::new();
        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut function, &mut context);
        let block = builder.create_block();
        builder.switch_to_block(block);
        let base = builder.ins().iconst(types::I32, 0);
        let result = builder.ins().iadd_imm_s(base, 1);
        let leaf = Expr::Value(base);
        let one = Expr::Int(types::I32, 1);
        let expected = Expr::Binary(Opcode::Iadd, types::I32, &leaf, &one);
        assert!(matches(builder.func, result, &expected, &mut 3));
        assert!(!matches(builder.func, result, &expected, &mut 2));
        assert!(increment(builder.func, result, base));
        let wrong = builder.ins().iadd_imm_s(base, 2);
        assert!(!increment(builder.func, wrong, base));
        let wrong_width = Expr::Binary(Opcode::Iadd, types::I64, &leaf, &one);
        assert!(!matches(builder.func, result, &wrong_width, &mut 128));
        let wrong_opcode = Expr::Binary(Opcode::Isub, types::I32, &leaf, &one);
        assert!(!matches(builder.func, result, &wrong_opcode, &mut 128));
        builder.ins().return_(&[]);
        builder.seal_all_blocks();
        let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
            .unwrap()
            .finish(cranelift_codegen::settings::Flags::new(
                cranelift_codegen::settings::builder(),
            ))
            .unwrap();
        builder.finalize(isa.frontend_config());
        cranelift_codegen::verify_function(
            &function,
            &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
        )
        .unwrap();
    }
}
