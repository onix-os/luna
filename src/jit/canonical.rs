use std::{
    any::Any,
    ffi::c_void,
    mem::MaybeUninit,
    panic::{catch_unwind, AssertUnwindSafe},
};

use crate::{
    opcode::Operation, thread::activation::ActivationHost, types::RegisterIndex, Context, Function,
    Value,
};

use super::{
    abi::{self, Exit, Slot},
    backend::calls::{CallCode, Hooks},
    calls::NativeFrame,
    exits::Kind,
    leaf::{self, Operand, Pattern},
    projection::Origin,
};

pub(super) struct Site {
    caller: u64,
    callee: u64,
    pc: usize,
    function: RegisterIndex,
    arguments: u8,
    pattern: Pattern,
    registers: usize,
    start: RegisterIndex,
    returns: u8,
}

pub(super) struct Program {
    pub site: Site,
    pub code: CallCode,
    origin: super::resources::MappingCounter,
}

impl Program {
    pub(super) fn key(&self) -> super::pairs::Key {
        super::pairs::Key {
            caller: self.site.caller,
            callee: self.site.callee,
            pc: self.site.pc,
        }
    }

    pub fn new(
        key: super::pairs::Key,
        caller: &super::ir::Snapshot,
        callee: &super::ir::Snapshot,
        memory: super::resources::MappingCounter,
        limit: usize,
        metadata: super::resources::BudgetAllocator,
        limits: super::work::Limits,
        #[cfg(test)] failure: super::backend::Failure,
    ) -> Result<Self, super::JitError> {
        if !super::resources::LedgerRef::ptr_eq(
            &caller.operations.allocator().0,
            &callee.operations.allocator().0,
        ) || !memory.same_root(&super::resources::MappingCounter::new(
            caller.operations.allocator().0.clone(),
        )) || !memory.same_root(&super::resources::MappingCounter::new(metadata.0.clone()))
        {
            return Err(super::JitError::Compilation(
                "pair ledger provenance".into(),
            ));
        }
        let plan = super::calls::Plan::new(caller, callee, key.pc, limits)?;
        let Operation::Return { start, count } = callee.operations[3] else {
            unreachable!()
        };
        let site = Site {
            caller: key.caller,
            callee: key.callee,
            pc: key.pc,
            function: plan.function,
            arguments: plan.arguments,
            pattern: plan.pattern,
            registers: callee.registers,
            start,
            returns: count.to_constant().unwrap(),
        };
        let origin = memory.clone();
        let code = super::backend::calls::compile(
            &plan,
            Hooks { enter, leave },
            memory,
            limit,
            metadata,
            limits,
            #[cfg(test)]
            failure,
            #[cfg(test)]
            super::backend::calls::LinkFault::None,
        )?;
        Ok(Self { site, code, origin })
    }

    #[cfg(test)]
    pub fn invoke<'gc>(
        &self,
        ctx: Context<'gc>,
        host: &mut ActivationHost<'gc, '_>,
        budget: u32,
    ) -> Option<(usize, usize)> {
        self.invoke_result(ctx, host, budget, 0).map(|outcome| {
            outcome.result.unwrap();
            (outcome.calls, outcome.returns)
        })
    }

    pub fn invoke_result<'gc>(
        &self,
        ctx: Context<'gc>,
        host: &mut ActivationHost<'gc, '_>,
        budget: u32,
        prefix: u32,
    ) -> Option<super::PairOutcome> {
        let manager = ctx.jit().0.borrow();
        if manager.config.mode != super::JitMode::Auto || !self.origin.same_root(&manager.memory) {
            return None;
        }
        drop(manager);
        invoke_result(ctx, host, &self.site, &self.code, budget, prefix)
    }
}

struct Session<'gc, 'host, 'borrow> {
    ctx: Context<'gc>,
    host: &'borrow mut ActivationHost<'gc, 'host>,
    site: &'borrow Site,
    frame: NativeFrame,
    slots: &'borrow mut [MaybeUninit<Slot>; 256],
    cell: Slot,
    view: leaf::View,
    target: Option<(bool, usize)>,
    calls: usize,
    returns: usize,
    error: Option<crate::thread::VMError>,
    panic: Option<Box<dyn Any + Send>>,
    prefix: u32,
}

#[cold]
#[inline(never)]
fn assert_leave_transition(actual: (u64, u32, usize, usize), start: u32) {
    assert_eq!(actual, (3, start, 1, 0));
}

impl<'gc, 'host, 'borrow> Session<'gc, 'host, 'borrow> {
    fn new(
        ctx: Context<'gc>,
        host: &'borrow mut ActivationHost<'gc, 'host>,
        site: &'borrow Site,
        slots: &'borrow mut [MaybeUninit<Slot>; 256],
    ) -> Self {
        let nil = Slot::from_value(Value::Nil);
        Self {
            ctx,
            host,
            site,
            frame: NativeFrame {
                slots: std::ptr::null_mut(),
                view: std::ptr::null_mut(),
                exit: Exit::default(),
            },
            slots,
            cell: nil,
            view: leaf::View {
                version: leaf::VERSION,
                cell: std::ptr::null_mut(),
                reads: 0,
                writes: 0,
                dirty: 0,
            },
            target: None,
            calls: 0,
            returns: 0,
            error: None,
            panic: None,
            prefix: 0,
        }
    }

    fn preflight(&mut self, budget: u32) -> bool {
        if !self.host.lua_ready() {
            return false;
        }
        self.host.clear_hook(self.ctx);
        if budget < 4
            || self.ctx.hook_enabled()
            || !self.host.lua_ready()
            || self.site.registers > 256
        {
            return false;
        }
        let mut fuel = self.host.fuel().clone();
        fuel.consume(self.prefix.try_into().unwrap());
        fuel.consume(8 + i32::from(self.site.arguments));
        if !fuel.should_continue() {
            return false;
        }
        let ctx = self.ctx;
        let site = self.site;
        self.host.with_registers(|caller, registers| {
            let registry = ctx.jit_registry().borrow();
            if *registers.pc != site.pc
                || registry.identity(ctx, caller.prototype()) != Some(site.caller)
            {
                return false;
            }
            let prototype = caller.prototype();
            let Some(opcode) = prototype.opcodes.get(site.pc) else {
                return false;
            };
            let Operation::Call {
                func,
                args,
                returns,
            } = opcode.decode()
            else {
                return false;
            };
            if func != site.function
                || args.to_constant() != Some(site.arguments)
                || returns.to_constant() != Some(0)
            {
                return false;
            }
            let index = usize::from(site.function.0);
            let Some(Value::Function(Function::Closure(callee))) =
                registers.stack_frame.get(index).copied()
            else {
                return false;
            };
            if registry.identity(ctx, callee.prototype()) != Some(site.callee)
                || usize::from(callee.prototype().stack_size) != site.registers
            {
                return false;
            }
            let Some(upvalue) = callee.upvalues().get(usize::from(site.pattern.upvalue)) else {
                return false;
            };
            let Some(origin) = registers.projection_origin(upvalue.get()) else {
                return false;
            };
            let capture = match origin {
                Origin::Upper(_, value) | Origin::Register(_, value) => value,
                Origin::Closed(_) => return false,
            };
            if !matches!(capture, Value::Integer(_))
                || index + 1 + usize::from(site.arguments) > registers.stack_frame.len()
            {
                return false;
            }
            match site.pattern.right {
                Operand::Register(register) if register == site.pattern.read => true,
                Operand::Register(register) => {
                    usize::from(register.0) < usize::from(site.arguments)
                        && register.0 < callee.prototype().fixed_params
                        && matches!(
                            registers
                                .stack_frame
                                .get(index + 1 + usize::from(register.0)),
                            Some(Value::Integer(_))
                        )
                }
                Operand::Constant(constant) => matches!(
                    callee.prototype().constants.get(usize::from(constant)),
                    Some(crate::Constant::Integer(_))
                ),
            }
        })
    }

    fn enter(&mut self, pc: u64, function: u32, arguments: u32) -> *mut NativeFrame {
        if self.calls != 0
            || (pc, function, arguments)
                != (
                    self.site.pc as u64,
                    u32::from(self.site.function.0),
                    u32::from(self.site.arguments),
                )
        {
            return std::ptr::null_mut();
        }
        let ctx = self.ctx;
        self.calls = 1;
        let result = self.host.call(ctx, self.site.function, self.site.arguments);
        let mut stats = ctx.jit().interpreter_stats();
        stats.dispatches = 1;
        stats.reported_instructions = result.as_ref().ok().map(|_| 0);
        drop(stats);
        if let Err(error) = result {
            self.error = Some(error);
            return std::ptr::null_mut();
        }
        self.host.charge_instructions(self.prefix);
        if !self.host.lua_ready() || !self.host.fuel().should_continue() {
            return std::ptr::null_mut();
        }
        let site = self.site;
        let prepared = self.host.with_registers(|closure, registers| {
            if *registers.pc != 0
                || ctx
                    .jit_registry()
                    .borrow()
                    .identity(ctx, closure.prototype())
                    != Some(site.callee)
                || registers.stack_frame.len() < site.registers
            {
                return None;
            }
            let upvalue = closure
                .upvalues()
                .get(usize::from(site.pattern.upvalue))?
                .get();
            let origin = registers.projection_origin(upvalue)?;
            let (target, value) = match origin {
                Origin::Upper(index, value) => ((true, index), value),
                Origin::Register(index, value) if index < site.registers => ((false, index), value),
                _ => return None,
            };
            if !matches!(value, Value::Integer(_)) {
                return None;
            }
            for (slot, value) in self.slots[..site.registers]
                .iter_mut()
                .zip(registers.stack_frame.iter().copied())
            {
                slot.write(Slot::from_value(value));
            }
            Some((target, Slot::from_value(value)))
        });
        let Some((target, cell)) = prepared else {
            return std::ptr::null_mut();
        };
        self.target = Some(target);
        self.cell = cell;
        self.view.cell = if target.0 {
            std::ptr::addr_of_mut!(self.cell)
        } else {
            self.slots[target.1].as_mut_ptr()
        };
        self.frame.slots = self.slots.as_mut_ptr().cast();
        self.frame.view = std::ptr::addr_of_mut!(self.view);
        std::ptr::addr_of_mut!(self.frame)
    }

    fn leave(&mut self, frame: *mut NativeFrame, pc: u64, start: u32) -> u32 {
        assert_eq!(frame, std::ptr::addr_of_mut!(self.frame));
        if pc != 3 || start != u32::from(self.site.start.0) || self.calls != 1 || self.returns != 0
        {
            assert_leave_transition(
                (pc, start, self.calls, self.returns),
                self.site.start.0.into(),
            );
        }
        let (upper, index) = self.target.unwrap();
        let cell = if upper {
            std::ptr::addr_of_mut!(self.cell)
        } else {
            self.slots[index].as_mut_ptr()
        };
        assert_eq!(self.view.cell, cell);
        assert_eq!(self.view.version, leaf::VERSION);
        assert!(self.view.reads <= 1 && self.view.writes <= 1);
        assert_eq!(self.view.dirty, u64::from(self.view.writes));
        assert_eq!(self.frame.slots, self.slots.as_mut_ptr().cast());
        assert!(self.frame.exit.pc <= 3 && self.frame.exit.instructions <= 3);
        let ctx = self.ctx;
        let site = self.site;
        let cell = unsafe { cell.read() };
        assert_eq!(cell.tag, abi::INTEGER);
        self.host.with_registers(|closure, mut registers| {
            assert_eq!(
                ctx.jit_registry()
                    .borrow()
                    .identity(ctx, closure.prototype()),
                Some(site.callee)
            );
            assert_eq!(*registers.pc, 0);
            assert!(registers.projection_read(upper, index).is_some());
            // Enter initializes the entire prefix before publishing the native frame.
            let slots = unsafe {
                std::slice::from_raw_parts(self.slots.as_ptr().cast::<Slot>(), site.registers)
            };
            assert!(slots.iter().all(|slot| slot.tag <= abi::REFERENCE));
            for (slot, dest) in slots.iter().copied().zip(registers.stack_frame.iter_mut()) {
                slot.write_back(dest);
            }
            if upper && self.view.writes != 0 {
                registers.projection_write(true, index, cell.value(Value::Nil));
            }
            *registers.pc = self.frame.exit.pc as usize;
        });
        {
            let mut manager = ctx.jit().0.borrow_mut();
            manager.stats.native_upvalue_reads = manager
                .stats
                .native_upvalue_reads
                .saturating_add(u64::from(self.view.reads));
            manager.stats.native_upvalue_writes = manager
                .stats
                .native_upvalue_writes
                .saturating_add(u64::from(self.view.writes));
            manager.stats.record_native_exit(&self.frame.exit);
        }
        if (
            self.frame.exit.pc,
            self.frame.exit.instructions,
            self.frame.exit.reason,
        ) != (3, 3, Kind::Interpreter as u32)
        {
            self.host.charge_native_slice(self.frame.exit.instructions);
            return 1;
        }
        self.returns = 1;
        let result = self.host.return_fixed(
            ctx,
            self.site.start,
            self.site.returns,
            self.frame.exit.instructions,
        );
        let mut stats = ctx.jit().interpreter_stats();
        stats.dispatches = 1;
        stats.reported_instructions = result.as_ref().ok().map(|_| 0);
        drop(stats);
        if let Err(error) = result {
            self.error = Some(error);
        }
        2
    }

    fn finish(mut self) -> super::PairOutcome {
        let payload = self.panic.take();
        let outcome = super::PairOutcome {
            calls: self.calls,
            returns: self.returns,
            result: self.error.take().map_or(Ok(()), Err),
        };
        drop(self);
        if let Some(payload) = payload {
            std::panic::resume_unwind(payload);
        }
        outcome
    }
}

unsafe extern "C" fn enter(
    data: *mut c_void,
    pc: u64,
    function: u32,
    arguments: u32,
) -> *mut NativeFrame {
    let session = unsafe { &mut *data.cast::<Session<'_, '_, '_>>() };
    match catch_unwind(AssertUnwindSafe(|| session.enter(pc, function, arguments))) {
        Ok(frame) => frame,
        Err(payload) => {
            session.panic = Some(payload);
            std::ptr::null_mut()
        }
    }
}

unsafe extern "C" fn leave(data: *mut c_void, frame: *mut NativeFrame, pc: u64, start: u32) -> u32 {
    let session = unsafe { &mut *data.cast::<Session<'_, '_, '_>>() };
    match catch_unwind(AssertUnwindSafe(|| session.leave(frame, pc, start))) {
        Ok(result) => result,
        Err(payload) => {
            session.panic = Some(payload);
            0
        }
    }
}

#[cfg(test)]
fn invoke<'gc>(
    ctx: Context<'gc>,
    host: &mut ActivationHost<'gc, '_>,
    site: &Site,
    code: &CallCode,
    budget: u32,
) -> Option<(usize, usize)> {
    invoke_result(ctx, host, site, code, budget, 0).map(|outcome| {
        outcome.result.unwrap();
        (outcome.calls, outcome.returns)
    })
}

fn invoke_result<'gc>(
    ctx: Context<'gc>,
    host: &mut ActivationHost<'gc, '_>,
    site: &Site,
    code: &CallCode,
    budget: u32,
    prefix: u32,
) -> Option<super::PairOutcome> {
    let mut scratch = [MaybeUninit::uninit(); 256];
    let mut session = Session::new(ctx, host, site, &mut scratch);
    session.prefix = prefix;
    if !session.preflight(budget) {
        return None;
    }
    unsafe {
        code.invoke(std::ptr::addr_of_mut!(session).cast(), budget.min(64));
    }
    let outcome = session.finish();
    (outcome.calls != 0).then_some(outcome)
}

#[cfg(test)]
mod tests;
