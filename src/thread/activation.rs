use std::cell::RefMut;

use crate::{Context, Fuel, ThreadMode};

use super::{
    thread::{Frame, LuaFrame, StackVec, ThreadState},
    vm::run_vm,
    VMError,
};

#[cfg(all(
    test,
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod call_tests;

#[cfg(all(
    test,
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(crate) mod atomic_call;

pub(crate) struct ActivationHost<'gc, 'a> {
    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    pair: crate::jit::PairScope<'gc>,
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

#[cfg(all(
    test,
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(crate) struct CallerFrame<'gc> {
    state: usize,
    depth: usize,
    closure: crate::Closure<'gc>,
    bottom: usize,
    base: usize,
    size: usize,
    pc: usize,
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
            pair: crate::jit::PairScope::default(),
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
            pair_handoff: self.select_pairs.then_some(&mut self.pair),
            state: self.state,
            stack: &mut self.stack,
            fuel: self.fuel,
        })
    }

    #[cfg(any(
        test,
        all(
            not(miri),
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    ))]
    pub(crate) fn with_registers<R>(
        &mut self,
        f: impl for<'frame> FnOnce(crate::Closure<'gc>, crate::thread::LuaRegisters<'gc, 'frame>) -> R,
    ) -> R {
        self.with_frame(|mut frame| f(frame.closure(), frame.registers()))
    }

    #[cfg(test)]
    pub(crate) fn test_capacity(&self) -> usize {
        self.stack.capacity()
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

    #[cfg(test)]
    pub(crate) fn caller_frame(&self) -> Option<CallerFrame<'gc>> {
        let Frame::Lua {
            closure,
            bottom,
            base,
            stack_size,
            pc,
            is_variable: false,
            ..
        } = self.state.frames.last()?
        else {
            return None;
        };
        Some(CallerFrame {
            state: std::ptr::from_ref(&*self.state) as usize,
            depth: self.state.frames.len(),
            closure: *closure,
            bottom: *bottom,
            base: *base,
            size: *stack_size,
            pc: *pc,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_caller_prefix<R>(
        &mut self,
        caller: &CallerFrame<'gc>,
        count: usize,
        call: impl FnOnce(crate::Closure<'gc>, (usize, usize), &mut [crate::Value<'gc>]) -> R,
    ) -> Option<R> {
        if caller.state != std::ptr::from_ref(&*self.state) as usize
            || !(caller.depth..=caller.depth.checked_add(1)?).contains(&self.state.frames.len())
            || count > caller.size
        {
            return None;
        }
        let Frame::Lua {
            closure,
            bottom,
            base,
            stack_size,
            pc,
            ..
        } = self.state.frames.get(caller.depth.checked_sub(1)?)?
        else {
            return None;
        };
        if !ottavino_gc_arena::Gc::ptr_eq(closure.into_inner(), caller.closure.into_inner())
            || (*bottom, *base, *stack_size, *pc)
                != (
                    caller.bottom,
                    caller.base,
                    caller.size,
                    caller.pc.checked_add(1)?,
                )
        {
            return None;
        }
        if self.state.frames.len() > caller.depth {
            match self.state.frames.last()? {
                Frame::Lua { bottom, .. } | Frame::Callback { bottom, .. }
                    if caller.base.checked_add(count)? <= *bottom => {}
                Frame::Error(_) => {}
                _ => return None,
            }
        }
        let key = (
            ottavino_gc_arena::Gc::as_ptr(self.state.stack) as usize,
            caller.base,
        );
        let values = self
            .stack
            .get_mut(caller.base..caller.base.checked_add(count)?)?;
        Some(call(*closure, key, values))
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

    #[cfg(test)]
    pub(crate) fn lua_pending(&self) -> bool {
        self.state.mode() == ThreadMode::Normal
            && matches!(self.state.frames.last(), Some(Frame::Lua { .. }))
    }

    pub(crate) fn pairing_enabled(&self, ctx: Context<'gc>) -> bool {
        #[cfg(test)]
        if self.decline_pairs {
            return false;
        }
        ctx.jit().call_pairs_enabled()
    }

    #[cfg(test)]
    pub(crate) fn stack_capacity(&self) -> usize {
        self.stack.capacity()
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
        if self.call_in_place(function, arguments) {
            return Ok(());
        }
        self.call_generic(ctx, function, arguments)
    }

    fn call_in_place(&mut self, function: crate::types::RegisterIndex, arguments: u8) -> bool {
        use super::thread::LuaReturn;
        use crate::{Function, Value};

        if self.state.frames.len() >= self.state.max_call_depth
            || self.state.frames.len() == self.state.frames.capacity()
        {
            return false;
        }
        let Some(Frame::Lua {
            base,
            is_variable: false,
            pc,
            expected_return,
            ..
        }) = self.state.frames.last_mut()
        else {
            return false;
        };
        let bottom = *base + usize::from(function.0);
        let Some(Value::Function(Function::Closure(closure))) = self.stack.get(bottom).copied()
        else {
            return false;
        };
        let prototype = closure.prototype();
        let count = usize::from(arguments);
        let width = usize::from(prototype.stack_size);
        let top = bottom + width;
        if arguments > prototype.fixed_params
            || count > width
            || bottom + 1 + count > self.stack.len()
            || top > self.stack.capacity()
        {
            return false;
        }
        *pc += 1;
        self.fuel.consume(LuaFrame::FUEL_PER_CALL);
        *expected_return = Some(LuaReturn::Normal(crate::types::VarCount::constant(0)));
        self.fuel
            .consume(crate::fuel::count_fuel(LuaFrame::FUEL_PER_ITEM, count));
        self.stack
            .copy_within(bottom + 1..bottom + 1 + count, bottom);
        self.stack.resize(top, Value::Nil);
        self.stack[bottom + count..top].fill(Value::Nil);
        self.state.frames.push(Frame::Lua {
            bottom,
            closure,
            base: bottom,
            is_variable: false,
            pc: 0,
            stack_size: width,
            expected_return: None,
        });
        self.fuel.consume(4);
        true
    }

    fn call_generic(
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

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[cfg(test)]
    pub(crate) fn test_replace_closure(&mut self, replacement: crate::Closure<'gc>) {
        let Some(Frame::Lua { closure, .. }) = self.state.frames.last_mut() else {
            panic!();
        };
        *closure = replacement;
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    pub(crate) fn native_transition(
        &mut self,
        ctx: Context<'gc>,
        transition: crate::opcode::CallTransition,
        prefix: u32,
    ) -> Result<(), VMError> {
        let result = self.with_frame(|mut frame| {
            *frame.registers().pc += 1;
            match transition {
                crate::opcode::CallTransition::Call {
                    func,
                    args,
                    returns,
                } => {
                    if ctx.jit().call_pairs_enabled() {
                        ctx.jit().observe_call(
                            ctx,
                            frame.closure(),
                            &frame.registers(),
                            func,
                            args,
                            returns,
                        );
                    }
                    frame.call_function(ctx, func, args, returns)
                }
                crate::opcode::CallTransition::TailCall { func, args } => {
                    frame.tail_call_function(ctx, func, args)
                }
                crate::opcode::CallTransition::Return { start, count } => {
                    frame.return_upper(&ctx, start, count)
                }
            }
        });
        let mut stats = ctx.jit().interpreter_stats();
        stats.dispatches = 1;
        stats.reported_instructions = result.as_ref().ok().map(|_| 0);
        drop(stats);
        if result.is_ok() {
            self.charge_instructions(prefix);
        }
        self.charge_native_slice(0);
        result
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[cfg(test)]
    pub(crate) fn resume_native(
        &mut self,
        ctx: Context<'gc>,
        budget: u32,
        resume: super::vm::NativeResume<'gc>,
    ) -> Result<u32, VMError> {
        self.resume_native_paired(ctx, budget, resume, None)
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[cfg(test)]
    pub(crate) fn resume_native_paired(
        &mut self,
        ctx: Context<'gc>,
        budget: u32,
        resume: super::vm::NativeResume<'gc>,
        scope: Option<&mut crate::jit::PairScope<'gc>>,
    ) -> Result<u32, VMError> {
        assert!(self.pair.handoff.is_none());
        self.select_pairs = false;
        let frame = LuaFrame {
            pair_handoff: scope,
            state: self.state,
            stack: &mut self.stack,
            fuel: self.fuel,
        };
        super::vm::resume_vm(ctx, frame, budget, resume)
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    pub(crate) fn canonical_slice(
        &mut self,
        ctx: Context<'gc>,
        budget: u32,
    ) -> Result<u32, VMError> {
        self.canonical_slice_paired(ctx, budget, None)
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    pub(crate) fn canonical_slice_paired(
        &mut self,
        ctx: Context<'gc>,
        budget: u32,
        scope: Option<&mut crate::jit::PairScope<'gc>>,
    ) -> Result<u32, VMError> {
        assert!(self.pair.handoff.is_none());
        if let Some(scope) = &scope {
            assert!(scope.handoff.is_none());
            #[cfg(test)]
            assert!(scope.resume.is_none());
        }
        self.select_pairs = false;
        let frame = LuaFrame {
            pair_handoff: scope,
            state: self.state,
            stack: &mut self.stack,
            fuel: self.fuel,
        };
        run_vm(ctx, frame, budget)
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    pub(crate) fn frame_identity(&self) -> (usize, usize) {
        (
            std::ptr::from_ref(&*self.state) as usize,
            self.state.frames.len(),
        )
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
            if step_fuel == 4 {
                assert!(self.pair.handoff.is_none());
                self.select_pairs = false;
                if let Some(outcome) = ctx.jit().run_region(ctx, self, limit - activations, budget)
                {
                    activations += outcome.slices;
                    #[cfg(test)]
                    {
                        stack_growths += usize::from(self.stack.capacity() > capacity);
                    }
                    if outcome.result.is_err()
                        || activations == limit
                        || !self.fuel.should_continue()
                        || self.state.mode() != ThreadMode::Normal
                        || !matches!(self.state.frames.last(), Some(Frame::Lua { .. }))
                    {
                        return Outcome {
                            result: outcome.result,
                            #[cfg(test)]
                            activations,
                            #[cfg(test)]
                            stack_growths,
                        };
                    }
                    continue;
                }
            }
            #[cfg(all(
                not(miri),
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            {
                assert!(self.pair.handoff.is_none());
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
            let (result, step_charged) = if let Some(pair) = self.pair.handoff.take() {
                let prefix = result.expect("pair handoff with VM error");
                assert!(prefix < budget);
                self.select_pairs = false;
                let outcome = pair.invoke(ctx, self, budget, prefix);
                self.pair.cache = Some(pair);
                if let Some(outcome) = outcome {
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

    #[cfg(test)]
    pub(crate) fn run_canonical(
        &mut self,
        ctx: Context<'gc>,
        limit: usize,
        budget: u32,
        step_fuel: i32,
    ) -> Outcome {
        self.run(ctx, limit, budget, step_fuel)
    }
}

#[cfg(all(
    test,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
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
    test,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
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

#[cfg(all(
    test,
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod return_tests;
