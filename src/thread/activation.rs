use std::cell::RefMut;

use crate::{Context, Fuel, ThreadMode};

use super::{
    thread::{Frame, LuaFrame, StackVec, ThreadState},
    vm::run_vm,
    VMError,
};

pub(crate) struct ActivationHost<'gc, 'a> {
    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    handoff: Option<crate::jit::PreparedPair>,
    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    select_pairs: bool,
    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[cfg(test)]
    decline_pairs: bool,
    state: &'a mut ThreadState<'gc>,
    stack: RefMut<'a, StackVec<'gc>>,
    fuel: &'a mut Fuel,
}

pub(crate) struct Outcome {
    pub result: Result<(), VMError>,
    #[cfg(test)]
    pub activations: usize,
    #[cfg(test)]
    pub stack_growths: usize,
}

impl<'gc, 'a> ActivationHost<'gc, 'a> {
    pub fn new(
        state: &'a mut ThreadState<'gc>,
        stack: RefMut<'a, StackVec<'gc>>,
        fuel: &'a mut Fuel,
    ) -> Self {
        Self {
            state,
            stack,
            fuel,
            #[cfg(all(
                not(miri),
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            handoff: None,
            #[cfg(all(
                not(miri),
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            select_pairs: false,
            #[cfg(all(
                not(miri),
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            #[cfg(test)]
            decline_pairs: false,
        }
    }

    fn with_frame<R>(&mut self, f: impl for<'frame> FnOnce(LuaFrame<'gc, 'frame>) -> R) -> R {
        f(LuaFrame {
            #[cfg(all(
                not(miri),
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            pair_handoff: self.select_pairs.then_some(&mut self.handoff),
            state: self.state,
            stack: &mut self.stack,
            fuel: self.fuel,
        })
    }
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
impl<'gc, 'a> ActivationHost<'gc, 'a> {
    pub(crate) fn clear_hook(&mut self, ctx: Context<'gc>) {
        self.with_frame(|frame| ctx.clear_hook_at(frame.frame_depth()));
    }

    pub(crate) fn with_registers<R>(
        &mut self,
        f: impl for<'frame> FnOnce(crate::Closure<'gc>, crate::thread::LuaRegisters<'gc, 'frame>) -> R,
    ) -> R {
        self.with_frame(|mut frame| f(frame.closure(), frame.registers()))
    }

    pub(crate) fn lua_ready(&self) -> bool {
        self.state.mode() == ThreadMode::Normal
            && matches!(
                self.state.frames.last(),
                Some(Frame::Lua {
                    is_variable: false,
                    ..
                })
            )
    }

    pub(crate) fn fuel(&self) -> &Fuel {
        self.fuel
    }

    pub(crate) fn charge_instructions(&mut self, instructions: u32) {
        self.fuel.consume(instructions.try_into().unwrap());
    }

    pub(crate) fn charge_native_slice(&mut self, instructions: u32) {
        self.fuel.consume(instructions.try_into().unwrap());
        self.fuel.consume(4);
    }

    pub(crate) fn call(
        &mut self,
        ctx: Context<'gc>,
        function: crate::types::RegisterIndex,
        arguments: u8,
    ) -> Result<(), VMError> {
        let result = self.with_frame(|mut frame| {
            *frame.registers().pc += 1;
            frame.call_function(
                ctx,
                function,
                crate::types::VarCount::constant(arguments),
                crate::types::VarCount::constant(0),
            )
        });
        self.fuel.consume(4);
        result
    }

    pub(crate) fn return_fixed(
        &mut self,
        ctx: Context<'gc>,
        start: crate::types::RegisterIndex,
        count: u8,
        instructions: u32,
    ) -> Result<(), VMError> {
        let result = self.with_frame(|mut frame| {
            *frame.registers().pc += 1;
            frame.return_upper(&ctx, start, crate::types::VarCount::constant(count))
        });
        if result.is_ok() {
            self.fuel.consume(instructions.try_into().unwrap());
        }
        self.fuel.consume(4);
        result
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[cfg(test)]
    pub(crate) fn test_trace(&self) -> (Vec<String>, Vec<crate::Value<'gc>>, usize, i32) {
        let frames = self
            .state
            .frames
            .iter()
            .map(|frame| match frame {
                Frame::Lua {
                    bottom,
                    base,
                    pc,
                    is_variable,
                    stack_size,
                    expected_return,
                    ..
                } => format!("{bottom}:{base}:{pc}:{is_variable}:{stack_size}:{expected_return:?}"),
                frame => format!("{frame:?}"),
            })
            .collect();
        (
            frames,
            self.stack.to_vec(),
            self.state.open_upvalues.len(),
            self.fuel.remaining(),
        )
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[cfg(test)]
    pub(crate) fn test_fuel(&mut self, fuel: Fuel) {
        *self.fuel = fuel;
    }

    #[cfg(test)]
    pub(crate) fn test_capacity(&self) -> usize {
        self.stack.capacity()
    }

    #[cfg(test)]
    pub(crate) fn test_pair_selection(&mut self, enabled: bool) {
        self.decline_pairs = !enabled;
    }

    #[cfg(test)]
    pub(crate) fn test_variable_stack(&mut self) {
        let Some(Frame::Lua { is_variable, .. }) = self.state.frames.last_mut() else {
            panic!();
        };
        *is_variable = true;
    }
}

impl<'gc, 'a> ActivationHost<'gc, 'a> {
    pub fn run(&mut self, ctx: Context<'gc>, limit: usize, budget: u32, step_fuel: i32) -> Outcome {
        assert!(limit > 0);
        let mut activations = 0;
        #[cfg(test)]
        let mut stack_growths = 0;
        loop {
            assert!(matches!(self.state.frames.last(), Some(Frame::Lua { .. })));
            #[cfg(test)]
            let capacity = self.stack.capacity();
            #[cfg(all(
                not(miri),
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            {
                assert!(self.handoff.is_none());
                self.select_pairs = limit - activations >= 2
                    && budget >= 4
                    && step_fuel == 4
                    && ctx.jit().call_pairs_enabled();
                #[cfg(test)]
                {
                    self.select_pairs &= !self.decline_pairs;
                }
            }
            let result = self.with_frame(|frame| run_vm(ctx, frame, budget));
            activations += 1;
            #[cfg(all(
                not(miri),
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            let (result, step_charged) = if let Some(pair) = self.handoff.take() {
                let prefix = result.expect("pair handoff with VM error");
                assert!(prefix < budget);
                self.select_pairs = false;
                if let Some(outcome) = pair.invoke(ctx, self, budget, prefix) {
                    activations += outcome.returns;
                    (outcome.result.map(|()| 0), true)
                } else {
                    (
                        self.with_frame(|frame| run_vm(ctx, frame, budget - prefix))
                            .map(|instructions| prefix + instructions),
                        false,
                    )
                }
            } else {
                (result, false)
            };
            #[cfg(test)]
            {
                stack_growths += usize::from(self.stack.capacity() > capacity);
            }
            let result =
                result.map(|instructions| self.fuel.consume(instructions.try_into().unwrap()));
            #[cfg(all(
                not(miri),
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            if !step_charged {
                self.fuel.consume(step_fuel);
            }
            #[cfg(not(all(
                not(miri),
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            )))]
            self.fuel.consume(step_fuel);
            if result.is_err()
                || activations == limit
                || !self.fuel.should_continue()
                || self.state.mode() != ThreadMode::Normal
                || !matches!(self.state.frames.last(), Some(Frame::Lua { .. }))
            {
                return Outcome {
                    result,
                    #[cfg(test)]
                    activations,
                    #[cfg(test)]
                    stack_growths,
                };
            }
        }
    }
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[cfg(test)]
pub(crate) fn with_test_thread<'gc, R>(
    ctx: Context<'gc>,
    closure: crate::Closure<'gc>,
    fuel: &mut Fuel,
    test: impl FnOnce(&mut ActivationHost<'gc, '_>) -> R,
) -> R {
    let thread = crate::Thread::new(ctx);
    thread.start(ctx, closure.into(), ()).unwrap();
    with_test_existing_thread(ctx, thread, fuel, test)
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[cfg(test)]
pub(crate) fn with_test_existing_thread<'gc, R>(
    ctx: Context<'gc>,
    thread: crate::Thread<'gc>,
    fuel: &mut Fuel,
    test: impl FnOnce(&mut ActivationHost<'gc, '_>) -> R,
) -> R {
    let inner = thread.into_inner();
    let mut state = inner.borrow_mut(&ctx);
    let stack = state.stack;
    let mut host = ActivationHost::new(&mut state, stack.borrow_mut(&ctx), fuel);
    test(&mut host)
}
