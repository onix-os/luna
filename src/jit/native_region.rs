use std::{
    any::Any,
    mem::MaybeUninit,
    panic::{catch_unwind, AssertUnwindSafe},
};

use crate::{
    thread::{activation::ActivationHost, NativeResume, VMError},
    Closure, Context,
};

use super::{
    abi,
    backend::region::{self, View},
    helpers, scoped_helpers, JitError, Prepared, PreparedPair,
};

pub(super) struct Region<'gc> {
    caller: scoped_helpers::Code<'gc>,
    ordinary: Prepared,
    pair: PreparedPair,
    driver: region::Driver,
    source: u64,
}

#[derive(Debug)]
pub(super) struct Outcome {
    pub slices: usize,
    pub fragments: usize,
    pub pairs: usize,
    pub result: Result<(), VMError>,
}

impl<'gc> Region<'gc> {
    pub(super) fn new(
        ctx: Context<'gc>,
        caller: Closure<'gc>,
        pair: PreparedPair,
    ) -> Result<Self, JitError> {
        let source = ctx
            .jit_registry()
            .borrow()
            .identity(ctx, caller.prototype())
            .ok_or_else(|| JitError::Compilation("unregistered region caller".into()))?;
        if pair.program.key().caller != source {
            return Err(JitError::Compilation("region pair caller".into()));
        }
        let ordinary = ctx
            .jit()
            .lookup(source)
            .ok_or_else(|| JitError::Compilation("missing region fallback".into()))?;
        let (memory, metadata, workspace, limit, limits) = {
            let manager = ctx.jit().0.borrow();
            (
                manager.memory.clone(),
                manager.metadata.clone(),
                manager.snapshots.clone(),
                manager.config.max_code_bytes,
                super::work::Limits::from(&manager.config),
            )
        };
        let snapshot = super::ir::Snapshot::new_in(
            &caller.prototype(),
            ctx.jit().0.borrow().config.max_prototype_instructions,
            workspace.clone(),
        )?;
        let expansion = super::work::Expansion::admit(&snapshot, limits)?;
        if expansion
            .instructions
            .checked_add(64)
            .is_none_or(|count| count > limits.instructions)
            || expansion
                .blocks
                .checked_add(4)
                .is_none_or(|count| count > limits.blocks)
        {
            return Err(JitError::ResourceLimit("connected region IR"));
        }
        drop(snapshot);
        let caller = scoped_helpers::Code::new(ctx, caller)?;
        let remaining = super::work::Limits {
            instructions: 64,
            blocks: 4,
            relocations: limits.relocations.saturating_sub(caller.relocations()),
        };
        let driver = region::compile(
            caller.entry(),
            boundary,
            memory,
            limit,
            metadata,
            workspace,
            remaining,
            region::Fault::None,
        )?;
        Ok(Self {
            caller,
            ordinary,
            pair,
            driver,
            source,
        })
    }

    pub(super) fn run(
        &self,
        ctx: Context<'gc>,
        host: &mut ActivationHost<'gc, '_>,
        limit: usize,
        budget: u32,
    ) -> Option<Outcome> {
        if limit == 0
            || limit > u32::MAX as usize
            || !(1..=64).contains(&budget)
            || !host.lua_ready()
        {
            return None;
        }
        let pair = ctx.jit().pair_lease(self.pair.program.key())?;
        if !super::owner::Shared::ptr_eq(&pair, &self.pair.program) {
            return None;
        }
        drop(pair);
        {
            let manager = ctx.jit().0.borrow();
            if !manager
                .code
                .get(&self.source)
                .is_some_and(|code| super::owner::Shared::ptr_eq(&code.code, &self.ordinary.code))
            {
                return None;
            }
        }
        let width = self.caller.registers();
        let mut storage = [MaybeUninit::<abi::Slot>::uninit(); 256];
        if width > storage.len() {
            return None;
        }
        host.with_registers(|_, registers| {
            if registers.stack_frame.len() < width {
                return None;
            }
            for (dest, value) in storage[..width]
                .iter_mut()
                .zip(registers.stack_frame.iter().copied())
            {
                dest.write(abi::Slot::from_value(value));
            }
            Some(())
        })?;
        let slots = unsafe {
            std::slice::from_raw_parts_mut(storage.as_mut_ptr().cast::<abi::Slot>(), width)
        };
        let identity = host.frame_identity();
        let mut frame = scoped_helpers::Frame::new(ctx, host);
        let pc = self.caller.prepare(&mut frame, slots)?;
        let admitted = super::canonical::admission::Admitted::new(
            &self.pair.program,
            ctx,
            self.caller.source(),
            identity,
        )?;
        let mut session = Session {
            region: self,
            admitted,
            frame,
            slots,
            host: abi::Host {
                data: std::ptr::null_mut(),
                projection: std::ptr::null_mut(),
            },
            identity,
            limit,
            budget,
            outcome: Outcome {
                slices: 0,
                fragments: 0,
                pairs: 0,
                result: Ok(()),
            },
            panic: None,
        };
        let pointer = std::ptr::from_mut(&mut session);
        let mut view = View {
            data: pointer.cast(),
            slots: std::ptr::null_mut(),
            pc: pc as u64,
            budget,
            host: std::ptr::null_mut(),
            exit: abi::Exit::default(),
        };
        unsafe { (&mut *pointer).publish(&mut view) };
        unsafe { self.driver.invoke(&mut view, limit as u32) };
        let payload = session.panic.take();
        let outcome = session.outcome;
        if let Some(payload) = payload {
            std::panic::resume_unwind(payload);
        }
        Some(outcome)
    }
}

struct Session<'gc, 'host, 'borrow, 'region> {
    region: &'region Region<'gc>,
    admitted: super::canonical::admission::Admitted<'region, 'gc>,
    frame: scoped_helpers::Frame<'gc, 'host, 'borrow>,
    slots: &'borrow mut [abi::Slot],
    host: abi::Host,
    identity: (usize, usize),
    limit: usize,
    budget: u32,
    outcome: Outcome,
    panic: Option<Box<dyn Any + Send>>,
}

impl Session<'_, '_, '_, '_> {
    fn publish(&mut self, view: &mut View) {
        scoped_helpers::publish_region(&mut self.frame, self.slots, &mut self.host, view);
    }

    fn complete(&mut self, view: &mut View) -> u32 {
        let ctx = self.frame.ctx;
        self.outcome.fragments += 1;
        let transition = self.frame.host.with_registers(|closure, registers| {
            assert_eq!(closure, self.region.caller.source());
            for (slot, value) in self.slots.iter().zip(registers.stack_frame.iter_mut()) {
                slot.write_back(value);
            }
            if self.frame.panic.is_none() {
                *registers.pc = view.exit.pc as usize;
            }
            closure
                .prototype()
                .opcodes
                .get(*registers.pc)
                .and_then(|op| op.call_transition())
        });
        record(ctx, &view.exit, std::mem::take(&mut self.frame.count));
        if let Some(payload) = self.frame.panic.take() {
            self.panic = Some(payload);
            return 0;
        }
        assert!(view.exit.instructions <= self.budget);
        if view.exit.instructions == self.budget {
            self.frame.host.charge_native_slice(view.exit.instructions);
            self.outcome.slices += 1;
        } else if let Some(transition) = transition {
            let paired = if self.limit - self.outcome.slices >= 2
                && view.exit.pc as usize == self.region.pair.program.key().pc
            {
                self.admitted
                    .invoke(self.frame.host, self.budget, view.exit.instructions)
            } else {
                None
            };
            if let Some(paired) = paired {
                self.outcome.slices += 1 + paired.returns;
                self.outcome.pairs += paired.returns;
                self.outcome.result = paired.result;
            } else {
                self.outcome.result =
                    self.frame
                        .host
                        .native_transition(ctx, transition, view.exit.instructions);
                self.outcome.slices += 1;
                return 0;
            }
        } else {
            let resume = NativeResume::new(
                ctx,
                self.region.caller.source(),
                self.region.source,
                self.identity,
                view.exit.pc as usize,
                view.exit.instructions,
                Prepared {
                    code: self.region.ordinary.code.clone(),
                },
            );
            let result = self.frame.host.resume_native(ctx, self.budget, resume);
            if let Ok(instructions) = result {
                self.frame.host.charge_instructions(instructions);
            }
            self.frame.host.charge_native_slice(0);
            self.outcome.slices += 1;
            self.outcome.result = result.map(|_| ());
            return 0;
        }
        if self.outcome.result.is_err()
            || self.outcome.slices >= self.limit
            || !self.frame.host.fuel().should_continue()
            || !self.frame.host.lua_ready()
            || self.frame.host.frame_identity() != self.identity
        {
            return 0;
        }
        let pc = self.frame.host.with_registers(|closure, registers| {
            if registers.stack_frame.len() < self.slots.len()
                || !self.region.caller.accepts_entry(closure, *registers.pc)
            {
                return None;
            }
            for (slot, value) in self
                .slots
                .iter_mut()
                .zip(registers.stack_frame.iter().copied())
            {
                *slot = abi::Slot::from_value(value);
            }
            Some(*registers.pc)
        });
        let Some(pc) = pc else {
            return 0;
        };
        view.pc = pc as u64;
        self.publish(view);
        1
    }
}

fn record(ctx: Context<'_>, exit: &abi::Exit, counts: helpers::Counts) {
    let mut manager = ctx.jit().0.borrow_mut();
    let stats = &mut manager.stats;
    stats.helper_calls = stats.helper_calls.saturating_add(counts.calls);
    stats.helper_instructions = stats.helper_instructions.saturating_add(counts.completed);
    stats.helper_declines = stats.helper_declines.saturating_add(counts.declined);
    stats.native_table_reads = stats.native_table_reads.saturating_add(counts.table_reads);
    stats.native_table_writes = stats
        .native_table_writes
        .saturating_add(counts.table_writes);
    stats.native_upvalue_reads = stats
        .native_upvalue_reads
        .saturating_add(counts.upvalue_reads);
    stats.native_upvalue_writes = stats
        .native_upvalue_writes
        .saturating_add(counts.upvalue_writes);
    stats.native_allocations = stats.native_allocations.saturating_add(counts.allocations);
    stats.record_native_exit(exit);
}

unsafe extern "C" fn boundary(view: *mut View) -> u32 {
    let view = unsafe { &mut *view };
    let session = unsafe { &mut *view.data.cast::<Session<'_, '_, '_, '_>>() };
    match catch_unwind(AssertUnwindSafe(|| session.complete(view))) {
        Ok(result) => result,
        Err(payload) => {
            session.panic = Some(payload);
            0
        }
    }
}

#[cfg(test)]
mod tests;
