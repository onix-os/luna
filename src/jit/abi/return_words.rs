use super::{Exit, Host, Slot};

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Words {
    pub pc: u64,
    pub counts: u64,
}

impl Words {
    pub fn from_exit(exit: Exit) -> Self {
        Self {
            pc: exit.pc,
            counts: u64::from(exit.instructions) | (u64::from(exit.reason) << 32),
        }
    }

    pub fn into_exit(self) -> Exit {
        Exit {
            pc: self.pc,
            instructions: self.counts as u32,
            reason: (self.counts >> 32) as u32,
        }
    }
}

pub(crate) type Entry = unsafe extern "C" fn(*mut Slot, u64, u32, *mut Host) -> Words;

/// Calls a register-return entry with a bounded instruction budget.
///
/// # Safety
/// Slots, host data and executable code satisfy the entry's live exclusive call contract.
pub(crate) unsafe fn invoke(
    entry: Entry,
    slots: *mut Slot,
    pc: usize,
    budget: u32,
    host: *mut Host,
) -> Exit {
    unsafe { entry(slots, pc as u64, budget.min(64), host) }.into_exit()
}

const _: () = {
    assert!(std::mem::size_of::<Words>() == 16);
    assert!(std::mem::offset_of!(Words, pc) == 0);
    assert!(std::mem::offset_of!(Words, counts) == 8);
};

#[test]
fn exit_words_preserve_every_field_without_pc_truncation() {
    let mut seed = 0x1234_5678_9abc_def0u64;
    for pc in [0, 1, u64::from(u32::MAX), 1 << 32, 1 << 63, u64::MAX] {
        for instructions in [0, 1, 63, 64, 65, u32::MAX] {
            for reason in [0, 1, 2, 3, 1 << 31, u32::MAX] {
                let exit = Words::from_exit(Exit {
                    pc,
                    instructions,
                    reason,
                })
                .into_exit();
                assert_eq!(
                    (exit.pc, exit.instructions, exit.reason),
                    (pc, instructions, reason)
                );
            }
        }
    }
    for _ in 0..4096 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let words = Words {
            pc: seed.rotate_left(19),
            counts: seed,
        };
        assert_eq!(Words::from_exit(words.into_exit()), words);
    }
}

unsafe extern "C" fn rust_entry(slots: *mut Slot, pc: u64, budget: u32, host: *mut Host) -> Words {
    unsafe {
        (*slots).bits = pc;
        if !host.is_null() {
            *(*host).data.cast::<u32>() = budget;
        }
        Words::from_exit(Exit {
            pc,
            instructions: budget,
            reason: (*slots).tag as u32,
        })
    }
}

pub(crate) fn check_entry(entry: Entry) {
    for pc in [0, 1, 255, u32::MAX as usize, usize::MAX / 2, usize::MAX] {
        for budget in [0, 1, 63, 64, 65, u32::MAX] {
            for reason in [0, 1, 2, 3, 1 << 31, u32::MAX] {
                for null_host in [false, true] {
                    let mut slot = Slot {
                        tag: u64::from(reason),
                        bits: !pc as u64,
                    };
                    let mut observed = u32::MAX;
                    let mut host = Host {
                        data: std::ptr::from_mut(&mut observed).cast(),
                        projection: std::ptr::null_mut(),
                    };
                    let pointer = if null_host {
                        std::ptr::null_mut()
                    } else {
                        &mut host
                    };
                    let exit = unsafe { invoke(entry, &mut slot, pc, budget, pointer) };
                    assert_eq!(exit.pc, pc as u64);
                    assert_eq!(exit.instructions, budget.min(64));
                    assert_eq!(exit.reason, reason);
                    assert_eq!(slot.bits, pc as u64);
                    assert_eq!(slot.tag, u64::from(reason));
                    assert_eq!(observed, if null_host { u32::MAX } else { budget.min(64) });
                }
            }
        }
    }
}

#[test]
fn rust_return_entry_preserves_pointer_pc_host_and_budget() {
    check_entry(rust_entry);
}
