use super::*;
use crate::{jit::ir::Snapshot, opcode::Operation, opcode::RCIndex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit) struct Plan {
    lookup: super::Plan,
    pub entry: usize,
    pub loop_pc: usize,
    pub finish_pc: usize,
    pub return_pc: usize,
    roles: [u8; 6],
    argument: u8,
    finish: u8,
    cookie: [u64; 2],
}

#[repr(C)]
pub(in crate::jit) struct View {
    pub cookie: [u64; 2],
    pub cells: [*mut i64; 6],
    pub argument: i64,
    pub finish: i64,
    pub executed: u64,
}

pub(in crate::jit) type Entry = unsafe extern "C" fn(*mut View, u32, *mut abi::Exit);

#[cfg(test)]
thread_local! {
    static ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static ENTRIES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(in crate::jit) fn enabled() -> bool {
    ENABLED.get()
}
#[cfg(not(test))]
pub(in crate::jit) const fn enabled() -> bool {
    true
}
#[cfg(test)]
pub(in crate::jit) fn record_entry() {
    ENTRIES.set(ENTRIES.get() + 1);
}
#[cfg(test)]
pub(in crate::jit) fn with_enabled<R>(enabled: bool, run: impl FnOnce() -> R) -> (R, u64) {
    struct Restore(bool, u64);
    impl Drop for Restore {
        fn drop(&mut self) {
            ENABLED.set(self.0);
            ENTRIES.set(self.1);
        }
    }
    let _restore = Restore(ENABLED.replace(enabled), ENTRIES.replace(0));
    let result = run();
    (result, ENTRIES.get())
}

impl Plan {
    #[cfg(test)]
    pub(in crate::jit) fn roles(self) -> [u8; 6] {
        self.roles
    }

    pub(in crate::jit) fn recognize(source: &Snapshot) -> Option<Self> {
        let lookup = super::Plan::recognize(source)?;
        if !lookup.scalar_entry {
            return None;
        }
        let call = lookup.call_pc as usize;
        let entry = call.checked_add(1)?;
        let Operation::Call {
            func,
            args,
            returns,
        } = *source.operations.get(call)?
        else {
            return None;
        };
        if args.to_constant() != Some(1) || returns.to_constant() != Some(1) {
            return None;
        }
        let Operation::Add {
            dest: accumulator,
            left: RCIndex::Register(left),
            right: RCIndex::Register(right),
        } = *source.operations.get(entry)?
        else {
            return None;
        };
        if accumulator != left || right != func {
            return None;
        }
        let loop_pc = entry + 1;
        let Operation::NumericForLoop { base, jump } = *source.operations.get(loop_pc)? else {
            return None;
        };
        if (loop_pc + 1).checked_add_signed(isize::from(jump)) != Some(lookup.helper_pc as usize)
            || lookup.helper_pc as usize + 2 != call
        {
            return None;
        }
        let Operation::Move {
            dest: argument,
            source: index,
        } = source.operations[lookup.helper_pc as usize + 1]
        else {
            return None;
        };
        let base = base.0;
        if base > 252 || index.0 != base + 3 || argument.0 != func.0.checked_add(1)? {
            return None;
        }
        let roles = [accumulator.0, right.0, base, base + 1, base + 2, base + 3];
        for (i, role) in roles.iter().enumerate() {
            if roles[..i].contains(role) || *role == argument.0 {
                return None;
            }
        }
        let finish_pc = loop_pc + 1;
        let Operation::Move {
            dest: finish,
            source: result,
        } = *source.operations.get(finish_pc)?
        else {
            return None;
        };
        let return_pc = finish_pc + 1;
        let Operation::Return { start, count } = *source.operations.get(return_pc)? else {
            return None;
        };
        if result != accumulator || start != finish || count.to_constant() != Some(1) {
            return None;
        }
        u32::try_from(return_pc).ok()?;
        let mut binding = u64::from(argument.0) << 48 | u64::from(finish.0) << 56;
        for (i, role) in roles.into_iter().enumerate() {
            binding |= u64::from(role) << (8 * i);
        }
        Some(Self {
            lookup,
            entry,
            loop_pc,
            finish_pc,
            return_pc,
            roles,
            argument: argument.0,
            finish: finish.0,
            cookie: [(entry as u64) << 32 | return_pc as u64, binding],
        })
    }

    pub(in crate::jit) fn cookie(&self) -> [u64; 2] {
        self.cookie
    }

    /// Invokes matching native code through exclusive typed integer payload loans.
    ///
    /// # Safety
    /// Entry matches this plan, writes only its described cells and outputs,
    /// retains no pointers, cannot call users or collect, and cannot unwind.
    pub(in crate::jit) unsafe fn invoke<'gc>(
        &self,
        entry: Entry,
        closure: Closure<'gc>,
        hints: &EntryBytes,
        registers: &mut crate::thread::LuaRegisters<'gc, '_>,
        pc: usize,
        budget: u32,
    ) -> Result<Option<super::scalar_entry::Outcome>, Fault> {
        if pc != self.entry || registers.stack_frame.len() < usize::from(self.lookup.registers) {
            return Ok(None);
        }
        let mut scope = ();
        let Some(bound) = self.lookup.capture(closure, hints, &mut scope) else {
            return Ok(None);
        };
        let mut cells = [std::ptr::null_mut(); 6];
        let frame = registers.stack_frame.as_mut_ptr();
        for (cell, index) in cells.iter_mut().zip(self.roles) {
            // Verified distinct register indices lie within the checked frame.
            let value = unsafe { &mut *frame.add(usize::from(index)) };
            let Value::Integer(value) = value else {
                return Ok(None);
            };
            *cell = std::ptr::from_mut(value);
        }
        if cells.iter().any(|cell| cell.is_null()) {
            return Ok(None);
        }
        let mut view = View {
            cookie: self.cookie(),
            cells,
            argument: 0,
            finish: 0,
            executed: 0,
        };
        let mut exit = abi::Exit::default();
        unsafe { entry(&mut view, budget.min(64), &mut exit) };
        if ![0, 1, 3, 4].contains(&view.executed) || view.cookie != self.cookie() {
            return Err(Fault::Marker);
        }
        if exit.instructions > budget.min(64) {
            return Err(Fault::Budget);
        }
        if view.executed & 1 != 0 {
            registers.stack_frame[usize::from(self.lookup.destination())] = bound.value;
        }
        if view.executed & 2 != 0 {
            registers.stack_frame[usize::from(self.argument)] = Value::Integer(view.argument);
        }
        if view.executed & 4 != 0 {
            registers.stack_frame[usize::from(self.finish)] = Value::Integer(view.finish);
        }
        Ok(Some(super::scalar_entry::Outcome {
            exit,
            reads: view.executed & 1,
        }))
    }
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<View>() == 88);
    assert!(std::mem::offset_of!(View, cells) == 16);
    assert!(std::mem::offset_of!(View, argument) == 64);
    assert!(std::mem::offset_of!(View, finish) == 72);
    assert!(std::mem::offset_of!(View, executed) == 80);
};

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod lowering;

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(in crate::jit) use lowering::function;

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(in crate::jit) use lowering::verify;

#[cfg(test)]
mod tests;
