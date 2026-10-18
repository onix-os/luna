use allocator_api2::SliceExt;
use ottavino_gc_arena::allocator_api::MetricsAlloc;

use crate::{
    meta_ops,
    opcode::{OpCode, Operation, RCIndex},
    thread::thread::{Frame, LuaFrame},
    types::{ConstantIndex8, RegisterIndex},
    Closure, Context, Fuel, FunctionPrototype, Lua, Thread, Value,
};

fn same(actual: Value<'_>, expected: Value<'_>) {
    match (actual, expected) {
        (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
        (Value::Number(a), Value::Number(b)) => {
            assert!(a.to_bits() == b.to_bits() || a.is_nan() && b.is_nan());
        }
        (Value::String(a), Value::String(b)) => assert_eq!(a, b),
        (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
        (Value::Nil, Value::Nil) => {}
        pair => panic!("different values: {pair:?}"),
    }
}

fn check<'gc>(ctx: Context<'gc>, input: [Value<'gc>; 2], base: u8, dest: u8, constants: u8) {
    let expected = input[0]
        .to_constant()
        .unwrap()
        .add(&input[1].to_constant().unwrap());
    let mut prototype = FunctionPrototype::compile(ctx, "numeric-add", b"return").unwrap();
    prototype.stack_size = (usize::from(base) + 3).try_into().unwrap();
    prototype.constants = SliceExt::to_vec_in(
        &input.map(|value| value.to_constant().unwrap())[..],
        MetricsAlloc::new(&ctx),
    )
    .into_boxed_slice();
    let operand = |offset| {
        if constants & (1u8 << offset) != 0 {
            RCIndex::Constant(ConstantIndex8(offset))
        } else {
            RCIndex::Register(RegisterIndex(base + offset))
        }
    };
    prototype.opcodes = SliceExt::to_vec_in(
        &[OpCode::encode(Operation::Add {
            dest: RegisterIndex(base + dest),
            left: operand(0),
            right: operand(1),
        })][..],
        MetricsAlloc::new(&ctx),
    )
    .into_boxed_slice();
    let closure = Closure::new(&ctx, prototype, Some(ctx.globals())).unwrap();
    let thread = Thread::new(ctx);
    thread.start(ctx, closure.into(), ()).unwrap();
    let mut state = thread.into_inner().borrow_mut(&ctx);
    let frame_base = match state.frames[0] {
        Frame::Lua { base, .. } => base,
        _ => panic!("missing Lua frame"),
    };
    let storage = state.stack;
    let mut stack = storage.borrow_mut(&ctx);
    stack[frame_base..].fill(Value::Integer(-999));
    let start = frame_base + usize::from(base);
    stack[start..start + 2].copy_from_slice(&input);
    let before = stack.to_vec();
    let mut fuel = Fuel::with(19);
    let result = super::run_vm(
        ctx,
        LuaFrame {
            #[cfg(all(
                feature = "jit",
                not(miri),
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            pair_handoff: None,
            state: &mut state,
            #[cfg(feature = "jit")]
            stack: &mut stack,
            #[cfg(not(feature = "jit"))]
            stack,
            fuel: &mut fuel,
        },
        1,
    );
    #[cfg(feature = "jit")]
    drop(stack);
    match expected {
        Some(_) => assert_eq!(result.unwrap(), 1),
        None => {
            let canonical = meta_ops::add(ctx, input[0], input[1]).unwrap_err();
            assert_eq!(
                format!("{:?}", result.unwrap_err()),
                format!("{:?}", super::VMError::OperatorError(canonical))
            );
        }
    }
    assert!(matches!(state.frames[0], Frame::Lua { pc: 1, .. }));
    assert_eq!(fuel.remaining(), 19);
    let stack = storage.borrow();
    assert_eq!(stack.len(), before.len());
    for (offset, (&actual, &original)) in stack.iter().zip(&before).enumerate() {
        if let Some(value) = expected.filter(|_| offset == start + usize::from(dest)) {
            same(actual, value.into());
        } else if offset >= frame_base {
            same(actual, original);
        }
    }
}

#[test]
fn numeric_add_preserves_operand_forms_aliases_errors_and_bits() {
    Lua::empty().enter(|ctx| {
        use Value::{Boolean as B, Integer as I, Nil, Number as N};
        let text = Value::String(ctx.intern(b"41"));
        let invalid = Value::String(ctx.intern(b"invalid"));
        let inputs = [
            [I(i64::MAX), I(1)],
            [I(i64::MIN), I(-1)],
            [I(9_007_199_254_740_993), I(1)],
            [N(0.5), N(0.25)],
            [I(9_007_199_254_740_993), N(0.5)],
            [N(0.5), I(9_007_199_254_740_993)],
            [N(-0.0), N(-0.0)],
            [N(0.0), N(-0.0)],
            [N(f64::INFINITY), N(f64::NEG_INFINITY)],
            [N(f64::NAN), I(1)],
            [N(f64::from_bits(1)), N(f64::from_bits(1))],
            [text, I(1)],
            [N(0.5), text],
            [invalid, I(1)],
            [Nil, I(1)],
            [B(false), N(0.5)],
        ];
        for input in inputs {
            for base in [0, 253] {
                for dest in 0..3 {
                    for constants in 0..4 {
                        check(ctx, input, base, dest, constants);
                    }
                }
            }
        }
    });
}
