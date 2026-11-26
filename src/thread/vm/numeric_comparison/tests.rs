use allocator_api2::SliceExt;
use ottavino_gc_arena::allocator_api::MetricsAlloc;

use crate::{
    meta_ops::{self, MetaResult},
    opcode::{OpCode, Operation, RCIndex},
    thread::thread::{Frame, LuaFrame},
    types::{ConstantIndex8, RegisterIndex},
    Closure, Context, Fuel, FunctionPrototype, Lua, Thread, Value,
};

fn inputs<'gc>(ctx: Context<'gc>) -> Vec<Value<'gc>> {
    let mut values = vec![
        Value::Nil,
        Value::Boolean(false),
        ctx.intern(b"2").into(),
        ctx.intern(b"x").into(),
    ];
    for i in [
        i64::MIN,
        i64::MAX,
        -9_007_199_254_740_993,
        9_007_199_254_740_993,
        -1,
        0,
        1,
    ] {
        values.extend([Value::Integer(i), Value::Number(i as f64)]);
    }
    for bits in [
        0,
        1,
        0x000f_ffff_ffff_ffff,
        0x0010_0000_0000_0000,
        0x3fe0_0000_0000_0000,
        0x7fef_ffff_ffff_ffff,
        0x7ff0_0000_0000_0000,
        0x7ff0_0000_0000_0001,
        0x7ff8_0000_0000_1234,
    ] {
        values.extend([bits, bits | (1 << 63)].map(|b| Value::Number(f64::from_bits(b))));
    }
    values
}

#[test]
fn numeric_oracle_preserves_exact_order_and_unordered_values() {
    Lua::empty().enter(|ctx| {
        for a in inputs(ctx) {
            for b in inputs(ctx) {
                for le in [false, true] {
                    let actual = if le {
                        super::compare::<true>(a, b)
                    } else {
                        super::compare::<false>(a, b)
                    };
                    if matches!(a, Value::Integer(_) | Value::Number(_))
                        && matches!(b, Value::Integer(_) | Value::Number(_))
                    {
                        let expected = if le {
                            meta_ops::less_equal(ctx, a, b)
                        } else {
                            meta_ops::less_than(ctx, a, b)
                        }
                        .unwrap();
                        let MetaResult::Value(expected) = expected else {
                            panic!("numeric call")
                        };
                        assert_eq!(actual, Some(expected.to_bool()), "{a:?} {b:?} le={le}");
                    } else {
                        assert_eq!(actual, None);
                    }
                }
            }
        }
    });
}

fn check<'gc>(
    ctx: Context<'gc>,
    input: [Value<'gc>; 2],
    base: u8,
    constants: u8,
    le: bool,
    skip_if: bool,
) {
    let expected = if le {
        meta_ops::less_equal(ctx, input[0], input[1])
    } else {
        meta_ops::less_than(ctx, input[0], input[1])
    };
    let mut prototype = FunctionPrototype::compile(ctx, "numeric-comparison", b"return").unwrap();
    prototype.stack_size = (usize::from(base) + 2).try_into().unwrap();
    prototype.constants = SliceExt::to_vec_in(
        &input.map(|v| v.to_constant().unwrap())[..],
        MetricsAlloc::new(&ctx),
    )
    .into_boxed_slice();
    let operand = |i| {
        if constants & (1u8 << i) != 0 {
            RCIndex::Constant(ConstantIndex8(i))
        } else {
            RCIndex::Register(RegisterIndex(base + i))
        }
    };
    let operation = if le {
        Operation::LessEq {
            skip_if,
            left: operand(0),
            right: operand(1),
        }
    } else {
        Operation::Less {
            skip_if,
            left: operand(0),
            right: operand(1),
        }
    };
    prototype.opcodes =
        SliceExt::to_vec_in(&[OpCode::encode(operation)][..], MetricsAlloc::new(&ctx))
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
    let result = super::super::run_vm(
        ctx,
        LuaFrame {
            #[cfg(all(feature = "jit", not(miri), luna_jit_native))]
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
    let skip = match expected {
        Ok(MetaResult::Value(v)) => {
            assert_eq!(result.unwrap(), 1);
            v.to_bool() == skip_if
        }
        Err(e) => {
            assert_eq!(
                format!("{:?}", result.unwrap_err()),
                format!("{:?}", super::super::VMError::OperatorError(e))
            );
            false
        }
        _ => panic!("unexpected call"),
    };
    assert!(matches!(state.frames[0],Frame::Lua{pc,..} if pc==1+usize::from(skip)));
    assert_eq!(fuel.remaining(), 19);
    let stack = storage.borrow();
    assert_eq!(stack.len(), before.len());
    for (a, b) in stack[frame_base..].iter().zip(&before[frame_base..]) {
        match (a, b) {
            (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
            (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
            (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
            (Value::String(a), Value::String(b)) => assert_eq!(a, b),
            (Value::Nil, Value::Nil) => {}
            pair => panic!("changed stack value: {pair:?}"),
        }
    }
}

#[test]
fn single_step_preserves_pc_budget_stack_forms_and_errors() {
    Lua::empty().enter(|ctx| {
        let values = inputs(ctx);
        for &a in &values {
            for &b in &values {
                for base in [0, 253] {
                    for constants in 0..4 {
                        for le in [false, true] {
                            for skip_if in [false, true] {
                                check(ctx, [a, b], base, constants, le, skip_if);
                            }
                        }
                    }
                }
            }
        }
    });
}
