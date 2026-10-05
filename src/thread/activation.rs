use std::cell::RefMut;

use crate::{Context, Fuel, ThreadMode};

use super::{
    thread::{Frame, FrameStack, LuaFrame, StackVec, ThreadState},
    vm::run_vm,
    VMError,
};

pub(super) struct ActivationHost<'gc, 'a> {
    state: &'a mut ThreadState<'gc>,
    stack: RefMut<'a, StackVec<'gc>>,
    fuel: &'a mut Fuel,
}

pub(super) struct Outcome {
    pub result: Result<(), VMError>,
    pub activations: usize,
    pub stack_growths: usize,
}

impl<'gc, 'a> ActivationHost<'gc, 'a> {
    pub fn new(
        state: &'a mut ThreadState<'gc>,
        stack: RefMut<'a, StackVec<'gc>>,
        fuel: &'a mut Fuel,
    ) -> Self {
        Self { state, stack, fuel }
    }

    fn with_frame<R>(&mut self, f: impl for<'frame> FnOnce(LuaFrame<'gc, 'frame>) -> R) -> R {
        f(LuaFrame {
            state: self.state,
            stack: FrameStack::Borrowed(&mut self.stack),
            fuel: self.fuel,
        })
    }

    pub fn run(&mut self, ctx: Context<'gc>, limit: usize, budget: u32, step_fuel: i32) -> Outcome {
        assert!(limit > 0);
        let mut activations = 0;
        let mut stack_growths = 0;
        loop {
            assert!(matches!(self.state.frames.last(), Some(Frame::Lua { .. })));
            let capacity = self.stack.capacity();
            let result = self.with_frame(|frame| run_vm(ctx, frame, budget));
            stack_growths += usize::from(self.stack.capacity() > capacity);
            activations += 1;
            let result =
                result.map(|instructions| self.fuel.consume(instructions.try_into().unwrap()));
            self.fuel.consume(step_fuel);
            if result.is_err()
                || activations == limit
                || !self.fuel.should_continue()
                || self.state.mode() != ThreadMode::Normal
                || !matches!(self.state.frames.last(), Some(Frame::Lua { .. }))
            {
                return Outcome {
                    result,
                    activations,
                    stack_growths,
                };
            }
        }
    }
}
