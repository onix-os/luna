use std::sync::{Mutex, MutexGuard};

use allocator_api2::vec::Vec;

use super::{
    atomic_owner::AtomicShared,
    compiler_launch::Child,
    resources::{BudgetAllocator, LedgerRef, MappingCounter},
    JitError,
};

enum Slot {
    Empty,
    Reserved,
    Retained(Child<'static>),
}

struct Queue {
    slots: Vec<Slot, BudgetAllocator>,
    limit: usize,
}

impl Queue {
    fn poll(&mut self) {
        for slot in &mut self.slots {
            if let Slot::Retained(child) = slot {
                let _ = child.cleanup_once();
                if child.finished() {
                    *slot = Slot::Empty;
                }
            }
        }
    }

    fn occupied(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| !matches!(slot, Slot::Empty))
            .count()
    }
}

#[derive(Clone)]
pub(crate) struct Supervisor(AtomicShared<Mutex<Queue>>);

impl Supervisor {
    pub fn new(maximum: usize, metadata: BudgetAllocator) -> Result<Self, JitError> {
        if maximum == 0 {
            return Err(JitError::InvalidConfiguration("compiler job capacity"));
        }
        let mut slots = Vec::new_in(metadata.clone());
        slots
            .try_reserve_exact(maximum)
            .map_err(|_| JitError::ResourceLimit("compiler jobs metadata"))?;
        slots.resize_with(maximum, || Slot::Empty);
        AtomicShared::try_new(
            Mutex::new(Queue {
                slots,
                limit: maximum,
            }),
            metadata,
        )
        .map(Self)
        .map_err(|_| JitError::ResourceLimit("compiler jobs metadata"))
    }

    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn poll(&self) -> usize {
        let mut queue = self.lock();
        queue.poll();
        queue.occupied()
    }

    pub fn occupied(&self) -> usize {
        self.lock().occupied()
    }

    pub fn ensure_host(&self, host: &LedgerRef) -> Result<(), JitError> {
        if MappingCounter::new(self.lock().slots.allocator().0.clone())
            .same_root(&MappingCounter::new(host.clone()))
        {
            Ok(())
        } else {
            Err(JitError::InvalidConfiguration("compiler supervisor host"))
        }
    }

    #[cfg(test)]
    pub fn fail_cleanup(&self, signals: usize, waits: usize) {
        for slot in &mut self.lock().slots {
            if let Slot::Retained(child) = slot {
                child.fail_cleanup(signals, waits);
            }
        }
    }

    pub fn configure(&self, maximum: usize) -> Result<(), JitError> {
        if maximum == 0 {
            return Err(JitError::InvalidConfiguration("compiler job capacity"));
        }
        let mut queue = self.lock();
        if maximum > queue.slots.len() {
            let additional = maximum - queue.slots.len();
            queue
                .slots
                .try_reserve_exact(additional)
                .map_err(|_| JitError::ResourceLimit("compiler jobs metadata"))?;
            queue.slots.resize_with(maximum, || Slot::Empty);
        }
        queue.limit = maximum;
        Ok(())
    }

    pub(super) fn reserve(&self) -> Result<Ticket, JitError> {
        let mut queue = self.lock();
        queue.poll();
        if queue.occupied() >= queue.limit {
            return Err(JitError::ResourceLimit("compiler job capacity"));
        }
        let index = queue
            .slots
            .iter()
            .position(|slot| matches!(slot, Slot::Empty))
            .ok_or(JitError::ResourceLimit("compiler job capacity"))?;
        queue.slots[index] = Slot::Reserved;
        Ok(Ticket {
            supervisor: self.clone(),
            index,
            reserved: true,
        })
    }
}

pub(super) struct Ticket {
    supervisor: Supervisor,
    index: usize,
    reserved: bool,
}

impl Ticket {
    pub fn retain(mut self, child: Child<'static>) {
        let mut queue = self.supervisor.lock();
        debug_assert!(matches!(queue.slots[self.index], Slot::Reserved));
        queue.slots[self.index] = Slot::Retained(child);
        self.reserved = false;
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if self.reserved {
            self.supervisor.lock().slots[self.index] = Slot::Empty;
        }
    }
}

#[cfg(test)]
thread_local! {
    static CURRENT: std::cell::RefCell<Option<Supervisor>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct Scope(Option<Supervisor>);

#[cfg(test)]
impl Scope {
    pub fn enter(supervisor: Option<Supervisor>) -> Self {
        Self(CURRENT.replace(supervisor))
    }
}

#[cfg(test)]
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.replace(self.0.take());
    }
}

#[cfg(test)]
pub(super) fn current() -> Option<Supervisor> {
    CURRENT.with(|current| current.borrow().clone())
}

#[cfg(test)]
mod tests;
