use super::Plan;
use crate::{jit::abi, Value};

const _: () = assert!(abi::NUMBER == 3);

#[derive(Clone, Copy, Debug)]
pub(in crate::jit) struct Source(Plan);

impl Source {
    pub(in crate::jit) fn from_plan(plan: Plan) -> Option<Self> {
        plan.scalar_entry.then_some(Self(plan))
    }

    #[cfg(test)]
    pub(in crate::jit) fn admit(source: &super::Snapshot) -> Option<Self> {
        Self::from_plan(Plan::recognize(source)?)
    }

    pub(in crate::jit) fn plan(self) -> Plan {
        self.0
    }

    #[cfg(test)]
    pub(in crate::jit) fn fits(self, slots: &[abi::Slot], frame: &[Value<'_>]) -> bool {
        self.fits_extents(slots, frame)
            && slots.iter().fold(0, |tags, slot| tags | slot.tag) <= abi::NUMBER
    }

    pub(in crate::jit) fn fits_extents(self, slots: &[abi::Slot], frame: &[Value<'_>]) -> bool {
        let width = usize::from(self.0.registers);
        slots.len() == width && frame.len() >= width
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
thread_local! {
    static ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    static ENTRIES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(in crate::jit) fn enabled() -> bool {
    ENABLED.get()
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
