use super::{Entry, Exit, Host, Slot};

unsafe extern "C" fn rust_entry(
    slots: *mut Slot,
    pc: u64,
    budget: u32,
    exit: *mut Exit,
    host: *mut Host,
) {
    unsafe {
        (*slots).bits = pc;
        if !host.is_null() {
            *(*host).data.cast::<u32>() = budget;
        }
        std::ptr::addr_of_mut!((*exit).pc).write(pc);
        std::ptr::addr_of_mut!((*exit).reason).write((*slots).tag as u32);
        std::ptr::addr_of_mut!((*exit).instructions).write(budget);
    }
}

pub(crate) fn check_entry(entry: Entry) {
    for pc in [0, 1, 255, u32::MAX as usize, usize::MAX / 2, usize::MAX] {
        for budget in [0, 1, 63, 64, 65, u32::MAX] {
            for reason in [0, 1, 2, 3, 1 << 31, u32::MAX] {
                for null_host in [false, true] {
                    for bounded in [false, true] {
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
                        let exit = if bounded {
                            unsafe { super::invoke(entry, &mut slot, pc, budget, pointer) }
                        } else {
                            let mut output = std::mem::MaybeUninit::<Exit>::uninit();
                            unsafe {
                                entry(&mut slot, pc as u64, budget, output.as_mut_ptr(), pointer);
                                output.assume_init()
                            }
                        };
                        let count = if bounded { budget.min(64) } else { budget };
                        assert_eq!(exit.pc, pc as u64);
                        assert_eq!(exit.instructions, count);
                        assert_eq!(exit.reason, reason);
                        assert_eq!(slot.bits, pc as u64);
                        assert_eq!(slot.tag, u64::from(reason));
                        assert_eq!(observed, if null_host { u32::MAX } else { count });
                    }
                }
            }
        }
    }
}

#[test]
fn output_entry_initializes_every_field_before_read() {
    check_entry(rust_entry);
}
