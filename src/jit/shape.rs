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
    Overflow(&'a Expr<'a>, &'a Expr<'a>),
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
    let ValueDef::Result(inst, result_index) = function.dfg.value_def(value) else {
        return false;
    };
    if result_index != usize::from(matches!(expression, Expr::Overflow(..))) {
        return false;
    }
    let ty = function.dfg.value_type(value);
    match (expression, function.dfg.insts[inst]) {
        (
            Expr::Overflow(left, right),
            InstructionData::Binary {
                opcode: Opcode::SaddOverflow,
                args,
            },
        ) => {
            ty == types::I8
                && matches(function, args[0], left, remaining)
                && matches(function, args[1], right, remaining)
        }
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

#[derive(Clone, Copy)]
pub(super) enum LoopExpression {
    PrepSplit,
    StepSplit,
    LimitSplit,
    Nonzero,
    PrepInteger,
    PrepFloat,
    StepInteger,
    StepFloat,
    ConditionInteger,
    ConditionMixedLimit,
    ConditionFloat,
}

pub(super) fn numeric_loop(
    function: &Function,
    value: Value,
    inputs: &[(Value, Value)],
    part: LoopExpression,
) -> bool {
    use LoopExpression::*;
    let prep = matches!(part, PrepSplit | Nonzero | PrepInteger | PrepFloat);
    if inputs.len() != if prep { 2 } else { 3 } {
        return false;
    }
    let index_tag = Expr::Value(inputs[0].0);
    let index_bits = Expr::Value(inputs[0].1);
    let step_tag = Expr::Value(inputs[inputs.len() - 1].0);
    let step_bits = Expr::Value(inputs[inputs.len() - 1].1);
    let integer_tag = Expr::Int(types::I64, abi::INTEGER);
    let index_integer = Expr::Compare(IntCC::Equal, &index_tag, &integer_tag);
    let step_integer = Expr::Compare(IntCC::Equal, &step_tag, &integer_tag);
    let split = Expr::Binary(Opcode::Band, types::I8, &index_integer, &step_integer);
    if matches!(part, PrepSplit | StepSplit) {
        return matches(function, value, &split, &mut 1024);
    }
    let index_signed = Expr::Unary(Opcode::FcvtFromSint, types::F64, &index_bits);
    let index_cast = Expr::Cast(types::F64, &index_bits);
    let index_float = Expr::Select(types::F64, &index_integer, &index_signed, &index_cast);
    let step_signed = Expr::Unary(Opcode::FcvtFromSint, types::F64, &step_bits);
    let step_cast = Expr::Cast(types::F64, &step_bits);
    let step_float = Expr::Select(types::F64, &step_integer, &step_signed, &step_cast);
    let zero = Expr::Float(0.0f64.to_bits());
    let nonzero = Expr::FloatCompare(FloatCC::NotEqual, &step_float, &zero);
    let prep_int = Expr::Binary(Opcode::Isub, types::I64, &index_bits, &step_bits);
    let prep_float = Expr::Binary(Opcode::Fsub, types::F64, &index_float, &step_float);
    let prep_bits = Expr::Cast(types::I64, &prep_float);
    if prep {
        return matches(
            function,
            value,
            match part {
                Nonzero => &nonzero,
                PrepInteger => &prep_int,
                PrepFloat => &prep_bits,
                _ => return false,
            },
            &mut 1024,
        );
    }
    let index = Expr::Binary(Opcode::SaddOverflow, types::I64, &index_bits, &step_bits);
    let overflow = Expr::Overflow(&index_bits, &step_bits);
    let one = Expr::Int(types::I8, 1);
    let not_overflow = Expr::Binary(Opcode::Bxor, types::I8, &overflow, &one);
    let float_index = Expr::Binary(Opcode::Fadd, types::F64, &index_float, &step_float);
    let float_bits = Expr::Cast(types::I64, &float_index);
    let limit_tag = Expr::Value(inputs[1].0);
    let limit_bits = Expr::Value(inputs[1].1);
    let limit_integer = Expr::Compare(IntCC::Equal, &limit_tag, &integer_tag);
    let int_zero = Expr::Int(types::I64, 0);
    let negative = Expr::Compare(IntCC::SignedLessThan, &step_bits, &int_zero);
    let ge = Expr::Compare(IntCC::SignedGreaterThanOrEqual, &index, &limit_bits);
    let le = Expr::Compare(IntCC::SignedLessThanOrEqual, &index, &limit_bits);
    let int_range = Expr::Select(types::I8, &negative, &ge, &le);
    let converted_index = Expr::Unary(Opcode::FcvtFromSint, types::F64, &index);
    let cast_limit = Expr::Cast(types::F64, &limit_bits);
    let ge = Expr::FloatCompare(FloatCC::GreaterThanOrEqual, &converted_index, &cast_limit);
    let le = Expr::FloatCompare(FloatCC::LessThanOrEqual, &converted_index, &cast_limit);
    let float_range = Expr::Select(types::I8, &negative, &ge, &le);
    let condition = Expr::Binary(Opcode::Band, types::I8, &not_overflow, &int_range);
    let mixed_condition = Expr::Binary(Opcode::Band, types::I8, &not_overflow, &float_range);
    let limit_signed = Expr::Unary(Opcode::FcvtFromSint, types::F64, &limit_bits);
    let limit_float = Expr::Select(types::F64, &limit_integer, &limit_signed, &cast_limit);
    let float_negative = Expr::FloatCompare(FloatCC::LessThan, &step_float, &zero);
    let ge = Expr::FloatCompare(FloatCC::GreaterThanOrEqual, &float_index, &limit_float);
    let le = Expr::FloatCompare(FloatCC::LessThanOrEqual, &float_index, &limit_float);
    let float_condition = Expr::Select(types::I8, &float_negative, &ge, &le);
    matches(
        function,
        value,
        match part {
            StepInteger => &index,
            LimitSplit => &limit_integer,
            StepFloat => &float_bits,
            ConditionInteger => &condition,
            ConditionMixedLimit => &mixed_condition,
            ConditionFloat => &float_condition,
            _ => return false,
        },
        &mut 1024,
    )
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
