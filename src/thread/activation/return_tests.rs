use super::*;
use crate::{
    closure::UpValueState, opcode::Operation, types::VarCount, Closure, JitConfig, JitMode, Lua,
    Value,
};
use std::panic::{catch_unwind, AssertUnwindSafe};

fn at_return<'gc>(
    ctx: Context<'gc>,
    host: &mut ActivationHost<'gc, '_>,
) -> (crate::types::RegisterIndex, u8) {
    for _ in 0..200 {
        let Frame::Lua { closure, pc, .. } = *host.state.frames.last().unwrap() else {
            panic!("expected Lua frame")
        };
        if host.state.frames.len() == 2 {
            if let Operation::Return { start, count } = closure.prototype().opcodes[pc].decode() {
                return (start, count.to_constant().unwrap());
            }
        }
        host.run(ctx, 1, 1, 4).result.unwrap();
    }
    panic!("did not reach callee return");
}

#[test]
fn fixed_returns_match_generic_frames_values_captures_and_fuel() {
    let sources: &[&[u8]] = &[
        b"local function f() return end f() return 5",
        b"local function f() return 41 end f() return 5",
        b"local function f() local a,b,c=11,22,33 return b,c,a end f() return 5",
        b"local x=9 local function f() local y=17 local function g() return x,y end return g end f() return x",
    ];
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Off,
        ..JitConfig::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        for source in sources {
            let closure = Closure::load(ctx, None, *source).unwrap();
            for available in [i32::MIN, -1, 0, 8, 10000] {
                for interrupted in [false, true] {
                    let run = |fast| {
                        with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                            let (start, count) = at_return(ctx, host);
                            host.stack.reserve(512);
                            let captures = host.state.open_upvalues.to_vec();
                            let Frame::Lua { bottom, base, .. } =
                                *host.state.frames.last().unwrap()
                            else {
                                unreachable!()
                            };
                            let returned = host.stack[base + usize::from(start.0)
                                ..base + usize::from(start.0) + usize::from(count)]
                                .to_vec();
                            let expected_captures: Vec<_> =
                                captures.iter().map(|u| format!("{:?}", u.get())).collect();
                            assert_eq!(captures.len(), if *source == sources[3] { 2 } else { 0 });
                            *host.fuel = Fuel::with(available);
                            if interrupted {
                                host.fuel.interrupt();
                            }
                            if fast {
                                assert!(host.return_in_place(ctx, start, count, 3));
                            } else {
                                host.return_fixed(ctx, start, count, 3).unwrap();
                            }
                            for (index, (capture, before)) in
                                captures.iter().zip(expected_captures).enumerate()
                            {
                                match capture.get() {
                                    UpValueState::Closed(actual) => {
                                        assert_eq!(index, 1);
                                        assert!(matches!(actual, Value::Integer(17)));
                                    }
                                    UpValueState::Open(_) => {
                                        assert_eq!(index, 0);
                                        assert_eq!(before, format!("{:?}", capture.get()));
                                    }
                                }
                            }
                            for (i, value) in returned.iter().enumerate() {
                                if bottom + i < host.stack.len() {
                                    assert_eq!(
                                        host.stack[bottom + i].display().to_string(),
                                        value.display().to_string()
                                    );
                                }
                            }
                            let (frames, stack, open, fuel) = host.test_trace();
                            let values: Vec<_> = stack
                                .iter()
                                .map(|v| match v {
                                    Value::Function(_) => "function".to_owned(),
                                    other => format!("{other:?}"),
                                })
                                .collect();
                            (frames, values, open, fuel, host.fuel.is_interrupted())
                        })
                    };
                    assert_eq!(
                        run(true),
                        run(false),
                        "source={source:?} fuel={available} interrupted={interrupted}"
                    );
                }
            }
        }
    });
}

#[test]
fn return_refusals_and_panics_preserve_generic_commit_order() {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Off,
        ..JitConfig::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            &b"local function f() return 42 end f() return 5"[..],
        )
        .unwrap();
        for case in 0..11 {
            let run = |fast| {
                with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                    let (mut start, mut count) = at_return(ctx, host);
                    let mut instructions = 3;
                    match case {
                        0 => {
                            if let Frame::Lua { is_variable, .. } =
                                host.state.frames.last_mut().unwrap()
                            {
                                *is_variable = true;
                            }
                        }
                        1 | 2 => {
                            if let Frame::Lua {
                                expected_return, ..
                            } = &mut host.state.frames[0]
                            {
                                *expected_return =
                                    Some(super::super::thread::LuaReturn::Normal(if case == 1 {
                                        VarCount::constant(1)
                                    } else {
                                        VarCount::variable()
                                    }));
                            }
                        }
                        3 => count = u8::MAX,
                        4 => start = crate::types::RegisterIndex(u8::MAX),
                        5 => instructions = u32::MAX,
                        6 => {
                            let capacity = host.stack.capacity();
                            if let Frame::Lua { stack_size, .. } = &mut host.state.frames[0] {
                                *stack_size = capacity + 8;
                            }
                        }
                        7 => {
                            host.state.frames.remove(0);
                        }
                        8 | 9 => {
                            if let Frame::Lua {
                                expected_return, ..
                            } = &mut host.state.frames[0]
                            {
                                *expected_return = if case == 8 {
                                    None
                                } else {
                                    Some(super::super::thread::LuaReturn::Meta(
                                        super::super::thread::MetaReturn::None,
                                    ))
                                };
                            }
                        }
                        10 => {
                            count = 0;
                            start = crate::types::RegisterIndex(u8::MAX);
                        }
                        _ => unreachable!(),
                    }
                    let before = format!("{:?}", host.test_trace());
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        if fast {
                            let accepted = host.return_in_place(ctx, start, count, instructions);
                            assert!(!accepted);
                            assert_eq!(before, format!("{:?}", host.test_trace()));
                        }
                        host.return_fixed(ctx, start, count, instructions)
                    }));
                    let result = match result {
                        Ok(r) => format!("{r:?}"),
                        Err(_) => "panic".to_owned(),
                    };
                    let (frames, stack, open, fuel) = host.test_trace();
                    let values: Vec<_> = stack
                        .iter()
                        .map(|v| match v {
                            Value::Function(_) => "function".to_owned(),
                            other => format!("{other:?}"),
                        })
                        .collect();
                    (result, frames, values, open, fuel)
                })
            };
            assert_eq!(run(true), run(false), "case={case}");
        }
    });
}

#[test]
fn capture_close_panic_keeps_the_committed_frame_and_fuel_transition() {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Off,
        ..JitConfig::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, &b"local function f() local y=17 local function g() return y end return g end f() return 5"[..]).unwrap();
        let run = |fast| with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
            let (start, count) = at_return(ctx, host);
            assert_eq!(host.state.open_upvalues.len(), 1);
            host.state.open_upvalues[0].set(&ctx, UpValueState::Closed(Value::Integer(17)));
            let before: Vec<_> = host.stack.iter().map(|v| v.display().to_string()).collect();
            let fuel = host.fuel.remaining();
            assert!(catch_unwind(AssertUnwindSafe(|| {
                if fast { host.return_in_place(ctx, start, count, 3); }
                else { host.return_fixed(ctx, start, count, 3).unwrap(); }
            })).is_err());
            assert_eq!(host.state.frames.len(), 1);
            assert_eq!(host.fuel.remaining(), fuel - LuaFrame::FUEL_PER_CALL);
            assert!(matches!(host.state.frames[0], Frame::Lua { expected_return: Some(_), .. }));
            assert_eq!(before, host.stack.iter().map(|v| v.display().to_string()).collect::<Vec<_>>());
            let (frames, _, open, remaining) = host.test_trace();
            (frames, open, remaining)
        });
        assert_eq!(run(true), run(false));
    });
}

impl<'gc, 'a> ActivationHost<'gc, 'a> {
    fn return_in_place(
        &mut self,
        ctx: Context<'gc>,
        start: crate::types::RegisterIndex,
        count: u8,
        instructions: u32,
    ) -> bool {
        use super::super::thread::LuaReturn;

        if self.state.frames.len() < 2 || count == u8::MAX {
            return false;
        }
        let depth = self.state.frames.len();
        let Frame::Lua {
            bottom,
            base,
            is_variable: false,
            ..
        } = self.state.frames[depth - 1]
        else {
            return false;
        };
        let Frame::Lua {
            base: caller_base,
            stack_size,
            expected_return: Some(LuaReturn::Normal(wanted)),
            ..
        } = self.state.frames[depth - 2]
        else {
            return false;
        };
        let source = base + usize::from(start.0);
        let count = usize::from(count);
        let top = caller_base + stack_size;
        if wanted.to_constant() != Some(0)
            || source + count > self.stack.len()
            || bottom + count > self.stack.len()
            || top > self.stack.capacity()
        {
            return false;
        }
        let Frame::Lua { pc, .. } = self.state.frames.last_mut().unwrap() else {
            unreachable!()
        };
        *pc += 1;
        self.state.frames.pop();
        self.fuel.consume(LuaFrame::FUEL_PER_CALL);
        self.state.close_upvalues(&ctx, &self.stack, bottom);
        self.fuel
            .consume(crate::fuel::count_fuel(LuaFrame::FUEL_PER_ITEM, count));
        if count != 0 {
            self.stack.copy_within(source..source + count, bottom);
        }
        let Frame::Lua {
            expected_return,
            is_variable,
            ..
        } = self.state.frames.last_mut().unwrap()
        else {
            unreachable!()
        };
        *expected_return = None;
        *is_variable = false;
        self.stack.resize(top, crate::Value::Nil);
        self.fuel.consume(instructions.try_into().unwrap());
        self.fuel.consume(4);
        true
    }
}
