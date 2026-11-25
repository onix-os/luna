use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex, MutexGuard, OnceLock,
    },
    thread::{JoinHandle, Thread},
    time::Duration,
};

use allocator_api2::vec::Vec;

use super::{
    atomic_owner::AtomicShared,
    compiler_launch::Child,
    resources::{BudgetAllocator, LedgerRef, MappingCounter},
    CompileError,
};

enum Slot {
    Empty,
    Reserved,
    Retained(Child<'static>),
    InFlight(#[cfg(test)] Option<super::compiler_launch::CleanupFaults>),
}

struct Queue {
    slots: Vec<Slot, BudgetAllocator>,
    limit: usize,
}

impl Queue {
    fn occupied(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| !matches!(slot, Slot::Empty))
            .count()
    }
}

struct Shared {
    queue: Mutex<Queue>,
    clients: AtomicUsize,
    wake: OnceLock<Thread>,
    completion: Mutex<Option<JoinHandle<()>>>,
}

fn complete(shared: AtomicShared<Shared>) {
    let mut next = 0;
    loop {
        let mut queue = shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let selected = (0..queue.slots.len())
            .map(|offset| (next + offset) % queue.slots.len())
            .find(|&index| matches!(queue.slots[index], Slot::Retained(_)));
        let Some(index) = selected else {
            let finished = shared.clients.load(Ordering::Acquire) == 0 && queue.occupied() == 0;
            drop(queue);
            if finished {
                break;
            }
            std::thread::park();
            continue;
        };
        #[cfg(test)]
        let faults = match &queue.slots[index] {
            Slot::Retained(child) => child.cleanup_faults(),
            _ => unreachable!(),
        };
        let Slot::Retained(mut child) = std::mem::replace(
            &mut queue.slots[index],
            Slot::InFlight(
                #[cfg(test)]
                faults,
            ),
        ) else {
            unreachable!()
        };
        next = (index + 1) % queue.slots.len();
        drop(queue);
        let _ = child.cleanup_once();
        let pending = if child.finished() {
            drop(child);
            None
        } else {
            Some(child)
        };
        let finished = pending.is_none();
        let mut queue = shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        queue.slots[index] = pending.map_or(Slot::Empty, Slot::Retained);
        drop(queue);
        if !finished {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(test)]
thread_local! {
    static FAIL_START: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn start(shared: AtomicShared<Shared>) -> std::io::Result<JoinHandle<()>> {
    #[cfg(test)]
    if FAIL_START.get() {
        return Err(std::io::Error::from_raw_os_error(libc::EAGAIN));
    }
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || complete(shared))
}

pub(crate) struct Supervisor(AtomicShared<Shared>);

impl Clone for Supervisor {
    fn clone(&self) -> Self {
        let shared = self.0.clone();
        shared
            .clients
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                count.checked_add(1)
            })
            .expect("compiler client count overflow");
        Self(shared)
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        if self.0.clients.fetch_sub(1, Ordering::AcqRel) == 1 {
            if let Some(wake) = self.0.wake.get() {
                wake.unpark();
            }
        }
    }
}

#[cfg(test)]
pub(crate) struct Completion(AtomicShared<Shared>);

#[cfg(test)]
impl Completion {
    pub fn join(self) {
        assert_eq!(self.0.clients.load(Ordering::Acquire), 0);
        let handle = self
            .0
            .completion
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(handle) = handle {
            handle.join().unwrap();
        }
    }
}

impl Supervisor {
    pub fn new(maximum: usize, metadata: BudgetAllocator) -> Result<Self, CompileError> {
        if maximum == 0 {
            return Err(CompileError::InvalidConfiguration("compiler job capacity"));
        }
        let mut slots = Vec::new_in(metadata.clone());
        slots
            .try_reserve_exact(maximum)
            .map_err(|_| CompileError::ResourceLimit("compiler jobs metadata"))?;
        slots.resize_with(maximum, || Slot::Empty);
        let shared = AtomicShared::try_new(
            Shared {
                queue: Mutex::new(Queue {
                    slots,
                    limit: maximum,
                }),
                clients: AtomicUsize::new(1),
                wake: OnceLock::new(),
                completion: Mutex::new(None),
            },
            metadata,
        )
        .map_err(|_| CompileError::ResourceLimit("compiler jobs metadata"))?;
        Ok(Self(shared))
    }

    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.0
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    #[cfg(test)]
    pub fn completion(&self) -> Completion {
        Completion(self.0.clone())
    }

    pub fn poll(&self) -> usize {
        if let Some(wake) = self.0.wake.get() {
            wake.unpark();
        }
        self.lock().occupied()
    }

    pub fn occupied(&self) -> usize {
        self.lock().occupied()
    }

    pub fn ensure_host(&self, host: &LedgerRef) -> Result<(), CompileError> {
        if MappingCounter::new(self.lock().slots.allocator().0.clone())
            .same_root(&MappingCounter::new(host.clone()))
        {
            Ok(())
        } else {
            Err(CompileError::InvalidConfiguration(
                "compiler supervisor host",
            ))
        }
    }

    #[cfg(test)]
    pub fn fail_cleanup(&self, signals: usize, waits: usize) {
        for slot in &mut self.lock().slots {
            match slot {
                Slot::Retained(child) => {
                    child.fail_cleanup(signals, waits);
                }
                Slot::InFlight(Some(faults)) => faults.reset(signals, waits),
                _ => {}
            }
        }
    }

    pub fn configure(&self, maximum: usize) -> Result<(), CompileError> {
        if maximum == 0 {
            return Err(CompileError::InvalidConfiguration("compiler job capacity"));
        }
        let mut queue = self.lock();
        if maximum > queue.slots.len() {
            let additional = maximum - queue.slots.len();
            queue
                .slots
                .try_reserve_exact(additional)
                .map_err(|_| CompileError::ResourceLimit("compiler jobs metadata"))?;
            queue.slots.resize_with(maximum, || Slot::Empty);
        }
        queue.limit = maximum;
        Ok(())
    }

    pub(super) fn reserve(&self) -> Result<Ticket, CompileError> {
        let mut queue = self.lock();
        if queue.occupied() >= queue.limit {
            return Err(CompileError::ResourceLimit("compiler job capacity"));
        }
        let index = queue
            .slots
            .iter()
            .position(|slot| matches!(slot, Slot::Empty))
            .ok_or(CompileError::ResourceLimit("compiler job capacity"))?;
        if self.0.wake.get().is_none() {
            let handle = start(self.0.clone())
                .map_err(|_| CompileError::ResourceLimit("compiler completion service"))?;
            let wake = handle.thread().clone();
            *self
                .0
                .completion
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(handle);
            self.0
                .wake
                .set(wake)
                .ok()
                .expect("compiler wake already set");
        }
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
        drop(queue);
        self.supervisor.0.wake.get().unwrap().unpark();
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
