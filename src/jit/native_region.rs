use std::{
    any::Any,
    mem::MaybeUninit,
    panic::{catch_unwind, AssertUnwindSafe},
};

use crate::{
    thread::{activation::ActivationHost, VMError},
    Context,
};

#[cfg(test)]
use crate::{jit::JitError, Closure};

use super::{
    abi,
    backend::region::{self, View},
    helpers, scoped_helpers, Prepared, PreparedPair,
};

pub(super) mod cache;
mod request;
pub(super) mod resume;
pub(super) mod schedule;
pub(super) use request::Request;

pub(crate) struct Region {
    caller: scoped_helpers::Code,
    ordinary: Prepared,
    pair: PreparedPair,
    driver: region::Driver,
    source: u64,
    config: super::JitConfig,
}

#[derive(Debug)]
pub(crate) struct Outcome {
    pub slices: usize,
    pub fragments: usize,
    pub pairs: usize,
    pub result: Result<(), VMError>,
}

impl Region {
    fn installed(&self, manager: &super::Manager) -> bool {
        self.caller.belongs_to(&manager.memory)
            && manager
                .code
                .get(&self.source)
                .is_some_and(|code| super::owner::Shared::ptr_eq(&code.code, &self.ordinary.code))
            && manager
                .pairs
                .as_ref()
                .is_some_and(|pairs| pairs.contains(&self.pair))
    }

    #[cfg(test)]
    pub(super) fn new<'gc>(
        ctx: Context<'gc>,
        caller: Closure<'gc>,
        pair: PreparedPair,
    ) -> Result<Self, JitError> {
        Request::prepare(ctx, caller.prototype(), pair)?.compile(ctx.jit())
    }

    pub(super) fn run<'gc>(
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
        let closure = host.with_registers(|closure, _| closure);
        let caller = self.caller.bind(ctx, closure)?;
        let width = self.caller.registers();
        let mut storage = [MaybeUninit::<abi::Slot>::uninit(); 256];
        let mut root_storage = [MaybeUninit::<crate::Value>::uninit(); 256];
        if width > storage.len() {
            return None;
        }
        host.with_registers(|_, registers| {
            if registers.stack_frame.len() < width {
                return None;
            }
            for (index, (dest, value)) in storage[..width]
                .iter_mut()
                .zip(registers.stack_frame.iter().copied())
                .enumerate()
            {
                let slot = dest.write(abi::Slot::from_value(value));
                if self.caller.rooted_moves() {
                    root_storage[index].write(value);
                    if slot.tag == abi::REFERENCE {
                        slot.bits = index as u64;
                    }
                }
            }
            Some(())
        })?;
        let slots = unsafe {
            std::slice::from_raw_parts_mut(storage.as_mut_ptr().cast::<abi::Slot>(), width)
        };
        let roots = self.caller.rooted_moves().then(|| {
            // The complete root prefix is initialized with the slot prefix above.
            unsafe { std::slice::from_raw_parts_mut(root_storage.as_mut_ptr().cast(), width) }
        });
        let identity = host.frame_identity();
        let mut frame = scoped_helpers::Frame::new(ctx, host);
        let pc = caller.prepare(&mut frame, slots)?;
        let admitted = super::canonical::admission::Admitted::new(
            &self.pair.program,
            ctx,
            caller.source(),
            identity,
        )?;
        let mut session = Session {
            region: self,
            admitted,
            frame,
            slots,
            roots,
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
            rooted_pending: false,
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
        {
            let mut manager = ctx.jit().0.borrow_mut();
            manager.stats.native_region_entries =
                manager.stats.native_region_entries.saturating_add(1);
        }
        unsafe { (&mut *pointer).publish(&mut view) };
        unsafe { self.driver.invoke(&mut view, limit as u32) };
        assert!(!session.rooted_pending);
        let payload = session.panic.take();
        let outcome = session.outcome;
        if let Some(payload) = payload {
            std::panic::resume_unwind(payload);
        }
        Some(outcome)
    }
}

impl super::Runtime {
    pub(crate) fn run_region<'gc>(
        &self,
        ctx: Context<'gc>,
        host: &mut ActivationHost<'gc, '_>,
        limit: usize,
        budget: u32,
    ) -> Option<Outcome> {
        {
            let manager = self.0.borrow();
            if manager.config.mode != super::JitMode::Auto
                || manager
                    .pairs
                    .as_ref()
                    .is_none_or(|pairs| pairs.regions.is_empty())
            {
                return None;
            }
        }
        if !host.lua_ready() {
            return None;
        }
        host.clear_hook(ctx);
        let region =
            host.with_registers(|caller, registers| self.region_at(ctx, caller, *registers.pc))?;
        region.run(ctx, host, limit, budget)
    }
}

struct Session<'gc, 'host, 'borrow, 'region> {
    region: &'region Region,
    admitted: super::canonical::admission::Admitted<'region, 'gc>,
    frame: scoped_helpers::Frame<'gc, 'host, 'borrow>,
    slots: &'borrow mut [abi::Slot],
    roots: Option<&'borrow mut [crate::Value<'gc>]>,
    host: abi::Host,
    identity: (usize, usize),
    limit: usize,
    budget: u32,
    outcome: Outcome,
    panic: Option<Box<dyn Any + Send>>,
    rooted_pending: bool,
}

enum RootedCompletion {
    Unavailable,
    Declined,
    Complete(u32),
}

impl Session<'_, '_, '_, '_> {
    fn publish(&mut self, view: &mut View) {
        scoped_helpers::publish_region(&mut self.frame, self.slots, &mut self.host, view);
    }

    #[inline(never)]
    fn fallback(&mut self, exit: &abi::Exit) {
        let ctx = self.frame.ctx;
        let token = resume::Token::new(
            ctx,
            self.admitted.caller(),
            self.region.source,
            self.identity,
            exit.pc as usize,
            exit.instructions,
            Prepared {
                code: self.region.ordinary.code.clone(),
            },
        );
        let mut scope = super::PairScope::default();
        let select_pairs = self.limit - self.outcome.slices >= 2
            && self.budget >= 4
            && self.frame.host.pairing_enabled(ctx);
        let result = token.run_paired(
            ctx,
            self.frame.host,
            self.budget,
            select_pairs.then_some(&mut scope),
        );
        self.outcome.slices += 1;
        let (result, charged) = if let Some(pair) = scope.handoff.take() {
            let completed = result.expect("pair handoff with VM error");
            assert!(completed < self.budget);
            if let Some(paired) = pair.invoke(ctx, self.frame.host, self.budget, completed) {
                self.outcome.slices += paired.returns;
                self.outcome.pairs += paired.returns;
                (paired.result.map(|()| 0), true)
            } else {
                (
                    self.frame
                        .host
                        .canonical_slice(ctx, self.budget - completed)
                        .map(|instructions| completed + instructions),
                    false,
                )
            }
        } else {
            (result, false)
        };
        if let Ok(instructions) = result {
            self.frame.host.charge_instructions(instructions);
        }
        if !charged {
            self.frame.host.charge_native_slice(0);
        }
        self.outcome.result = result.map(|_| ());
    }

    fn complete_rooted(&mut self, view: &mut View) -> RootedCompletion {
        if self.frame.panic.is_some()
            || view.exit.instructions >= self.budget
            || self.limit - self.outcome.slices < 2
            || view.exit.pc as usize != self.region.pair.program.key().pc
            || !self.frame.host.pairing_enabled(self.frame.ctx)
        {
            return RootedCompletion::Unavailable;
        }
        let Some(roots) = self.roots.as_deref() else {
            return RootedCompletion::Unavailable;
        };
        let Some(mut snapshot) = abi::roots::call::Snapshot::new(roots, self.slots) else {
            return RootedCompletion::Unavailable;
        };
        let ready = self.frame.host.with_registers(|caller, registers| {
            assert_eq!(caller, self.admitted.caller());
            if registers.stack_frame.len() < snapshot.len() {
                return false;
            }
            *registers.pc = view.exit.pc as usize;
            true
        });
        if !ready {
            return RootedCompletion::Unavailable;
        }
        self.rooted_pending = true;
        record(
            self.frame.ctx,
            &view.exit,
            std::mem::take(&mut self.frame.count),
        );
        #[cfg(test)]
        tests::rooted_checkpoint(1);
        let completed = self.admitted.invoke_rooted(
            self.frame.host,
            self.budget,
            view.exit.instructions,
            &mut snapshot,
        );
        #[cfg(test)]
        if completed {
            tests::ROOTED_COMPLETIONS.with(|count| count.set(count.get() + 1));
            tests::rooted_checkpoint(2);
        }
        if completed {
            self.outcome.slices += 2;
            self.outcome.pairs += 1;
        }
        let continuing = completed
            && self.outcome.slices < self.limit
            && self.frame.host.fuel().should_continue()
            && self.frame.host.lua_ready()
            && self.frame.host.frame_identity() == self.identity;
        let pc = self.frame.host.with_registers(|caller, registers| {
            let pc = (continuing
                && caller == self.admitted.caller()
                && registers.stack_frame.len() >= snapshot.len()
                && self.region.caller.accepts_pc(*registers.pc))
            .then_some(*registers.pc);
            if pc.is_none() {
                assert!(snapshot.publish(registers.stack_frame));
            }
            pc
        });
        self.rooted_pending = pc.is_some();
        if !completed {
            return RootedCompletion::Declined;
        }
        let Some(pc) = pc else {
            return RootedCompletion::Complete(0);
        };
        #[cfg(test)]
        tests::ROOTED_DEFERRED.with(|count| count.set(count.get() + 1));
        view.pc = pc as u64;
        self.publish(view);
        RootedCompletion::Complete(1)
    }

    fn complete(&mut self, view: &mut View) -> u32 {
        let ctx = self.frame.ctx;
        self.outcome.fragments += 1;
        let recorded = match self.complete_rooted(view) {
            RootedCompletion::Unavailable => false,
            RootedCompletion::Declined => true,
            RootedCompletion::Complete(result) => return result,
        };
        let transition = self.frame.host.with_registers(|closure, registers| {
            assert_eq!(closure, self.admitted.caller());
            if !recorded {
                if let Some(roots) = self.roots.as_deref() {
                    assert!(abi::roots::materialize(
                        roots,
                        self.slots,
                        registers.stack_frame
                    ));
                } else {
                    for (slot, value) in self.slots.iter().zip(registers.stack_frame.iter_mut()) {
                        slot.write_back(value);
                    }
                }
                if self.frame.panic.is_none() {
                    *registers.pc = view.exit.pc as usize;
                }
            }
            closure
                .prototype()
                .opcodes
                .get(*registers.pc)
                .and_then(|op| op.call_transition())
        });
        self.rooted_pending = false;
        if !recorded {
            record(ctx, &view.exit, std::mem::take(&mut self.frame.count));
        }
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
                && self.frame.host.pairing_enabled(ctx)
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
            } else if matches!(transition, crate::opcode::CallTransition::Call { .. })
                && view.exit.pc as usize != self.region.pair.program.key().pc
            {
                self.fallback(&view.exit);
            } else {
                self.outcome.result =
                    self.frame
                        .host
                        .native_transition(ctx, transition, view.exit.instructions);
                self.outcome.slices += 1;
                return 0;
            }
        } else {
            self.fallback(&view.exit);
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
                || closure != self.admitted.caller()
                || !self.region.caller.accepts_pc(*registers.pc)
            {
                return None;
            }
            if let Some(roots) = self.roots.as_deref_mut() {
                assert!(abi::roots::capture(
                    roots,
                    self.slots,
                    registers.stack_frame
                ));
            } else {
                for (slot, value) in self
                    .slots
                    .iter_mut()
                    .zip(registers.stack_frame.iter().copied())
                {
                    *slot = abi::Slot::from_value(value);
                }
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
    stats.native_region_fragments = stats.native_region_fragments.saturating_add(1);
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
            if session.rooted_pending {
                session.frame.host.with_registers(|_, registers| {
                    assert!(abi::roots::materialize(
                        session.roots.as_deref().unwrap(),
                        session.slots,
                        registers.stack_frame
                    ));
                });
                session.rooted_pending = false;
            }
            session.panic = Some(payload);
            0
        }
    }
}

#[cfg(test)]
mod tests;
