use std::cell::{Cell, RefCell};

use super::{
    site::{kernel, Input, Site},
    Region,
};
use crate::{
    jit::{abi, backend, owner::Shared, work, JitError, Runtime, RuntimeOwner},
    opcode::Operation,
    thread::LuaRegisters,
    Closure, Context,
};

thread_local! {
    static ACTIVE: RefCell<Option<Lease>> = const { RefCell::new(None) };
}

#[derive(Clone)]
pub(crate) struct Lease(Shared<Prepared>);

impl std::ops::Deref for Lease {
    type Target = Prepared;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub(crate) struct Request {
    runtime: Runtime,
    source: u64,
    input: Input,
}

pub(crate) struct Prepared {
    runtime: Runtime,
    source: u64,
    code: backend::call_fusion::Code,
    view: kernel::View,
    site: Site,
    distances: [u16; 256],
    executions: Cell<u64>,
}

impl Request {
    pub(crate) fn new<'gc>(
        ctx: Context<'gc>,
        caller: Closure<'gc>,
    ) -> Result<Option<Self>, JitError> {
        let Some(source) = ctx
            .jit_registry()
            .borrow()
            .identity(ctx, caller.prototype())
        else {
            return Ok(None);
        };
        let (config, allocator) = {
            let manager = ctx.jit().0.borrow();
            (manager.config.clone(), manager.snapshots.clone())
        };
        Ok(Input::new(
            &caller.prototype(),
            0,
            config.max_prototype_instructions,
            allocator,
        )?
        .map(|input| Self {
            runtime: ctx.jit().clone(),
            source,
            input,
        }))
    }

    pub(crate) fn compile(self) -> Result<Lease, JitError> {
        let (config, metadata, memory) = {
            let manager = self.runtime.0.borrow();
            (
                manager.config.clone(),
                manager.metadata.clone(),
                manager.memory.clone(),
            )
        };
        let plan = kernel::Plan::new(&self.input)?;
        let code = backend::call_fusion::compile(
            &plan,
            memory,
            config.max_code_bytes,
            metadata.clone(),
            work::Limits::from(&config),
        )?;
        let view = plan.view();
        let (parent, _, site) = self.input.sources();
        let mut distances = [u16::MAX; 256];
        distances[site.region().entry] = 0;
        for _ in 0..parent.operations.len() {
            let mut changed = false;
            for (pc, &op) in parent.operations.iter().enumerate().rev() {
                if pc == site.region().entry {
                    continue;
                }
                let mut next = [None; 2];
                match op {
                    Operation::Return { .. } => {}
                    Operation::Jump { offset, .. } => {
                        next[0] = (pc + 1).checked_add_signed(isize::from(offset))
                    }
                    Operation::NumericForPrep { jump, .. }
                    | Operation::NumericForLoop { jump, .. } => {
                        next = [Some(pc + 1), (pc + 1).checked_add_signed(isize::from(jump))];
                    }
                    _ => next[0] = Some(pc + 1),
                }
                let best = next
                    .into_iter()
                    .flatten()
                    .filter_map(|p| distances.get(p))
                    .copied()
                    .min()
                    .unwrap_or(u16::MAX)
                    .saturating_add(1);
                if best < distances[pc] {
                    distances[pc] = best;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        Shared::try_new(
            Prepared {
                runtime: self.runtime,
                source: self.source,
                code,
                view,
                site,
                distances,
                executions: Cell::new(0),
            },
            metadata,
        )
        .map(Lease)
        .map_err(|_| JitError::ResourceLimit("fused prepared owner"))
    }
}

impl Prepared {
    pub(crate) fn executions(&self) -> u64 {
        self.executions.get()
    }
    pub(crate) fn region(&self) -> Region {
        self.site.region()
    }

    pub(crate) fn funded(&self, fuel: &crate::Fuel, deferred: u32, requested: u32) -> Option<u32> {
        super::Permit::new(fuel, deferred, requested).map(|p| p.budget)
    }

    pub(crate) fn extra_fuel(&self) -> i32 {
        super::EXTRA_FUEL
    }

    pub(crate) fn matches<'gc>(&self, ctx: Context<'gc>, caller: Closure<'gc>) -> bool {
        RuntimeOwner::ptr_eq(&self.runtime.0, &ctx.jit().0)
            && ctx
                .jit_registry()
                .borrow()
                .identity(ctx, caller.prototype())
                == Some(self.source)
    }

    pub(crate) fn prefix_budget(&self, pc: usize, requested: u32) -> u32 {
        match self.distances.get(pc).copied() {
            Some(1..=u16::MAX) => requested.min(u32::from(self.distances[pc])),
            _ => requested,
        }
    }

    pub(crate) fn invoke<'gc>(
        &self,
        ctx: Context<'gc>,
        caller: Closure<'gc>,
        registers: &mut LuaRegisters<'gc, '_>,
        budget: u32,
    ) -> Option<u32> {
        if !self.matches(ctx, caller) {
            return None;
        }
        let count = self.region().registers;
        let mut scratch = [abi::Slot {
            tag: abi::NIL,
            bits: 0,
        }; 256];
        let slots = &mut scratch[..count];
        for (slot, &value) in slots.iter_mut().zip(registers.stack_frame.iter()) {
            *slot = abi::Slot::from_value(value);
        }
        let mut view = self.view;
        let exit = self
            .site
            .with_live_binding(caller, registers, |_| {
                self.code
                    .invoke_scratch(slots, self.region().entry as u64, budget, Some(&mut view))
            })?
            .ok()?;
        ctx.jit().0.borrow_mut().stats.record_native_exit(&exit);
        if exit.pc != self.region().resume as u64
            || exit.instructions != 5
            || exit.reason != crate::jit::exits::Kind::Interpreter as u32
            || !view.completed_from(&self.view)
        {
            return None;
        }
        if !self.site.publish(registers, slots) {
            return None;
        }
        let mut manager = ctx.jit().0.borrow_mut();
        manager.stats.native_upvalue_reads = manager.stats.native_upvalue_reads.saturating_add(1);
        manager.stats.native_upvalue_writes = manager.stats.native_upvalue_writes.saturating_add(1);
        self.executions.set(self.executions.get().saturating_add(1));
        Some(exit.instructions)
    }
}

pub(crate) fn current(ctx: Context<'_>, source: Option<u64>) -> Option<Lease> {
    ACTIVE.with(|active| {
        active
            .borrow()
            .as_ref()
            .filter(|p| {
                Some(p.source) == source && RuntimeOwner::ptr_eq(&p.runtime.0, &ctx.jit().0)
            })
            .cloned()
    })
}

pub(crate) fn with_prepared<R>(prepared: Lease, run: impl FnOnce() -> R) -> R {
    struct Restore(Option<Lease>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ACTIVE.with(|active| *active.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(ACTIVE.with(|active| active.borrow_mut().replace(prepared)));
    run()
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod timing;
