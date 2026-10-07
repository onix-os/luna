use super::*;
use crate::{opcode::Operation, Closure, JitConfig, JitMode, Lua, Value};

#[test]
fn in_place_growth_refusal_preserves_caller_before_generic_allocation() {
    let locals = (0..64)
        .map(|i| format!("v{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let source = format!("local function f() local {locals} return {locals} end f() return 5");
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Off,
        ..JitConfig::default()
    })
    .unwrap();
    lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes()).unwrap();
        let (pc, function) = closure
            .prototype()
            .opcodes
            .iter()
            .enumerate()
            .find_map(|(pc, op)| {
                if let Operation::Call { func, .. } = op.decode() {
                    Some((pc, func))
                } else {
                    None
                }
            })
            .unwrap();
        let run = |fast| {
            with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                host.run(ctx, 1, pc as u32, 4).result.unwrap();
                host.state.frames.reserve(8);
                host.stack.shrink_to_fit();
                let capacity = host.stack.capacity();
                if fast {
                    let before = format!("{:?}", host.test_trace());
                    assert!(!host.call_in_place(function, 0));
                    assert_eq!(before, format!("{:?}", host.test_trace()));
                    host.call(ctx, function, 0).unwrap();
                } else {
                    host.call_generic(ctx, function, 0).unwrap();
                }
                assert!(host.stack.capacity() > capacity);
                let (frames, values, open, fuel) = host.test_trace();
                (
                    frames,
                    values
                        .iter()
                        .copied()
                        .map(Value::type_name)
                        .collect::<Vec<_>>(),
                    open,
                    fuel,
                )
            })
        };
        assert_eq!(run(true), run(false));
    });
}

#[test]
fn in_place_calls_match_generic_frames_slots_fuel_and_refusals() {
    let sources: &[&[u8]] = &[
        b"local function f(a,b,c) local d=7 return a,b,c,d end f(1) return 5",
        b"local function f(a,b,c) return a,b,c end f(1,2,3) return 5",
        b"local function f() return end f() return 5",
        b"local function f(a,...) return a,... end f(1,2,3) return 5",
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
            let (pc, function, arguments) = closure.prototype().opcodes.iter().enumerate().find_map(|(pc, op)| {
                match op.decode() {
                    Operation::Call { func, args, .. } => Some((pc, func, args.to_constant().unwrap())),
                    _ => None,
                }
            }).unwrap();
            for refusal in 0..3 {
                for available in [i32::MIN, -1, 0, 8, 9, 10000] {
                    for interrupted in [false, true] {
                        let run = |fast: bool| with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                            host.run(ctx, 1, pc as u32, 4).result.unwrap();
                            host.stack.reserve(512);
                            host.state.frames.reserve(8);
                            if refusal == 1 {
                                host.state.max_call_depth = host.state.frames.len();
                            } else if refusal == 2 {
                                host.state.frames.shrink_to_fit();
                            }
                            *host.fuel = Fuel::with(available);
                            if interrupted { host.fuel.interrupt(); }
                            if fast {
                                let before = format!("{:?}", host.test_trace());
                                let accepted = host.call_in_place(function, arguments);
                                assert_eq!(accepted, refusal == 0 && arguments <= 3 && *source != sources[3]);
                                if !accepted {
                                    assert_eq!(before, format!("{:?}", host.test_trace()));
                                    host.call_generic(ctx, function, arguments).unwrap();
                                }
                            } else {
                                host.call_generic(ctx, function, arguments).unwrap();
                            }
                            let (frames, stack, open, fuel) = host.test_trace();
                            let values: Vec<_> = stack.iter().map(|value| match value {
                                Value::Function(_) => "function".to_owned(),
                                other => format!("{other:?}"),
                            }).collect();
                            (frames, values, open, fuel, host.fuel.is_interrupted())
                        });
                        assert_eq!(run(true), run(false), "source={source:?} refusal={refusal} fuel={available} interrupted={interrupted}");
                    }
                }
            }
        }
    });
}
