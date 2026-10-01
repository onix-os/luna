use cranelift_codegen::ir::{
    condcodes::IntCC, types, Function, InstructionData, Opcode, Type, Value, ValueDef,
};

use super::abi;

enum Expr<'a> {
    Value(Value),
    Int(Type, u64),
    Compare(IntCC, &'a Expr<'a>, &'a Expr<'a>),
    Unary(Opcode, Type, &'a Expr<'a>),
    Binary(Opcode, Type, &'a Expr<'a>, &'a Expr<'a>),
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
            Expr::Int(expected, bits),
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            },
        ) => ty == *expected && imm.bits() as u64 == *bits,
        (Expr::Compare(expected, left, right), InstructionData::IntCompare { cond, args, .. }) => {
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
