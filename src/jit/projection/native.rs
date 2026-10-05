use std::{marker::PhantomData, ptr};

use super::*;

const VERSION: u64 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct View {
    version: u64,
    bindings: *mut u32,
    binding_count: u64,
    cells: *mut Cell,
    cell_count: u64,
    slots: *mut Slot,
    slot_count: u64,
    counts: *mut Counts,
}

type Entry = unsafe extern "C" fn(*mut View, u32, u32) -> u32;

struct Session<'a, 'gc, const CAPACITY: usize> {
    view: *mut View,
    targets: &'a mut [Option<Target<'gc>>; CAPACITY],
    binding_count: &'a mut usize,
    cell_count: &'a mut usize,
    buffers: PhantomData<&'a mut Projection<'gc, CAPACITY>>,
    scratch: PhantomData<&'a mut [Slot]>,
}

impl<'gc, const CAPACITY: usize> Projection<'gc, CAPACITY> {
    fn with_native<R>(
        &mut self,
        scratch: &mut [Slot],
        call: impl FnOnce(&mut Session<'_, 'gc, CAPACITY>) -> R,
    ) -> Result<R, Error> {
        if scratch.len() != self.slot_count {
            return Err(Error::Capacity);
        }
        let mut view = View {
            version: VERSION,
            bindings: self.bindings.as_mut_ptr(),
            binding_count: self.binding_count as u64,
            cells: self.cells.as_mut_ptr(),
            cell_count: self.cell_count as u64,
            slots: scratch.as_mut_ptr(),
            slot_count: scratch.len() as u64,
            counts: ptr::from_mut(&mut self.counts),
        };
        let mut session = Session {
            view: ptr::from_mut(&mut view),
            targets: &mut self.targets,
            binding_count: &mut self.binding_count,
            cell_count: &mut self.cell_count,
            buffers: PhantomData,
            scratch: PhantomData,
        };
        Ok(call(&mut session))
    }
}

impl<'gc, const CAPACITY: usize> Session<'_, 'gc, CAPACITY> {
    /// Entry preserves descriptor pointers/lengths, accesses only its buffers, and retains no pointers.
    unsafe fn invoke(&mut self, entry: Entry, a: u32, b: u32) -> u32 {
        unsafe { entry(self.view, a, b) }
    }

    fn view(&self) -> View {
        unsafe { self.view.read() }
    }

    fn flush(
        &mut self,
        ctx: Context<'gc>,
        registers: &mut LuaRegisters<'gc, '_>,
    ) -> Result<(), Error> {
        let view = self.view();
        let scratch = unsafe { std::slice::from_raw_parts(view.slots, view.slot_count as usize) };
        for index in 0..*self.cell_count {
            let cell = unsafe { view.cells.add(index).read() };
            if cell.dirty != 0 || cell.register != DETACHED {
                pending(registers, self.targets[index], cell, scratch)?;
            }
        }
        for index in 0..*self.cell_count {
            let cell = unsafe { view.cells.add(index).read() };
            if cell.dirty == 0 && cell.register == DETACHED {
                continue;
            }
            let value = pending(registers, self.targets[index], cell, scratch)?;
            match self.targets[index].unwrap() {
                Target::Closed(upvalue) => upvalue.set(&ctx, UpValueState::Closed(value)),
                Target::Upper(index) => registers.projection_write(true, index, value),
                Target::Register(index) => registers.projection_write(false, index, value),
            }
            unsafe { ptr::addr_of_mut!((*view.cells.add(index)).dirty).write(0) };
        }
        Ok(())
    }

    fn refresh(
        &mut self,
        registers: &LuaRegisters<'gc, '_>,
        upvalues: &[UpValue<'gc>],
    ) -> Result<(), Error> {
        let view = self.view();
        for index in 0..*self.cell_count {
            if unsafe { view.cells.add(index).read().dirty } != 0 {
                return Err(Error::PendingWrites);
            }
        }
        let scratch = unsafe { std::slice::from_raw_parts(view.slots, view.slot_count as usize) };
        let refreshed = Projection::<CAPACITY>::new(registers, upvalues, scratch)?;
        unsafe {
            ptr::copy_nonoverlapping(refreshed.bindings.as_ptr(), view.bindings, CAPACITY);
            ptr::copy_nonoverlapping(refreshed.cells.as_ptr(), view.cells, CAPACITY);
        }
        *self.targets = refreshed.targets;
        *self.binding_count = refreshed.binding_count;
        *self.cell_count = refreshed.cell_count;
        unsafe {
            ptr::addr_of_mut!((*self.view).binding_count).write(refreshed.binding_count as u64);
            ptr::addr_of_mut!((*self.view).cell_count).write(refreshed.cell_count as u64);
        }
        Ok(())
    }

    fn counts(&self) -> Counts {
        unsafe { self.view().counts.read() }
    }

    fn slot(&self, index: usize) -> Option<Slot> {
        let view = self.view();
        (index < view.slot_count as usize).then(|| unsafe { view.slots.add(index).read() })
    }

    fn store(&mut self, index: usize, value: Slot) -> Option<()> {
        let view = self.view();
        if index >= view.slot_count as usize {
            return None;
        }
        unsafe { view.slots.add(index).write(value) };
        Some(())
    }
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<View>() == 64);
    assert!(std::mem::offset_of!(View, bindings) == 8);
    assert!(std::mem::offset_of!(View, binding_count) == 16);
    assert!(std::mem::offset_of!(View, cells) == 24);
    assert!(std::mem::offset_of!(View, cell_count) == 32);
    assert!(std::mem::offset_of!(View, slots) == 40);
    assert!(std::mem::offset_of!(View, slot_count) == 48);
    assert!(std::mem::offset_of!(View, counts) == 56);
};

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn resolve(pointer: *mut View, binding: u32) -> Option<(View, *mut Cell)> {
        if pointer.is_null() {
            return None;
        }
        let view = unsafe { pointer.read() };
        if view.version != VERSION
            || view.binding_count > LIMIT as u64
            || view.cell_count > LIMIT as u64
            || view.slot_count > LIMIT as u64
            || u64::from(binding) >= view.binding_count
            || view.bindings.is_null()
            || view.cells.is_null()
            || view.slots.is_null()
            || view.counts.is_null()
        {
            return None;
        }
        let index = unsafe { view.bindings.add(binding as usize).read() };
        if u64::from(index) >= view.cell_count {
            return None;
        }
        Some((view, unsafe { view.cells.add(index as usize) }))
    }

    unsafe fn value_pointer(view: View, cell: *mut Cell) -> Option<*mut Slot> {
        let register = unsafe { ptr::addr_of!((*cell).register).read() };
        if register == DETACHED {
            Some(unsafe { ptr::addr_of_mut!((*cell).value) })
        } else if register < view.slot_count {
            Some(unsafe { view.slots.add(register as usize) })
        } else {
            None
        }
    }

    unsafe extern "C" fn get(view: *mut View, dest: u32, binding: u32) -> u32 {
        let Some((view, cell)) = (unsafe { resolve(view, binding) }) else {
            return abi::HELPER_DECLINED;
        };
        if u64::from(dest) >= view.slot_count {
            return abi::HELPER_DECLINED;
        }
        let Some(source) = (unsafe { value_pointer(view, cell) }) else {
            return abi::HELPER_DECLINED;
        };
        let value = unsafe { source.read() };
        if !scalar(value) {
            return abi::HELPER_DECLINED;
        }
        let mut counts = unsafe { view.counts.read() };
        let Some(reads) = counts.reads.checked_add(1) else {
            return abi::HELPER_DECLINED;
        };
        counts.reads = reads;
        unsafe {
            view.slots.add(dest as usize).write(value);
            view.counts.write(counts);
        }
        abi::HELPER_COMPLETED
    }

    unsafe extern "C" fn set(view: *mut View, binding: u32, source: u32) -> u32 {
        let Some((view, cell)) = (unsafe { resolve(view, binding) }) else {
            return abi::HELPER_DECLINED;
        };
        if u64::from(source) >= view.slot_count {
            return abi::HELPER_DECLINED;
        }
        let Some(dest) = (unsafe { value_pointer(view, cell) }) else {
            return abi::HELPER_DECLINED;
        };
        let value = unsafe { view.slots.add(source as usize).read() };
        if !scalar(value) {
            return abi::HELPER_DECLINED;
        }
        let mut counts = unsafe { view.counts.read() };
        let Some(writes) = counts.writes.checked_add(1) else {
            return abi::HELPER_DECLINED;
        };
        counts.writes = writes;
        unsafe {
            dest.write(value);
            ptr::addr_of_mut!((*cell).dirty).write(1);
            view.counts.write(counts);
        }
        abi::HELPER_COMPLETED
    }

    #[test]
    fn descriptor_buffers_survive_flush_and_in_place_refresh() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let first = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(7)));
            let second = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(11)));
            let mut canonical = [Value::Integer(42), Value::Nil];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                let mut scratch = [
                    Slot::from_value(registers.stack_frame[0]),
                    Slot::from_value(registers.stack_frame[1]),
                ];
                let mut projection =
                    Projection::<2>::new(&registers, &[first, first], &scratch).unwrap();
                projection
                    .with_native(&mut scratch, |session| {
                        let original = session.view();
                        unsafe {
                            assert_eq!(session.invoke(set, 0, 0), abi::HELPER_COMPLETED);
                            assert_eq!(session.invoke(get, 1, 1), abi::HELPER_COMPLETED);
                        }
                        assert_eq!(session.slot(1).unwrap().bits, 42);
                        assert_eq!(
                            session.refresh(&registers, &[first, second]),
                            Err(Error::PendingWrites)
                        );
                        session.flush(ctx, &mut registers).unwrap();
                        assert!(matches!(
                            first.get(),
                            UpValueState::Closed(Value::Integer(42))
                        ));
                        first.set(&ctx, UpValueState::Closed(Value::Integer(99)));
                        session.refresh(&registers, &[first, second]).unwrap();
                        let refreshed = session.view();
                        assert_eq!(original.bindings, refreshed.bindings);
                        assert_eq!(original.cells, refreshed.cells);
                        assert_eq!(original.slots, refreshed.slots);
                        assert_eq!(original.counts, refreshed.counts);
                        assert_eq!(refreshed.cell_count, 2);
                        unsafe {
                            assert_eq!(session.invoke(get, 0, 0), abi::HELPER_COMPLETED);
                            assert_eq!(session.invoke(get, 1, 1), abi::HELPER_COMPLETED);
                        }
                        assert_eq!(session.slot(0).unwrap().bits, 99);
                        assert_eq!(session.slot(1).unwrap().bits, 11);
                        assert_eq!(
                            session.counts(),
                            Counts {
                                reads: 3,
                                writes: 1
                            }
                        );
                        session.flush(ctx, &mut registers).unwrap();
                    })
                    .unwrap();
                assert_eq!(projection.cell_count, 2);
                assert_eq!(
                    projection.counts,
                    Counts {
                        reads: 3,
                        writes: 1
                    }
                );
                assert_eq!(projection.read(1, &scratch).unwrap().bits, 11);
            });
        });
    }

    #[test]
    fn shared_current_frame_cells_observe_native_and_ordinary_slot_writes() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let mut canonical = [Value::Integer(7), Value::Nil, Value::Nil];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                let first = registers.projection_open_at(ctx, 0);
                let second = registers.projection_open_at(ctx, 0);
                let mut scratch = [Slot::from_value(Value::Integer(7)); 3];
                let mut projection =
                    Projection::<2>::new(&registers, &[first, second], &scratch).unwrap();
                projection
                    .with_native(&mut scratch, |session| {
                        unsafe {
                            assert_eq!(session.invoke(get, 1, 0), abi::HELPER_COMPLETED);
                        }
                        session
                            .store(0, Slot::from_value(Value::Integer(11)))
                            .unwrap();
                        unsafe {
                            assert_eq!(session.invoke(get, 2, 1), abi::HELPER_COMPLETED);
                            assert_eq!(session.invoke(set, 0, 1), abi::HELPER_COMPLETED);
                        }
                        assert_eq!(session.slot(2).unwrap().bits, 11);
                        assert_eq!(session.slot(0).unwrap().bits, 7);
                        session.flush(ctx, &mut registers).unwrap();
                        assert!(matches!(registers.stack_frame[0], Value::Integer(7)));
                        session
                            .store(0, Slot::from_value(Value::Integer(99)))
                            .unwrap();
                        session.flush(ctx, &mut registers).unwrap();
                        assert!(matches!(registers.stack_frame[0], Value::Integer(99)));
                        assert_eq!(
                            session.counts(),
                            Counts {
                                reads: 2,
                                writes: 1
                            }
                        );
                    })
                    .unwrap();
                assert_eq!(projection.cells[0].dirty, 0);
                assert_eq!(projection.cell_count, 1);
            });
        });
    }

    #[test]
    fn descriptor_guards_decline_before_slots_cells_or_counts_change() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let cell = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(7)));
            let mut canonical = [Value::Integer(42)];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |registers| {
                let mut scratch = [Slot::from_value(Value::Integer(42))];
                let mut projection = Projection::<1>::new(&registers, &[cell], &scratch).unwrap();
                projection
                    .with_native(&mut scratch, |session| {
                        let original = session.view();
                        unsafe {
                            assert_eq!(get(ptr::null_mut(), 0, 0), abi::HELPER_DECLINED);
                            for (entry, a, b) in
                                [(get as Entry, 1, 0), (get, 0, 1), (set, 0, 1), (set, 1, 0)]
                            {
                                assert_eq!(session.invoke(entry, a, b), abi::HELPER_DECLINED);
                            }
                            for bad in [
                                View {
                                    version: VERSION + 1,
                                    ..original
                                },
                                View {
                                    binding_count: LIMIT as u64 + 1,
                                    ..original
                                },
                                View {
                                    cell_count: LIMIT as u64 + 1,
                                    ..original
                                },
                                View {
                                    slot_count: LIMIT as u64 + 1,
                                    ..original
                                },
                                View {
                                    bindings: ptr::null_mut(),
                                    ..original
                                },
                                View {
                                    cells: ptr::null_mut(),
                                    ..original
                                },
                                View {
                                    slots: ptr::null_mut(),
                                    ..original
                                },
                                View {
                                    counts: ptr::null_mut(),
                                    ..original
                                },
                            ] {
                                session.view.write(bad);
                                assert_eq!(session.invoke(get, 0, 0), abi::HELPER_DECLINED);
                                assert_eq!(session.invoke(set, 0, 0), abi::HELPER_DECLINED);
                            }
                            session.view.write(original);
                            original.bindings.write(FALLBACK);
                            assert_eq!(session.invoke(get, 0, 0), abi::HELPER_DECLINED);
                            original.bindings.write(0);
                            ptr::addr_of_mut!((*original.cells).register).write(1);
                            assert_eq!(session.invoke(get, 0, 0), abi::HELPER_DECLINED);
                            assert_eq!(session.invoke(set, 0, 0), abi::HELPER_DECLINED);
                            ptr::addr_of_mut!((*original.cells).register).write(DETACHED);
                            assert_eq!(original.cells.read().dirty, 0);
                            assert_eq!(original.cells.read().value.bits, 7);
                        }
                        assert_eq!(session.slot(0).unwrap().bits, 42);
                        assert_eq!(session.counts(), Counts::default());
                    })
                    .unwrap();
            });
        });
    }

    #[test]
    fn counts_overflow_and_reference_markers_have_no_partial_effects() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let cell = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(7)));
            let mut canonical = [Value::Nil];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |registers| {
                let mut scratch = [Slot::from_value(Value::Integer(42))];
                let mut projection = Projection::<1>::new(&registers, &[cell], &scratch).unwrap();
                projection.counts = Counts {
                    reads: u32::MAX,
                    writes: u32::MAX,
                };
                projection
                    .with_native(&mut scratch, |session| {
                        unsafe {
                            assert_eq!(session.invoke(get, 0, 0), abi::HELPER_DECLINED);
                            assert_eq!(session.invoke(set, 0, 0), abi::HELPER_DECLINED);
                            assert_eq!(session.view().cells.read().dirty, 0);
                            session.view().counts.write(Counts::default());
                        }
                        for value in [
                            Slot {
                                tag: abi::REFERENCE,
                                bits: 0,
                            },
                            Slot {
                                tag: abi::NIL,
                                bits: 1,
                            },
                            Slot {
                                tag: abi::BOOLEAN,
                                bits: 2,
                            },
                            Slot {
                                tag: u64::MAX,
                                bits: 0,
                            },
                        ] {
                            session.store(0, value).unwrap();
                            unsafe {
                                assert_eq!(session.invoke(set, 0, 0), abi::HELPER_DECLINED);
                                ptr::addr_of_mut!((*session.view().cells).value).write(value);
                                assert_eq!(session.invoke(get, 0, 0), abi::HELPER_DECLINED);
                            }
                            assert_eq!(session.slot(0).unwrap().tag, value.tag);
                            assert_eq!(session.slot(0).unwrap().bits, value.bits);
                        }
                        assert_eq!(session.counts(), Counts::default());
                        unsafe {
                            assert_eq!(session.view().cells.read().dirty, 0);
                        }
                    })
                    .unwrap();
            });
        });
    }

    #[test]
    fn refresh_rebinds_to_current_frame_and_preserves_number_payloads() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let cell = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(7)));
            let mut canonical = [Value::Nil, Value::Integer(11)];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                let open = registers.projection_open_at(ctx, 1);
                let mut scratch = [
                    Slot::from_value(Value::Nil),
                    Slot::from_value(Value::Integer(11)),
                ];
                let mut projection = Projection::<1>::new(&registers, &[cell], &scratch).unwrap();
                projection
                    .with_native(&mut scratch, |session| {
                        cell.set(&ctx, open.get());
                        session.refresh(&registers, &[cell]).unwrap();
                        unsafe {
                            assert_eq!(session.invoke(get, 0, 0), abi::HELPER_COMPLETED);
                        }
                        assert_eq!(session.slot(0).unwrap().bits, 11);
                        for bits in [(-0.0f64).to_bits(), 0x7ff8_0000_0000_1234] {
                            session
                                .store(
                                    0,
                                    Slot {
                                        tag: abi::NUMBER,
                                        bits,
                                    },
                                )
                                .unwrap();
                            unsafe {
                                assert_eq!(session.invoke(set, 0, 0), abi::HELPER_COMPLETED);
                                assert_eq!(session.invoke(get, 0, 0), abi::HELPER_COMPLETED);
                            }
                            assert_eq!(session.slot(0).unwrap().bits, bits);
                            assert_eq!(session.slot(1).unwrap().bits, bits);
                            session.flush(ctx, &mut registers).unwrap();
                            let Value::Number(value) = registers.stack_frame[1] else {
                                panic!("scalar not materialized");
                            };
                            assert_eq!(value.to_bits(), bits);
                            session.refresh(&registers, &[cell]).unwrap();
                        }
                        assert_eq!(
                            session.counts(),
                            Counts {
                                reads: 3,
                                writes: 2
                            }
                        );
                    })
                    .unwrap();
            });
        });
    }

    #[test]
    fn empty_and_last_binding_use_only_the_declared_prefix() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let cell = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(7)));
            let mut canonical = [Value::Nil; 256];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                let mut empty = Projection::<0>::new(&registers, &[], &[]).unwrap();
                empty
                    .with_native(&mut [], |session| {
                        unsafe {
                            assert_eq!(session.invoke(get, 0, 0), abi::HELPER_DECLINED);
                        }
                        session.flush(ctx, &mut registers).unwrap();
                        session.refresh(&registers, &[]).unwrap();
                    })
                    .unwrap();
                let mut scratch = [Slot::from_value(Value::Integer(42)); 256];
                let mut projection =
                    Projection::<256>::new(&registers, &[cell; 256], &scratch).unwrap();
                assert_eq!(
                    projection.with_native(&mut scratch[..255], |_| ()),
                    Err(Error::Capacity)
                );
                projection
                    .with_native(&mut scratch, |session| {
                        unsafe {
                            assert_eq!(session.invoke(set, 255, 255), abi::HELPER_COMPLETED);
                            assert_eq!(session.invoke(get, 255, 255), abi::HELPER_COMPLETED);
                            assert_eq!(session.invoke(get, 256, 255), abi::HELPER_DECLINED);
                            assert_eq!(session.invoke(get, 255, 256), abi::HELPER_DECLINED);
                        }
                        session.flush(ctx, &mut registers).unwrap();
                        session.refresh(&registers, &[cell; 256]).unwrap();
                        assert_eq!(session.slot(255).unwrap().bits, 42);
                    })
                    .unwrap();
                assert!(matches!(
                    cell.get(),
                    UpValueState::Closed(Value::Integer(42))
                ));
                assert_eq!(
                    projection.counts,
                    Counts {
                        reads: 1,
                        writes: 1
                    }
                );
            });
        });
    }

    #[test]
    fn unwinding_the_scope_retains_pending_writes_for_rust_materialization() {
        use std::panic::{catch_unwind, AssertUnwindSafe};
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let cell = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(7)));
            let mut canonical = [Value::Nil];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                let mut scratch = [Slot::from_value(Value::Integer(42))];
                let mut projection = Projection::<1>::new(&registers, &[cell], &scratch).unwrap();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    projection
                        .with_native(&mut scratch, |session| {
                            unsafe {
                                assert_eq!(session.invoke(set, 0, 0), abi::HELPER_COMPLETED);
                            }
                            panic!("rust boundary panic");
                        })
                        .unwrap();
                }));
                assert!(result.is_err());
                assert_eq!(
                    projection.counts,
                    Counts {
                        reads: 0,
                        writes: 1
                    }
                );
                assert_eq!(projection.cells[0].dirty, 1);
                projection.flush(ctx, &mut registers, &scratch).unwrap();
                assert!(matches!(
                    cell.get(),
                    UpValueState::Closed(Value::Integer(42))
                ));
            });
        });
    }

    #[test]
    fn detached_upper_and_outside_prefix_cells_commit_through_typed_targets() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let mut canonical = [Value::Integer(7), Value::Integer(42), Value::Integer(11)];
            let mut pc = 0;
            LuaRegisters::projection_split_frame(
                ctx,
                &mut pc,
                &mut canonical,
                1,
                |mut registers| {
                    let upper = registers.projection_open_at(ctx, 0);
                    let outside = registers.projection_open_at(ctx, 2);
                    let mut scratch = [Slot::from_value(Value::Integer(42))];
                    let mut projection =
                        Projection::<2>::new(&registers, &[upper, outside], &scratch).unwrap();
                    projection
                        .with_native(&mut scratch, |session| {
                            unsafe {
                                assert_eq!(session.invoke(set, 0, 0), abi::HELPER_COMPLETED);
                                assert_eq!(session.invoke(set, 1, 0), abi::HELPER_COMPLETED);
                            }
                            session.flush(ctx, &mut registers).unwrap();
                            assert!(matches!(
                                registers.projection_read(true, 0),
                                Some(Value::Integer(42))
                            ));
                            assert!(matches!(registers.stack_frame[1], Value::Integer(42)));
                            session.refresh(&registers, &[upper, outside]).unwrap();
                            unsafe {
                                assert_eq!(session.invoke(get, 0, 1), abi::HELPER_COMPLETED);
                            }
                            assert_eq!(
                                session.counts(),
                                Counts {
                                    reads: 1,
                                    writes: 2
                                }
                            );
                        })
                        .unwrap();
                },
            );
        });
    }

    #[test]
    fn failed_refresh_and_flush_preserve_pending_cells_and_descriptor_identity() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let first = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(7)));
            let second = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(11)));
            let mut canonical = [Value::Integer(42)];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                let mut scratch = [Slot::from_value(Value::Integer(42))];
                let mut projection =
                    Projection::<2>::new(&registers, &[first, second], &scratch).unwrap();
                projection
                    .with_native(&mut scratch, |session| {
                        let original = session.view();
                        assert_eq!(
                            session.refresh(&registers, &[first; 3]),
                            Err(Error::Capacity)
                        );
                        assert_eq!(session.view().cell_count, 2);
                        assert_eq!(session.view().cells, original.cells);
                        unsafe {
                            assert_eq!(session.invoke(set, 0, 0), abi::HELPER_COMPLETED);
                            assert_eq!(session.invoke(set, 1, 0), abi::HELPER_COMPLETED);
                            ptr::addr_of_mut!((*original.cells.add(1)).value.tag).write(u64::MAX);
                        }
                        assert_eq!(session.flush(ctx, &mut registers), Err(Error::InvalidSlot));
                        assert!(matches!(
                            first.get(),
                            UpValueState::Closed(Value::Integer(7))
                        ));
                        assert!(matches!(
                            second.get(),
                            UpValueState::Closed(Value::Integer(11))
                        ));
                        unsafe {
                            assert_eq!(original.cells.read().dirty, 1);
                            ptr::addr_of_mut!((*original.cells.add(1)).value.tag)
                                .write(abi::INTEGER);
                        }
                        session.flush(ctx, &mut registers).unwrap();
                        assert!(matches!(
                            first.get(),
                            UpValueState::Closed(Value::Integer(42))
                        ));
                        assert!(matches!(
                            second.get(),
                            UpValueState::Closed(Value::Integer(42))
                        ));
                    })
                    .unwrap();
            });
        });
    }
}
