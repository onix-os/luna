use super::*;
use crate::{opcode::Operation, types::VarCount, Lua};

#[test]
fn snapshot_windows_refuse_invalid_dynamic_state_without_mutation() {
    Lua::empty().enter(|ctx| {
        let caller = Closure::load(
            ctx,
            None,
            b"local n=7 local function f(v) n=n+v end f(2) return n",
        )
        .unwrap();
        let wrong = Closure::load(ctx, None, b"return 0").unwrap();
        let (call_pc, function, arguments) = caller
            .prototype()
            .opcodes
            .iter()
            .enumerate()
            .find_map(|(pc, op)| match op.decode() {
                Operation::Call { func, args, .. } => Some((pc, func, args.to_constant().unwrap())),
                _ => None,
            })
            .unwrap();
        let mut accepted = 0;
        for case in 0..19 {
            with_test_thread(ctx, caller, &mut Fuel::with(10000), |host| {
                host.run(ctx, 1, call_pc as u32, 4).result.unwrap();
                host.state.frames.reserve(8);
                let mut spec = host.with_registers(|_, registers| {
                    let Value::Function(Function::Closure(callee)) =
                        registers.stack_frame[usize::from(function.0)]
                    else {
                        panic!()
                    };
                    let crate::jit::projection::Origin::Register(capture, _) = registers
                        .projection_origin(callee.upvalues()[0].get())
                        .unwrap()
                    else {
                        panic!()
                    };
                    let Operation::GetUpValue { dest: read, .. } =
                        callee.prototype().opcodes[0].decode()
                    else {
                        panic!()
                    };
                    let Operation::Add { dest: result, .. } =
                        callee.prototype().opcodes[1].decode()
                    else {
                        panic!()
                    };
                    let Operation::Return { start, .. } = callee.prototype().opcodes[3].decode()
                    else {
                        panic!()
                    };
                    Spec {
                        callee,
                        function,
                        arguments,
                        capture: (false, capture),
                        read,
                        result,
                        start,
                        prefix: 0,
                    }
                });
                let mut width = usize::from(caller.prototype().stack_size);
                let mut source = caller;
                let mut pc = call_pc;
                match case {
                    0 => {}
                    1 => host.state.max_call_depth = host.state.frames.len(),
                    2 => host.state.frames.shrink_to_fit(),
                    3 => host.test_variable_stack(),
                    4 => {
                        let Some(Frame::Lua {
                            expected_return, ..
                        }) = host.state.frames.last_mut()
                        else {
                            panic!()
                        };
                        *expected_return = Some(super::super::super::thread::LuaReturn::Normal(
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
                    7 => width += 1,
                    8 => spec.prefix = 65,
                    9 => host.fuel.set_remaining(0),
                    10 => host.fuel.interrupt(),
                    11 => host.fuel.set_remaining(i32::MIN),
                    12 => spec.capture.1 = usize::from(function.0),
                    13 => spec.capture = (true, 0),
                    14 => pc = usize::MAX,
                    15 => source = wrong,
                    16 => spec.prefix = 64,
                    17 => host.state.to_be_closed.push(usize::from(function.0)),
                    18 => host.with_registers(|_, mut registers| {
                        registers.open_test_upvalue(&ctx, function);
                    }),
                    _ => unreachable!(),
                }
                let before = format!("{:?}", host.test_trace());
                let interrupted = host.fuel.is_interrupted();
                let original = host
                    .snapshot_window(&spec, source, width, pc)
                    .map(|window| {
                        (
                            window.pc,
                            window.upper,
                            window.fuel.remaining(),
                            window.fuel.is_interrupted(),
                        )
                    });
                assert_eq!(original.is_some(), matches!(case, 0 | 16), "case={case}");
                accepted += usize::from(original.is_some());
                assert_eq!(format!("{:?}", host.test_trace()), before, "case={case}");
                assert_eq!(host.fuel.is_interrupted(), interrupted);
            });
        }
        assert_eq!(accepted, 2);
    });
}
