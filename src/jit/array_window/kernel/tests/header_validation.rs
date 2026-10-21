use super::super::super::native::{View, VERSION};
use super::*;

fn scratch(source: &Snapshot, plan: Plan) -> Vec<Slot> {
    let mut slots = vec![Slot::from_value(Value::Integer(3)); source.registers];
    for (offset, value) in [1, 9, 1, 1].into_iter().enumerate() {
        slots[usize::from(plan.base) + offset] = Slot::from_value(Value::Integer(value));
    }
    slots
}

fn invoke(
    entry: KernelEntry,
    slots: &mut [Slot],
    pc: usize,
    budget: u32,
    view: *const View,
) -> abi::Exit {
    let mut exit = abi::Exit::default();
    unsafe {
        entry(
            slots.as_mut_ptr(),
            pc as u64,
            budget,
            &mut exit,
            view.cast(),
        )
    };
    exit
}

#[test]
fn descriptor_proof_is_lazy_and_resets_at_every_native_entry() {
    for text in [
        "local t={} local sum=0 for i=1,9 do sum=sum+t[i] end return sum",
        "local t={} for i=1,9 do t[i]=i end return t",
    ] {
        Lua::empty().enter(|ctx| {
            let closure = Closure::load(ctx, None, text.as_bytes()).unwrap();
            let source = Snapshot::new(&closure.prototype(), 4096, 1024 * 1024).unwrap();
            let plan = (0..source.operations.len())
                .find_map(|pc| Plan::new(&source, pc))
                .unwrap();
            let access = (plan.start..plan.end)
                .find(|&pc| {
                    matches!(
                        source.operations[pc],
                        Operation::GetTable { .. } | Operation::SetTable { .. }
                    )
                })
                .unwrap();
            with_kernel(&source, plan, |entry| {
                let initial = scratch(&source, plan);
                let mut cells = [Slot::from_value(Value::Integer(7)); 64];
                let original_cells = cells;
                let mut dirty = 0u64;
                let mut counts = Counts::default();
                let valid = View {
                    version: VERSION,
                    slots: cells.as_mut_ptr(),
                    length: 64,
                    first: 1,
                    dirty: &mut dirty,
                    counts: &mut counts,
                    writable: u64::from(plan.access == Access::Write),
                };
                for bad in 0..11 {
                    let mut view = valid;
                    let mut slots = initial.clone();
                    let exit = invoke(entry, &mut slots, access, 1, &view);
                    assert_eq!((exit.pc, exit.instructions), ((access + 1) as u64, 1));
                    view = valid;
                    match bad {
                        0 => view.version = VERSION + 1,
                        1 => view.length = 0,
                        2 => view.length = 65,
                        3 => view.first = 0,
                        4 => view.first = -1,
                        5 => view.slots = std::ptr::null_mut(),
                        6 => view.dirty = std::ptr::null_mut(),
                        7 => view.counts = std::ptr::null_mut(),
                        8 => view.writable = 2,
                        9 => view.first = 2,
                        10 => view.length = u64::MAX,
                        _ => unreachable!(),
                    }
                    cells = original_cells;
                    dirty = 0;
                    counts = Counts::default();
                    let mut slots = initial.clone();
                    let exit = invoke(entry, &mut slots, access, 64, &view);
                    assert_eq!(
                        (exit.pc, exit.instructions, exit.reason),
                        (access as u64, 0, Kind::Guard as u32),
                        "bad={bad}"
                    );
                    same(&slots, &initial);
                    same(&cells, &original_cells);
                    assert_eq!(dirty, 0);
                    assert_eq!(counts, Counts::default());
                }
                for pc in plan.start..=plan.end {
                    let mut slots = initial.clone();
                    let exit = invoke(entry, &mut slots, pc, 0, std::ptr::null());
                    assert_eq!(
                        (exit.pc, exit.instructions, exit.reason),
                        (pc as u64, 0, Kind::Budget as u32)
                    );
                    same(&slots, &initial);
                }
                let mut slots = initial.clone();
                let exit = invoke(entry, &mut slots, plan.end + 1, 64, std::ptr::null());
                assert_eq!(
                    (exit.pc, exit.instructions, exit.reason),
                    ((plan.end + 1) as u64, 0, Kind::Interpreter as u32)
                );
                same(&slots, &initial);
                let exit = invoke(entry, &mut slots, access, 64, std::ptr::null());
                assert_eq!(
                    (exit.pc, exit.instructions, exit.reason),
                    (access as u64, 0, Kind::Guard as u32)
                );
                same(&slots, &initial);
                slots[usize::from(plan.base)] = Slot::from_value(Value::Integer(9));
                let exit = invoke(entry, &mut slots, plan.end, 64, std::ptr::null());
                assert_eq!(
                    (exit.pc, exit.instructions, exit.reason),
                    ((plan.end + 1) as u64, 1, Kind::Interpreter as u32)
                );
            });
        });
    }
}

#[test]
fn cached_read_proof_never_grants_write_permission() {
    Lua::empty().enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            b"local t={} for i=1,9 do t[i]=t[i]+3 end return t",
        )
        .unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 1024 * 1024).unwrap();
        let plan = (0..source.operations.len())
            .find_map(|pc| Plan::new(&source, pc))
            .unwrap();
        let read = (plan.start..plan.end)
            .find(|&pc| matches!(source.operations[pc], Operation::GetTable { .. }))
            .unwrap();
        let write = (read + 1..plan.end)
            .find(|&pc| matches!(source.operations[pc], Operation::SetTable { .. }))
            .unwrap();
        with_kernel(&source, plan, |entry| {
            let mut cells = [Slot::from_value(Value::Integer(7)); 64];
            let original = cells;
            let mut counts = Counts::default();
            let mut dirty = 0u64;
            let mut view = View {
                version: VERSION,
                slots: cells.as_mut_ptr(),
                length: 64,
                first: 1,
                dirty: &mut dirty,
                counts: &mut counts,
                writable: 1,
            };
            let mut expected = scratch(&source, plan);
            let prefix = invoke(entry, &mut expected, read, (write - read) as u32, &view);
            assert_eq!(
                (prefix.pc, prefix.instructions, prefix.reason),
                (write as u64, (write - read) as u32, Kind::Budget as u32)
            );
            assert_eq!(
                counts,
                Counts {
                    reads: 1,
                    writes: 0
                }
            );
            view.writable = 0;
            counts = Counts::default();
            let mut actual = scratch(&source, plan);
            let exit = invoke(entry, &mut actual, read, 64, &view);
            assert_eq!(
                (exit.pc, exit.instructions, exit.reason),
                (write as u64, (write - read) as u32, Kind::Guard as u32)
            );
            same(&actual, &expected);
            same(&cells, &original);
            assert_eq!(dirty, 0);
            assert_eq!(
                counts,
                Counts {
                    reads: 1,
                    writes: 0
                }
            );
            let exit = invoke(entry, &mut actual, write, 1, &view);
            assert_eq!(
                (exit.pc, exit.instructions, exit.reason),
                (write as u64, 0, Kind::Guard as u32)
            );
            view.writable = 1;
            let exit = invoke(entry, &mut actual, write, 1, &view);
            assert_eq!((exit.pc, exit.instructions), ((write + 1) as u64, 1));
            assert_eq!((cells[0].tag, cells[0].bits), (abi::INTEGER, 10));
            assert_eq!(dirty, 1);
            assert_eq!(
                counts,
                Counts {
                    reads: 1,
                    writes: 1
                }
            );
        });
    });
}

#[test]
fn cached_header_never_caches_cell_scalar_guards_or_counter_values() {
    Lua::empty().enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            b"local t={} local sum=0 for i=1,9 do sum=sum+t[i] end return sum",
        )
        .unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 1024 * 1024).unwrap();
        let plan = (0..source.operations.len())
            .find_map(|pc| Plan::new(&source, pc))
            .unwrap();
        let read = (plan.start..plan.end)
            .find(|&pc| matches!(source.operations[pc], Operation::GetTable { .. }))
            .unwrap();
        with_kernel(&source, plan, |entry| {
            for invalid in [
                Slot {
                    tag: abi::REFERENCE,
                    bits: 0,
                },
                Slot {
                    tag: abi::BOOLEAN,
                    bits: 2,
                },
                Slot {
                    tag: abi::NIL,
                    bits: 1,
                },
            ] {
                let mut cells = [Slot::from_value(Value::Integer(7)); 64];
                cells[1] = invalid;
                let mut counts = Counts {
                    reads: u32::MAX - 1,
                    writes: u32::MAX,
                };
                let mut dirty = u64::MAX;
                let view = View {
                    version: VERSION,
                    slots: cells.as_mut_ptr(),
                    length: 64,
                    first: 1,
                    dirty: &mut dirty,
                    counts: &mut counts,
                    writable: 0,
                };
                let mut slots = scratch(&source, plan);
                let exit = invoke(entry, &mut slots, read, 64, &view);
                assert_eq!(
                    (exit.pc, exit.instructions, exit.reason),
                    (
                        read as u64,
                        (plan.end - read + 1) as u32,
                        Kind::Guard as u32
                    )
                );
                assert_eq!(
                    counts,
                    Counts {
                        reads: u32::MAX,
                        writes: u32::MAX
                    }
                );
                assert_eq!(dirty, u64::MAX);
                assert_eq!((cells[1].tag, cells[1].bits), (invalid.tag, invalid.bits));
                cells[1] = Slot::from_value(Value::Integer(11));
                let exit = invoke(entry, &mut slots, read, 1, &view);
                assert_eq!((exit.pc, exit.instructions), ((read + 1) as u64, 1));
                assert_eq!((cells[1].tag, cells[1].bits), (abi::INTEGER, 11));
                assert_eq!(counts.reads, u32::MAX);
            }
        });
    });
}
