use super::abi::Slot;

mod tests;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Empty,
    Pending,
    Active,
    Ready,
}

pub(crate) struct Scratch {
    enabled: bool,
    phase: Phase,
    closure: usize,
    key: (usize, usize),
    count: usize,
    pc: usize,
    tail: usize,
    slots: [Slot; 8],
}

impl Scratch {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            phase: Phase::Empty,
            closure: 0,
            key: (0, 0),
            count: 0,
            pc: 0,
            tail: 0,
            slots: [Slot::canonical(); 8],
        }
    }
}

impl Scratch {
    pub(crate) fn invalidate(&mut self) {
        self.phase = Phase::Empty;
    }

    pub(crate) fn vm_boundary(&mut self) {
        if self.phase != Phase::Ready {
            self.invalidate();
        }
    }

    pub(crate) fn callback_boundary(&mut self, bottom: usize) {
        self.phase =
            if self.phase == Phase::Pending && self.key.1.checked_add(self.tail) == Some(bottom) {
                Phase::Active
            } else {
                Phase::Empty
            };
    }

    pub(crate) fn returned(&mut self) {
        self.phase = if self.phase == Phase::Active {
            Phase::Ready
        } else {
            Phase::Empty
        };
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    pub(super) fn invoke<'gc>(
        &mut self,
        runtime: &super::Runtime,
        code: &super::backend::Code,
        ctx: crate::Context<'gc>,
        closure: crate::Closure<'gc>,
        registers: &mut crate::thread::LuaRegisters<'gc, '_>,
        budget: u32,
    ) -> Option<u32> {
        match code.registers {
            1 => self.invoke_width::<1>(runtime, code, ctx, closure, registers, budget),
            2 => self.invoke_width::<2>(runtime, code, ctx, closure, registers, budget),
            3 => self.invoke_width::<3>(runtime, code, ctx, closure, registers, budget),
            4 => self.invoke_width::<4>(runtime, code, ctx, closure, registers, budget),
            5 => self.invoke_width::<5>(runtime, code, ctx, closure, registers, budget),
            6 => self.invoke_width::<6>(runtime, code, ctx, closure, registers, budget),
            7 => self.invoke_width::<7>(runtime, code, ctx, closure, registers, budget),
            8 => self.invoke_width::<8>(runtime, code, ctx, closure, registers, budget),
            _ => {
                self.invalidate();
                None
            }
        }
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[inline(never)]
    fn invoke_width<'gc, const N: usize>(
        &mut self,
        runtime: &super::Runtime,
        code: &super::backend::Code,
        ctx: crate::Context<'gc>,
        closure: crate::Closure<'gc>,
        registers: &mut crate::thread::LuaRegisters<'gc, '_>,
        budget: u32,
    ) -> Option<u32> {
        use crate::{opcode::CallTransition, Function, Value};
        use ottavino_gc_arena::Gc;

        let count = N;
        if !self.enabled
            || !code.callback_calls
            || !registers
                .scratch_private
                .is_some_and(|private| private.get())
            || count > 8
            || registers.stack_frame.len() != count
            || code.entry_kind != super::backend::EntryKind::Ordinary
            || code.projected_upvalues
            || code.scalar_leaf.is_some()
        {
            self.invalidate();
            return None;
        }
        let key = registers.shadow_key();
        let identity = Gc::as_ptr(closure.into_inner()) as usize;
        let reuse = self.phase == Phase::Ready
            && self.key == key
            && self.closure == identity
            && self.count == count
            && self.pc == *registers.pc;
        let from = if reuse { self.tail } else { 0 };
        self.invalidate();
        for (slot, value) in self.slots[from..count]
            .iter_mut()
            .zip(registers.stack_frame[from..count].iter().copied())
        {
            *slot = Slot::from_value(value);
        }
        let counts = registers.callback_scratch_counts.as_deref_mut().unwrap();
        counts.0 = counts.0.saturating_add(u64::from(reuse));
        counts.1 = counts.1.saturating_add((count - from) as u64);
        let completed = runtime.invoke_frame::<N, false, false>(
            code,
            ctx,
            closure,
            registers,
            (self.slots.as_mut_ptr(), None, count),
            budget,
        );
        if let Some(CallTransition::Call {
            func,
            args,
            returns,
        }) = closure
            .prototype()
            .opcodes
            .get(*registers.pc)
            .and_then(|op| op.call_transition())
        {
            let tail = usize::from(func.0);
            if args.to_constant().is_some()
                && returns.to_constant().is_some()
                && matches!(
                    registers.stack_frame.get(tail),
                    Some(Value::Function(Function::Callback(_)))
                )
                && registers
                    .scratch_private
                    .is_some_and(|private| private.get())
            {
                self.key = key;
                self.closure = identity;
                self.count = count;
                self.pc = registers.pc.checked_add(1).unwrap();
                self.tail = tail;
                self.phase = Phase::Pending;
            }
        }
        Some(completed)
    }
}

impl super::Runtime {
    pub(crate) fn callback_scratch_enabled(&self) -> bool {
        #[cfg(all(
            not(miri),
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        {
            let manager = self.0.borrow();
            return manager.config.mode == super::JitMode::Auto
                && manager.memory_failure == super::backend::Failure::PreferCallbackScratch;
        }
        #[cfg(not(all(
            not(miri),
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )))]
        false
    }
}
