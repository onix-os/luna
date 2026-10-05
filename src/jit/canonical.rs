use std::{
    ffi::c_void,
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
}

pub(super) struct Program {
    pub site: Site,
    pub code: CallCode,
    origin: super::resources::MappingCounter,
}

impl Program {
    pub fn new(
        key: super::pairs::Key,
        caller: &super::ir::Snapshot,
        callee: &super::ir::Snapshot,
        memory: super::resources::MappingCounter,
        limit: usize,
        metadata: super::resources::BudgetAllocator,
        limits: super::work::Limits,
        failure: super::backend::Failure,
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
        let Operation::Return { start, .. } = callee.operations[3] else {
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
        };
        let origin = memory.clone();
        let code = super::backend::calls::compile(
            &plan,
            Hooks { enter, leave },
            memory,
            limit,
            metadata,
            limits,
            failure,
            super::backend::calls::LinkFault::None,
        )?;
        Ok(Self { site, code, origin })
    }

    pub fn invoke<'gc>(
        &self,
        ctx: Context<'gc>,
        host: &mut ActivationHost<'gc, '_>,
        budget: u32,
    ) -> Option<(usize, usize)> {
        let manager = ctx.jit().0.borrow();
        if manager.config.mode != super::JitMode::Auto || !self.origin.same_root(&manager.memory) {
            return None;
        }
        drop(manager);
        invoke(ctx, host, &self.site, &self.code, budget)
    }
}

struct Session<'gc, 'host, 'borrow> {
    ctx: Context<'gc>,
    host: &'borrow mut ActivationHost<'gc, 'host>,
    site: &'borrow Site,
    frame: NativeFrame,
    slots: [Slot; 256],
    cell: Slot,
    view: leaf::View,
    target: Option<(bool, usize)>,
    calls: usize,
    returns: usize,
    error: Option<crate::thread::VMError>,
    panicked: bool,
}

impl<'gc, 'host, 'borrow> Session<'gc, 'host, 'borrow> {
    fn new(
        ctx: Context<'gc>,
        host: &'borrow mut ActivationHost<'gc, 'host>,
        site: &'borrow Site,
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
            slots: [nil; 256],
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
            panicked: false,
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
                *slot = Slot::from_value(value);
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
            std::ptr::addr_of_mut!(self.slots[target.1])
        };
        self.frame.slots = self.slots.as_mut_ptr();
        self.frame.view = std::ptr::addr_of_mut!(self.view);
        std::ptr::addr_of_mut!(self.frame)
    }

    fn leave(&mut self, frame: *mut NativeFrame, pc: u64, start: u32) -> u32 {
        assert_eq!(frame, std::ptr::addr_of_mut!(self.frame));
        assert_eq!(
            (pc, start, self.calls, self.returns),
            (3, u32::from(self.site.start.0), 1, 0)
        );
        let (upper, index) = self.target.unwrap();
        let cell = if upper {
            std::ptr::addr_of_mut!(self.cell)
        } else {
            std::ptr::addr_of_mut!(self.slots[index])
        };
        assert_eq!(self.view.cell, cell);
        assert_eq!(self.view.version, leaf::VERSION);
        assert!(self.view.reads <= 1 && self.view.writes <= 1);
        assert_eq!(self.view.dirty, u64::from(self.view.writes));
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
            assert!(self.slots[..site.registers]
                .iter()
                .all(|slot| slot.tag <= abi::REFERENCE));
            for (slot, dest) in self.slots[..site.registers]
                .iter()
                .copied()
                .zip(registers.stack_frame.iter_mut())
            {
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
        let result = self
            .host
            .return_zero(ctx, self.site.start, self.frame.exit.instructions);
        let mut stats = ctx.jit().interpreter_stats();
        stats.dispatches = 1;
        stats.reported_instructions = result.as_ref().ok().map(|_| 0);
        drop(stats);
        if let Err(error) = result {
            self.error = Some(error);
        }
        2
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
        Err(_) => {
            session.panicked = true;
            std::ptr::null_mut()
        }
    }
}

unsafe extern "C" fn leave(data: *mut c_void, frame: *mut NativeFrame, pc: u64, start: u32) -> u32 {
    let session = unsafe { &mut *data.cast::<Session<'_, '_, '_>>() };
    match catch_unwind(AssertUnwindSafe(|| session.leave(frame, pc, start))) {
        Ok(result) => result,
        Err(_) => {
            session.panicked = true;
            0
        }
    }
}

fn invoke<'gc>(
    ctx: Context<'gc>,
    host: &mut ActivationHost<'gc, '_>,
    site: &Site,
    code: &CallCode,
    budget: u32,
) -> Option<(usize, usize)> {
    let mut session = Session::new(ctx, host, site);
    if !session.preflight(budget) {
        return None;
    }
    unsafe {
        code.invoke(std::ptr::addr_of_mut!(session).cast(), budget.min(64));
    }
    assert!(!session.panicked, "canonical hook panicked");
    assert!(
        session.error.is_none(),
        "canonical hook failed: {:?}",
        session.error
    );
    Some((session.calls, session.returns))
}

#[cfg(test)]
mod tests;
