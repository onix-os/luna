use std::{
    cell::Cell,
    panic::{catch_unwind, AssertUnwindSafe},
};

use allocator_api2::SliceExt;
use ottavino_gc_arena::allocator_api::MetricsAlloc;

use crate::{
    opcode::{OpCode, Operation, RCIndex},
    thread::thread::{Frame, LuaFrame},
    types::{ConstantIndex8, RegisterIndex},
    Callback, CallbackReturn, Closure, Context, Fuel, FunctionPrototype, Lua, Table, Thread, Value,
};

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
}

pub(super) fn enabled() -> bool {
    ENABLED.with(Cell::get)
}

fn scoped<R>(enabled: bool, run: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            ENABLED.with(|state| state.set(self.0));
        }
    }
    let restore = Restore(ENABLED.with(|state| state.replace(enabled)));
    let result = run();
    drop(restore);
    result
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Completed(u32),
    Error(String),
    Panic(String),
}

#[derive(Debug)]
struct Snapshot<'gc> {
    outcome: Outcome,
    frames: String,
    stack: Vec<Value<'gc>>,
    remaining: i32,
    interrupted: bool,
    #[cfg(feature = "jit")]
    dispatches: (u64, u64, u64),
}

fn run<'gc>(
    ctx: Context<'gc>,
    closure: Closure<'gc>,
    values: &[Value<'gc>],
    budget: u32,
    interrupted: bool,
    hook: Option<(Callback<'gc>, u32, bool)>,
) -> Snapshot<'gc> {
    if let Some((hook, count, _)) = hook {
        ctx.set_debug_hook(hook.into(), false, count);
    }
    let thread = Thread::new(ctx);
    thread.start(ctx, closure.into(), ()).unwrap();
    let mut state = thread.into_inner().borrow_mut(&ctx);
    let base = match state.frames[0] {
        Frame::Lua { base, .. } => base,
        _ => panic!("missing Lua frame"),
    };
    if hook.is_some_and(|(_, _, resume)| resume) {
        state.frames.push(Frame::HookResume);
    }
    let storage = state.stack;
    let mut stack = storage.borrow_mut(&ctx);
    stack.truncate(base + values.len());
    stack[base..].copy_from_slice(values);
    let mut fuel = Fuel::with(19);
    if interrupted {
        fuel.interrupt();
    }
    #[cfg(feature = "jit")]
    let before = ctx.jit().0.borrow().stats;
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        super::run_vm(
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
            budget,
        )
    }));
    #[cfg(feature = "jit")]
    drop(stack);
    let outcome = match outcome {
        Ok(Ok(count)) => Outcome::Completed(count),
        Ok(Err(error)) => Outcome::Error(format!("{error:?}")),
        Err(error) => Outcome::Panic(
            error
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| error.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .expect("string panic payload"),
        ),
    };
    #[cfg(feature = "jit")]
    let dispatches = {
        let mut after = ctx.jit().0.borrow().stats;
        let counts = (
            after.total_dispatches - before.total_dispatches,
            after.interpreted_slices - before.interpreted_slices,
            after.interpreted_instructions - before.interpreted_instructions,
        );
        after.total_dispatches = before.total_dispatches;
        after.interpreted_slices = before.interpreted_slices;
        after.interpreted_instructions = before.interpreted_instructions;
        assert_eq!(after, before);
        counts
    };
    Snapshot {
        outcome,
        frames: format!("{:?}", state.frames),
        stack: storage.borrow().to_vec(),
        remaining: fuel.remaining(),
        interrupted: fuel.is_interrupted(),
        #[cfg(feature = "jit")]
        dispatches,
    }
}

fn compare(actual: Snapshot<'_>, expected: Snapshot<'_>) {
    assert_eq!(actual.outcome, expected.outcome);
    assert_eq!(actual.frames, expected.frames);
    assert_eq!(actual.remaining, expected.remaining);
    assert_eq!(actual.interrupted, expected.interrupted);
    #[cfg(feature = "jit")]
    assert_eq!(actual.dispatches, expected.dispatches);
    assert_eq!(actual.stack.len(), expected.stack.len());
    for (actual, expected) in actual.stack.into_iter().zip(expected.stack) {
        match (actual, expected) {
            (Value::Number(a), Value::Number(b)) => {
                assert!(a.to_bits() == b.to_bits() || a.is_nan() && b.is_nan());
            }
            (a, b) => assert_eq!(
                format!("{:?}", a.debug_shallow()),
                format!("{:?}", b.debug_shallow())
            ),
        }
    }
}

#[derive(Clone, Copy)]
struct Case<'gc> {
    operands: [Value<'gc>; 2],
    controls: [Value<'gc>; 3],
    base: u8,
    dest: u8,
    constants: u8,
    visible: bool,
    jump: i16,
}

fn fixture<'gc>(ctx: Context<'gc>, case: Case<'gc>) -> (Closure<'gc>, Vec<Value<'gc>>) {
    let mut prototype = FunctionPrototype::compile(ctx, "numeric-pair", b"return").unwrap();
    let width = usize::from(case.base) + 5 + usize::from(case.visible);
    prototype.stack_size = width.try_into().unwrap();
    if case.constants != 0 {
        prototype.constants = SliceExt::to_vec_in(
            &case.operands.map(|v| v.to_constant().unwrap())[..],
            MetricsAlloc::new(&ctx),
        )
        .into_boxed_slice();
    }
    let operand = |i| {
        if case.constants & (1u8 << i) == 0 {
            RCIndex::Register(RegisterIndex(case.base + i))
        } else {
            RCIndex::Constant(ConstantIndex8(i))
        }
    };
    prototype.opcodes = SliceExt::to_vec_in(
        &[
            OpCode::encode(Operation::Add {
                dest: RegisterIndex(case.base + case.dest),
                left: operand(0),
                right: operand(1),
            }),
            OpCode::encode(Operation::NumericForLoop {
                base: RegisterIndex(case.base + 2),
                jump: case.jump,
            }),
        ][..],
        MetricsAlloc::new(&ctx),
    )
    .into_boxed_slice();
    let closure = Closure::new(&ctx, prototype, Some(ctx.globals())).unwrap();
    let mut values = vec![Value::Integer(-999); width];
    let base = usize::from(case.base);
    values[base..base + 2].copy_from_slice(&case.operands);
    values[base + 2..base + 5].copy_from_slice(&case.controls);
    (closure, values)
}

fn check<'gc>(ctx: Context<'gc>, case: Case<'gc>, budget: u32, interrupted: bool) {
    let (closure, values) = fixture(ctx, case);
    let expected = scoped(false, || {
        run(ctx, closure, &values, budget, interrupted, None)
    });
    let actual = scoped(true, || {
        run(ctx, closure, &values, budget, interrupted, None)
    });
    compare(actual, expected);
}

fn off() -> Lua {
    let mut lua = Lua::empty();
    #[cfg(feature = "jit")]
    lua.set_jit_config(crate::JitConfig {
        mode: crate::JitMode::Off,
        ..Default::default()
    })
    .unwrap();
    #[cfg(not(feature = "jit"))]
    let _ = &mut lua;
    lua
}

#[test]
fn selector_restores_nested_and_panicking_scopes() {
    assert!(!enabled());
    scoped(true, || {
        assert!(enabled());
        scoped(false, || assert!(!enabled()));
        assert!(enabled());
        assert!(catch_unwind(|| scoped(false, || panic!("selector"))).is_err());
        assert!(enabled());
    });
    assert!(!enabled());
}

#[test]
fn pair_prefixes_preserve_aliases_operands_controls_and_errors() {
    off().enter(|ctx| {
        use Value::{Integer as I, Nil, Number as N};
        let text = Value::String(ctx.intern(b"2"));
        let bad = Value::String(ctx.intern(b"bad"));
        let cases = [
            ([I(7), I(3)], [I(0), I(8), I(1)]),
            ([I(i64::MAX), I(1)], [I(i64::MAX), I(i64::MAX), I(1)]),
            ([I(i64::MIN), I(-1)], [I(i64::MIN), I(i64::MIN), I(-1)]),
            ([N(-0.0), N(-0.0)], [N(-0.0), N(0.0), N(-0.0)]),
            ([N(0.5), I(1)], [N(0.0), I(1), N(0.5)]),
            ([N(f64::NAN), I(1)], [N(1.0), N(f64::NAN), N(1.0)]),
            ([text, I(1)], [text, I(8), I(1)]),
            ([bad, I(1)], [I(0), I(8), I(1)]),
            ([Nil, I(1)], [I(0), I(8), I(1)]),
            ([I(7), I(3)], [I(0), Nil, I(1)]),
        ];
        for base in [0, 250] {
            for dest in 0..6 {
                for constants in 0..4 {
                    for budget in 0..=2 {
                        for (index, (operands, controls)) in cases.into_iter().enumerate() {
                            if cfg!(miri)
                                && (dest % 2 != 0
                                    || constants == 1
                                    || constants == 2
                                    || index % 3 != 0)
                            {
                                continue;
                            }
                            check(
                                ctx,
                                Case {
                                    operands,
                                    controls,
                                    base,
                                    dest,
                                    constants,
                                    visible: true,
                                    jump: -2,
                                },
                                budget,
                                false,
                            );
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn pair_missing_visible_slot_and_checked_jump_preserve_partial_writes() {
    off().enter(|ctx| {
        use Value::{Integer as I, Nil, Number as N};
        for controls in [
            [I(1), I(1), I(1)],
            [I(0), I(1), I(1)],
            [N(1.0), N(f64::NAN), N(1.0)],
            [I(0), Nil, I(1)],
        ] {
            for base in [0, 250] {
                for jump in [-3, -2, 0, i16::MAX] {
                    for budget in 0..=2 {
                        check(
                            ctx,
                            Case {
                                operands: [I(1), I(2)],
                                controls,
                                base,
                                dest: 0,
                                constants: 0,
                                visible: false,
                                jump,
                            },
                            budget,
                            true,
                        );
                    }
                }
            }
        }
    });
}

#[test]
fn pair_metamethods_stop_before_the_loop() {
    off().enter(|ctx| {
        let callback = Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return));
        let metatable = Table::new(&ctx);
        metatable.set(ctx, "__add", callback).unwrap();
        let table = Table::new(&ctx);
        table.set_metatable(ctx, Some(metatable));
        for budget in 0..=2 {
            check(
                ctx,
                Case {
                    operands: [table.into(), Value::Integer(1)],
                    controls: [Value::Integer(0), Value::Integer(8), Value::Integer(1)],
                    base: 0,
                    dest: 0,
                    constants: 0,
                    visible: true,
                    jump: -2,
                },
                budget,
                false,
            );
        }
    });
}

#[test]
fn pair_count_hook_and_resume_prefixes_match() {
    off().enter(|ctx| {
        let callback = Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return));
        for count in 1..=3 {
            for resume in [false, true] {
                for budget in 0..=2 {
                    let (closure, values) = fixture(
                        ctx,
                        Case {
                            operands: [Value::Integer(1), Value::Integer(2)],
                            controls: [Value::Integer(0), Value::Integer(8), Value::Integer(1)],
                            base: 0,
                            dest: 0,
                            constants: 0,
                            visible: true,
                            jump: -2,
                        },
                    );
                    let hook = Some((callback, count, resume));
                    let expected =
                        scoped(false, || run(ctx, closure, &values, budget, false, hook));
                    let actual = scoped(true, || run(ctx, closure, &values, budget, false, hook));
                    compare(actual, expected);
                }
            }
        }
    });
}
