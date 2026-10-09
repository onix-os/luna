use super::*;
use crate::{types::RegisterIndex, Closure, Function, Value};

pub(crate) struct Spec<'gc> {
    pub callee: Closure<'gc>,
    pub function: RegisterIndex,
    pub arguments: u8,
    pub capture: (bool, usize),
    pub read: RegisterIndex,
    pub result: RegisterIndex,
    pub start: RegisterIndex,
    pub prefix: u32,
}

pub(crate) struct Window<'gc, 'borrow, 'host> {
    host: &'borrow mut ActivationHost<'gc, 'host>,
    fuel: Fuel,
    upper: Option<usize>,
    pc: usize,
}

impl Window<'_, '_, '_> {
    pub(crate) fn commit(self, captured: i64) {
        if let Some(index) = self.upper {
            self.host.stack[index] = Value::Integer(captured);
        }
        let Some(Frame::Lua { pc, .. }) = self.host.state.frames.last_mut() else {
            unreachable!()
        };
        *pc = self.pc + 1;
        *self.host.fuel = self.fuel;
    }
}

impl<'gc, 'host> ActivationHost<'gc, 'host> {
    pub(crate) fn snapshot_window(
        &mut self,
        spec: &Spec<'gc>,
        caller: Closure<'gc>,
        width: usize,
        pc: usize,
    ) -> Option<Window<'gc, '_, 'host>> {
        let Some(Frame::Lua {
            base,
            closure,
            stack_size,
            is_variable: false,
            expected_return: None,
            ..
        }) = self.state.frames.last()
        else {
            return None;
        };
        if *closure != caller
            || *stack_size != width
            || width > 256
            || pc >= caller.prototype().opcodes.len()
        {
            return None;
        }
        let base = *base;
        let function = usize::from(spec.function.0);
        let bottom = base + function;
        let count = usize::from(spec.arguments);
        let callee_width = usize::from(spec.callee.prototype().stack_size);
        let capture = if spec.capture.0 {
            if spec.capture.1 >= base {
                return None;
            }
            spec.capture.1
        } else {
            base.checked_add(spec.capture.1)?
        };
        if self.state.frames.len() >= self.state.max_call_depth
            || self.state.frames.len() == self.state.frames.capacity()
            || self.stack.len() != base + width
            || function >= width
            || count > width - function - 1
            || spec.arguments > spec.callee.prototype().fixed_params
            || count > callee_width
            || bottom + callee_width > self.stack.capacity()
            || capture >= bottom
            || usize::from(spec.read.0) >= callee_width
            || usize::from(spec.result.0) >= callee_width
            || usize::from(spec.start.0) > callee_width
            || spec.prefix > 64
            || !self.state.captures_below(bottom)
        {
            return None;
        }
        let mut fuel = self.fuel.clone();
        fuel.consume(LuaFrame::FUEL_PER_CALL);
        fuel.consume(crate::fuel::count_fuel(LuaFrame::FUEL_PER_ITEM, count));
        fuel.consume(4);
        fuel.consume(spec.prefix as i32);
        if !fuel.should_continue() {
            return None;
        }
        fuel.consume(LuaFrame::FUEL_PER_CALL);
        fuel.consume(3);
        fuel.consume(4);
        Some(Window {
            host: self,
            fuel,
            upper: spec.capture.0.then_some(capture),
            pc,
        })
    }
}

impl<'gc> ActivationHost<'gc, '_> {
    pub(crate) fn atomic_call(
        &mut self,
        spec: Spec<'gc>,
        invoke: impl FnOnce(&[Value<'gc>], i64) -> Option<(i64, i64, i64)>,
    ) -> bool {
        let Some(Frame::Lua {
            base,
            stack_size,
            is_variable: false,
            expected_return: None,
            pc,
            ..
        }) = self.state.frames.last()
        else {
            return false;
        };
        let (base, caller_top, pc) = (*base, *base + *stack_size, *pc);
        let bottom = base + usize::from(spec.function.0);
        let prototype = spec.callee.prototype();
        let count = usize::from(spec.arguments);
        let width = usize::from(prototype.stack_size);
        let top = bottom + width;
        let capture = if spec.capture.0 {
            if spec.capture.1 >= base {
                return false;
            }
            spec.capture.1
        } else {
            base + spec.capture.1
        };
        if self.state.frames.len() >= self.state.max_call_depth
            || self.state.frames.len() == self.state.frames.capacity()
            || self.stack.len() != caller_top
            || !matches!(self.stack.get(bottom), Some(Value::Function(Function::Closure(callee))) if *callee == spec.callee)
            || spec.arguments > prototype.fixed_params
            || count > width
            || bottom + 1 + count > self.stack.len()
            || top > self.stack.capacity()
            || capture >= bottom
            || usize::from(spec.read.0) >= width
            || usize::from(spec.result.0) >= width
            || usize::from(spec.start.0) > width
            || spec.prefix > 64
            || !self.state.captures_below(bottom)
        {
            return false;
        }
        let Some(Value::Integer(captured)) = self.stack.get(capture).copied() else {
            return false;
        };
        let mut fuel = self.fuel.clone();
        fuel.consume(LuaFrame::FUEL_PER_CALL);
        fuel.consume(crate::fuel::count_fuel(LuaFrame::FUEL_PER_ITEM, count));
        fuel.consume(4);
        fuel.consume(spec.prefix as i32);
        if !fuel.should_continue() {
            return false;
        }
        let Some((read, result, updated)) =
            invoke(&self.stack[bottom + 1..bottom + 1 + count], captured)
        else {
            return false;
        };
        self.stack
            .copy_within(bottom + 1..bottom + 1 + count, bottom);
        self.stack.resize(top, Value::Nil);
        self.stack[bottom + count..top].fill(Value::Nil);
        self.stack[bottom + usize::from(spec.read.0)] = Value::Integer(read);
        self.stack[bottom + usize::from(spec.result.0)] = Value::Integer(result);
        self.stack[capture] = Value::Integer(updated);
        self.stack.resize(caller_top, Value::Nil);
        let Some(Frame::Lua { pc: current, .. }) = self.state.frames.last_mut() else {
            unreachable!()
        };
        *current = pc + 1;
        fuel.consume(LuaFrame::FUEL_PER_CALL);
        fuel.consume(3);
        fuel.consume(4);
        *self.fuel = fuel;
        true
    }
}

#[test]
fn guards_and_precommit_refusal_leave_canonical_state_untouched() {
    use crate::{jit::projection::Origin, opcode::Operation, Lua};
    let mut lua = Lua::empty();
    lua.enter(|ctx| {
        let root = Closure::load(
            ctx,
            None,
            &b"local n=7 local function f(v) n=n+v end f(2) return n"[..],
        )
        .unwrap();
        let (pc, function, arguments) = root
            .prototype()
            .opcodes
            .iter()
            .enumerate()
            .find_map(|(pc, opcode)| match opcode.decode() {
                Operation::Call { func, args, .. } => Some((pc, func, args.to_constant().unwrap())),
                _ => None,
            })
            .unwrap();
        for case in 0..21 {
            with_test_thread(ctx, root, &mut Fuel::with(10000), |host| {
                host.run(ctx, 1, pc as u32, 4).result.unwrap();
                host.state.frames.reserve(8);
                let mut spec = host.with_registers(|_, registers| {
                    let Value::Function(Function::Closure(callee)) =
                        registers.stack_frame[usize::from(function.0)]
                    else {
                        panic!()
                    };
                    let Origin::Register(capture, _) = registers
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
                match case {
                    0 | 20 => {}
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
                        *expected_return = Some(super::super::thread::LuaReturn::Normal(
                            crate::types::VarCount::constant(0),
                        ));
                    }
                    5 => spec.arguments = 255,
                    6 => spec.function = RegisterIndex(255),
                    7 => spec.capture.1 = usize::from(function.0),
                    8 => spec.capture = (true, 0),
                    9 => spec.read = RegisterIndex(255),
                    10 => spec.result = RegisterIndex(255),
                    11 => spec.start = RegisterIndex(255),
                    12 => spec.prefix = 65,
                    13 => host.fuel.set_remaining(0),
                    14 => host.fuel.interrupt(),
                    15 => host.stack[spec.capture.1] = Value::Number(7.5),
                    16 => host.with_registers(|_, mut registers| {
                        registers.open_test_upvalue(&ctx, function);
                    }),
                    17 => host.state.to_be_closed.push(usize::from(function.0)),
                    18 => {
                        host.stack.pop();
                    }
                    19 => spec.callee = Closure::load(ctx, None, &b"return"[..]).unwrap(),
                    _ => unreachable!(),
                }
                let before = format!("{:?}", host.test_trace());
                let markers = host.state.to_be_closed.clone();
                let capacities = (host.stack.capacity(), host.state.frames.capacity());
                let mut entered = 0;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    host.atomic_call(spec, |_, _| {
                        entered += 1;
                        if case == 20 {
                            panic!("before publication");
                        }
                        None
                    })
                }));
                if case == 20 {
                    assert!(result.is_err());
                } else {
                    assert!(!result.unwrap());
                }
                assert_eq!(entered, usize::from(case == 0 || case == 20), "case={case}");
                assert_eq!(format!("{:?}", host.test_trace()), before, "case={case}");
                assert_eq!(host.state.to_be_closed.as_slice(), markers.as_slice());
                assert_eq!(
                    (host.stack.capacity(), host.state.frames.capacity()),
                    capacities
                );
            });
        }
    });
}
