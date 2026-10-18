use allocator_api2::SliceExt;
use ottavino_gc_arena::allocator_api::MetricsAlloc;

use crate::{
    opcode::{OpCode, Operation},
    thread::thread::{Frame, LuaFrame},
    types::RegisterIndex,
    Closure, Context, Fuel, FunctionPrototype, Lua, Thread, Value,
};

fn same(actual: Value<'_>, expected: Value<'_>) {
    match (actual, expected) {
        (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
        (Value::Number(a), Value::Number(b)) => {
            assert!(a.to_bits() == b.to_bits() || a.is_nan() && b.is_nan());
        }
        (Value::String(a), Value::String(b)) => assert_eq!(a, b),
        (Value::Nil, Value::Nil) => {}
        pair => panic!("different values: {pair:?}"),
    }
}

fn check<'gc>(
    ctx: Context<'gc>,
    base: u8,
    visible: bool,
    values: [Value<'gc>; 3],
    next: Value<'gc>,
    continues: bool,
    error: Option<(&'static str, &'static str, &'static str)>,
) {
    let mut prototype = FunctionPrototype::compile(ctx, "for-controls", b"return").unwrap();
    let width = usize::from(base) + 3 + usize::from(visible);
    prototype.stack_size = width.try_into().unwrap();
    prototype.opcodes = SliceExt::to_vec_in(
        &[OpCode::encode(Operation::NumericForLoop {
            base: RegisterIndex(base),
            jump: 1,
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
    stack[start..start + 3].copy_from_slice(&values);
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
    let stack = storage.borrow();
    if let Some((index, limit, step)) = error {
        assert!(
            matches!(result, Err(super::VMError::BadForLoop(a,b,c)) if (a,b,c) == (index,limit,step))
        );
    } else {
        assert_eq!(result.unwrap(), 1);
    }
    assert!(matches!(state.frames[0], Frame::Lua { pc, .. } if pc == 1 + usize::from(continues)));
    assert_eq!(fuel.remaining(), 19);
    assert_eq!(stack.len(), before.len());
    for (offset, (&actual, &original)) in stack.iter().zip(&before).enumerate() {
        if offset == start {
            same(actual, next);
        } else if continues && offset == start + 3 {
            same(actual, next);
        } else if offset >= frame_base {
            same(actual, original);
        }
    }
}

#[test]
fn numeric_control_window_preserves_values_pc_and_errors() {
    Lua::empty().enter(|ctx| {
        use Value::{Integer as I, Nil, Number as N};
        let text = Value::String(ctx.intern(b"2"));
        let cases = [
            ([I(0), I(1), I(1)], I(1), true, None),
            ([I(1), I(1), I(1)], I(2), false, None),
            ([I(0), I(-1), I(-1)], I(-1), true, None),
            ([I(i64::MAX), I(i64::MAX), I(1)], I(i64::MIN), false, None),
            ([I(i64::MIN), I(i64::MIN), I(-1)], I(i64::MAX), false, None),
            ([I(0), text, I(1)], I(1), true, None),
            ([I(1), N(1.5), I(1)], I(2), false, None),
            ([N(0.0), I(1), N(0.5)], N(0.5), true, None),
            ([N(1.0), N(f64::NAN), N(1.0)], N(2.0), false, None),
            ([I(0), I(1), I(0)], I(0), true, None),
            (
                [I(0), Nil, I(1)],
                I(0),
                false,
                Some(("integer", "nil", "integer")),
            ),
            (
                [Nil, I(1), I(1)],
                Nil,
                false,
                Some(("nil", "number", "number")),
            ),
        ];
        for base in [0, 1, 252] {
            for (input, next, continues, error) in cases {
                check(ctx, base, true, input, next, continues, error);
            }
        }
    });
}

#[test]
fn terminating_numeric_loop_does_not_require_visible_slot() {
    Lua::empty().enter(|ctx| {
        use Value::{Integer as I, Number as N};
        for base in [0, 1, 253] {
            for (input, next) in [
                ([I(1), I(1), I(1)], I(2)),
                ([I(i64::MAX), I(i64::MAX), I(1)], I(i64::MIN)),
                ([I(1), N(1.5), I(1)], I(2)),
                ([N(1.0), N(f64::NAN), N(1.0)], N(2.0)),
            ] {
                check(ctx, base, false, input, next, false, None);
            }
        }
    });
}

#[test]
fn mixed_numeric_controls_preserve_conversion_and_termination() {
    Lua::empty().enter(|ctx| {
        use Value::{Integer as I, Nil, Number as N};
        let zero = Value::String(ctx.intern(b"0"));
        let step = Value::String(ctx.intern(b"-0.5"));
        let bad = Value::String(ctx.intern(b"invalid"));
        let cases = [
            ([N(1.0), N(0.0), N(-0.5)], N(0.5), true, None),
            ([N(1.0), N(1.0), N(-0.5)], N(0.5), false, None),
            ([N(1.0), N(1.0), N(0.0)], N(1.0), true, None),
            ([N(-0.0), N(0.0), N(-0.0)], N(-0.0), true, None),
            (
                [N(f64::INFINITY), N(f64::INFINITY), N(1.0)],
                N(f64::INFINITY),
                true,
                None,
            ),
            (
                [N(f64::NEG_INFINITY), N(0.0), N(1.0)],
                N(f64::NEG_INFINITY),
                true,
                None,
            ),
            (
                [N(f64::INFINITY), N(f64::INFINITY), N(f64::NEG_INFINITY)],
                N(f64::NAN),
                false,
                None,
            ),
            ([N(f64::NAN), N(1.0), N(1.0)], N(f64::NAN), false, None),
            ([N(0.0), N(1.0), N(f64::NAN)], N(f64::NAN), false, None),
            ([I(0), N(f64::NAN), I(1)], I(1), false, None),
            ([zero, I(2), I(1)], N(1.0), true, None),
            ([I(0), I(2), step], N(-0.5), false, None),
            (
                [N(0.0), bad, N(1.0)],
                N(0.0),
                false,
                Some(("number", "string", "number")),
            ),
            (
                [I(0), bad, I(1)],
                I(0),
                false,
                Some(("integer", "string", "integer")),
            ),
            (
                [I(0), I(1), Nil],
                I(0),
                false,
                Some(("number", "number", "nil")),
            ),
            (
                [I(i64::MAX), N(f64::INFINITY), I(1)],
                I(i64::MIN),
                false,
                None,
            ),
        ];
        for base in [0, 1, 252] {
            for (input, next, continues, error) in cases {
                check(ctx, base, true, input, next, continues, error);
            }
        }
    });
}
