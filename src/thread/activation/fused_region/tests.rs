use super::*;
use crate::{
    closure::{UpValue, UpValueState},
    compiler,
    jit::call_fusion::runtime::Request,
    types::{RegisterIndex, VarCount},
    Callback, CallbackReturn, Closure, Function, FunctionPrototype, JitConfig, JitMode, Lua, Value,
};

fn caller(ctx: Context<'_>) -> Closure<'_> {
    let mut interner = compiler::interning::BasicInterner::default();
    let parsed = compiler::parse_chunk(
        b"local sum=11 local function add(v) sum=sum+v end add(7) return sum",
        &mut interner,
    )
    .unwrap();
    let mut compiled = compiler::compile_chunk(&parsed, &mut interner).unwrap();
    compiled.prototypes[0].stack_size = 8;
    let compiled = compiled.map_strings(|s| ctx.intern(s.as_ref()));
    let proto = FunctionPrototype::from_compiled(
        &ctx,
        ctx.intern(b"fusion-frame-guards"),
        &compiled,
        false,
    );
    let closure = Closure::new(&ctx, proto, Some(ctx.globals())).unwrap();
    ctx.jit_registry()
        .borrow_mut(&ctx)
        .register(ctx, closure.prototype());
    closure
}

#[test]
fn preflight_refusals_preserve_frames_values_fuel_and_capacities() {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        ..Default::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        for case in 0..19 {
            let caller = caller(ctx);
            let prepared = Request::new(ctx, caller)
                .unwrap()
                .unwrap()
                .compile()
                .unwrap();
            let region = prepared.region();
            with_test_thread(ctx, caller, &mut Fuel::with(100000), |host| {
                host.run(ctx, 1, region.entry as u32, 4).result.unwrap();
                host.state.frames.reserve(8);
                host.stack.reserve(32);
                match case {
                    0 => host.state.max_call_depth = host.state.frames.len(),
                    1 => host.state.frames.shrink_to_fit(),
                    2 => host.stack.shrink_to_fit(),
                    3 => host.test_variable_stack(),
                    4 => {
                        let Some(Frame::Lua {
                            expected_return, ..
                        }) = host.state.frames.last_mut()
                        else {
                            panic!()
                        };
                        *expected_return = Some(crate::thread::thread::LuaReturn::Normal(
                            VarCount::constant(0),
                        ));
                    }
                    5 => {
                        host.stack.pop();
                    }
                    6 => {
                        let Some(Frame::Lua { stack_size, .. }) = host.state.frames.last_mut()
                        else {
                            panic!()
                        };
                        *stack_size += 1;
                    }
                    7 => {
                        let Some(Frame::Lua { pc, .. }) = host.state.frames.last_mut() else {
                            panic!()
                        };
                        *pc += 1;
                    }
                    8 => host.fuel.set_remaining(0),
                    9 => host.fuel.set_remaining(22),
                    10 => host.fuel.interrupt(),
                    11 => host.state.to_be_closed.push(region.function),
                    12 => host.with_registers(|_, mut regs| {
                        regs.open_test_upvalue(&ctx, RegisterIndex(region.function as u8));
                    }),
                    13 => {
                        let hook = Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return));
                        ctx.set_debug_hook(hook.into(), false, 1);
                    }
                    14 => {
                        let mut manager = ctx.jit().0.borrow_mut();
                        let mut config = manager.config.clone();
                        config.mode = JitMode::Off;
                        manager.configure(config);
                    }
                    15 => host.with_registers(|_, regs| {
                        regs.stack_frame[0] = Value::Number(11.0);
                    }),
                    16 => host.with_registers(|_, regs| {
                        regs.stack_frame[1] =
                            Callback::from_fn(&ctx, |_, _, _| unreachable!()).into();
                    }),
                    17 => host.with_registers(|_, regs| {
                        let Value::Function(Function::Closure(callee)) = regs.stack_frame[1] else {
                            panic!()
                        };
                        callee.set_upvalue(
                            &ctx,
                            0,
                            UpValue::new(&ctx, UpValueState::Closed(Value::Integer(11))),
                        );
                    }),
                    18 => ctx.jit_registry().borrow_mut(&ctx).reset(ctx),
                    _ => unreachable!(),
                }
                let before = format!("{:?}", host.test_trace());
                let capacities = (host.stack.capacity(), host.state.frames.capacity());
                let markers = host.state.to_be_closed.clone();
                let interrupted = host.fuel.is_interrupted();
                let entries = ctx.jit().0.borrow().stats.native_entries;
                let result =
                    host.with_frame(|mut frame| frame.try_fused_region(ctx, &prepared, 64, 0));
                assert_eq!(result, None, "case={case}");
                assert_eq!(format!("{:?}", host.test_trace()), before, "case={case}");
                assert_eq!(
                    (host.stack.capacity(), host.state.frames.capacity()),
                    capacities
                );
                assert_eq!(host.state.to_be_closed.as_slice(), markers.as_slice());
                assert_eq!(host.fuel.is_interrupted(), interrupted);
                assert_eq!(
                    ctx.jit().0.borrow().stats.native_entries,
                    entries,
                    "case={case}"
                );
                assert_eq!(prepared.executions(), 0);
            });
            ctx.set_debug_hook(Value::Nil, false, 0);
            if case == 14 {
                let mut manager = ctx.jit().0.borrow_mut();
                let mut config = manager.config.clone();
                config.mode = JitMode::Auto;
                manager.configure(config);
            }
        }
    });
}

#[test]
fn a_published_region_charges_only_extra_fuel_before_the_surviving_step() {
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Auto,
        ..Default::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        let caller = caller(ctx);
        let prepared = Request::new(ctx, caller)
            .unwrap()
            .unwrap()
            .compile()
            .unwrap();
        let region = prepared.region();
        with_test_thread(ctx, caller, &mut Fuel::with(100000), |host| {
            host.run(ctx, 1, region.entry as u32, 4).result.unwrap();
            host.state.frames.reserve(8);
            host.stack.reserve(32);
            let fuel = host.fuel.remaining();
            let capacity = (host.stack.capacity(), host.state.frames.capacity());
            let depth = host.state.frames.len();
            let result = host.with_frame(|mut frame| frame.try_fused_region(ctx, &prepared, 64, 0));
            assert_eq!(result, Some(5));
            assert_eq!(host.fuel.remaining(), fuel - 13);
            assert_eq!(
                (host.stack.capacity(), host.state.frames.capacity()),
                capacity
            );
            assert_eq!(host.state.frames.len(), depth);
            assert_eq!(prepared.executions(), 1);
            let Some(Frame::Lua {
                pc,
                expected_return,
                ..
            }) = host.state.frames.last()
            else {
                panic!()
            };
            assert_eq!(*pc, region.resume);
            assert!(expected_return.is_none());
            host.fuel.consume(5 + 4);
            assert_eq!(host.fuel.remaining(), fuel - 22);
        });
    });
}
