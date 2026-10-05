use ottavino_gc_arena::Gc;

use crate::{
    closure::{UpValue, UpValueState},
    thread::LuaRegisters,
    Context, Value,
};

use super::abi::{self, Slot};

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod lowering;
mod native;

const LIMIT: usize = 256;
const FALLBACK: u32 = u32::MAX;
const DETACHED: u64 = u64::MAX;

#[derive(Clone, Copy)]
pub(crate) enum Origin<'gc> {
    Closed(Value<'gc>),
    Upper(usize, Value<'gc>),
    Register(usize, Value<'gc>),
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Cell {
    value: Slot,
    register: u64,
    dirty: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Counts {
    reads: u32,
    writes: u32,
}

fn scalar(slot: Slot) -> bool {
    match slot.tag {
        abi::NIL => slot.bits == 0,
        abi::BOOLEAN => slot.bits <= 1,
        abi::INTEGER | abi::NUMBER => true,
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum Target<'gc> {
    Closed(UpValue<'gc>),
    Upper(usize),
    Register(usize),
}

impl Target<'_> {
    fn matches(self, other: Self) -> bool {
        match (self, other) {
            (Self::Closed(a), Self::Closed(b)) => Gc::ptr_eq(a.into_inner(), b.into_inner()),
            (Self::Upper(a), Self::Upper(b)) | (Self::Register(a), Self::Register(b)) => a == b,
            _ => false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Error {
    Capacity,
    PendingWrites,
    ChangedTarget,
    InvalidSlot,
}

struct Projection<'gc, const CAPACITY: usize> {
    bindings: [u32; CAPACITY],
    cells: [Cell; CAPACITY],
    targets: [Option<Target<'gc>>; CAPACITY],
    binding_count: usize,
    cell_count: usize,
    slot_count: usize,
    counts: Counts,
}

impl<'gc, const CAPACITY: usize> Projection<'gc, CAPACITY> {
    fn new(
        registers: &LuaRegisters<'gc, '_>,
        upvalues: &[UpValue<'gc>],
        scratch: &[Slot],
    ) -> Result<Self, Error> {
        if CAPACITY > LIMIT
            || upvalues.len() > CAPACITY
            || scratch.len() > LIMIT
            || scratch.len() > registers.stack_frame.len()
        {
            return Err(Error::Capacity);
        }
        let mut projection = Self {
            bindings: [FALLBACK; CAPACITY],
            cells: [Cell {
                value: Slot {
                    tag: abi::NIL,
                    bits: 0,
                },
                register: DETACHED,
                dirty: 0,
            }; CAPACITY],
            targets: [None; CAPACITY],
            binding_count: upvalues.len(),
            cell_count: 0,
            slot_count: scratch.len(),
            counts: Counts::default(),
        };
        for (binding, upvalue) in upvalues.iter().copied().enumerate() {
            let Some(origin) = registers.projection_origin(upvalue) else {
                continue;
            };
            let (target, value, register) = match origin {
                Origin::Closed(value) => (Target::Closed(upvalue), value, DETACHED),
                Origin::Upper(index, value) => (Target::Upper(index), value, DETACHED),
                Origin::Register(index, value) => (
                    Target::Register(index),
                    value,
                    if index < scratch.len() {
                        index as u64
                    } else {
                        DETACHED
                    },
                ),
            };
            let existing = projection.targets[..projection.cell_count]
                .iter()
                .position(|entry| entry.is_some_and(|entry| entry.matches(target)));
            let index = existing.unwrap_or(projection.cell_count);
            if existing.is_none() {
                projection.cells[index] = Cell {
                    value: Slot::from_value(value),
                    register,
                    dirty: 0,
                };
                projection.targets[index] = Some(target);
                projection.cell_count += 1;
            }
            projection.bindings[binding] = index as u32;
        }
        Ok(projection)
    }

    fn cell(&self, binding: usize) -> Option<usize> {
        let index = *self.bindings.get(binding)? as usize;
        (binding < self.binding_count && index < self.cell_count).then_some(index)
    }

    fn read(&mut self, binding: usize, scratch: &[Slot]) -> Option<Slot> {
        let cell = self.cells[self.cell(binding)?];
        let value = if cell.register == DETACHED {
            cell.value
        } else {
            *scratch.get(cell.register as usize)?
        };
        if !scalar(value) {
            return None;
        }
        self.counts.reads += 1;
        Some(value)
    }

    fn write(&mut self, binding: usize, scratch: &mut [Slot], value: Slot) -> Option<()> {
        if !scalar(value) {
            return None;
        }
        let index = self.cell(binding)?;
        let cell = &mut self.cells[index];
        if cell.register == DETACHED {
            cell.value = value;
        } else {
            *scratch.get_mut(cell.register as usize)? = value;
        }
        cell.dirty = 1;
        self.counts.writes += 1;
        Some(())
    }

    fn flush(
        &mut self,
        ctx: Context<'gc>,
        registers: &mut LuaRegisters<'gc, '_>,
        scratch: &[Slot],
    ) -> Result<(), Error> {
        for index in 0..self.cell_count {
            if self.cells[index].dirty == 0 && self.cells[index].register == DETACHED {
                continue;
            }
            self.pending(registers, scratch, index)?;
        }
        for index in 0..self.cell_count {
            if self.cells[index].dirty == 0 && self.cells[index].register == DETACHED {
                continue;
            }
            let value = self.pending(registers, scratch, index)?;
            match self.targets[index].unwrap() {
                Target::Closed(upvalue) => upvalue.set(&ctx, UpValueState::Closed(value)),
                Target::Upper(index) => registers.projection_write(true, index, value),
                Target::Register(index) => registers.projection_write(false, index, value),
            }
            self.cells[index].dirty = 0;
        }
        Ok(())
    }

    fn pending(
        &self,
        registers: &LuaRegisters<'gc, '_>,
        scratch: &[Slot],
        index: usize,
    ) -> Result<Value<'gc>, Error> {
        pending(registers, self.targets[index], self.cells[index], scratch)
    }

    fn refresh(
        &mut self,
        registers: &LuaRegisters<'gc, '_>,
        upvalues: &[UpValue<'gc>],
        scratch: &[Slot],
    ) -> Result<(), Error> {
        if self.cells[..self.cell_count]
            .iter()
            .any(|cell| cell.dirty != 0)
        {
            return Err(Error::PendingWrites);
        }
        let mut refreshed = Self::new(registers, upvalues, scratch)?;
        refreshed.counts = self.counts;
        *self = refreshed;
        Ok(())
    }
}

fn pending<'gc>(
    registers: &LuaRegisters<'gc, '_>,
    target: Option<Target<'gc>>,
    cell: Cell,
    scratch: &[Slot],
) -> Result<Value<'gc>, Error> {
    let reference = match target.ok_or(Error::ChangedTarget)? {
        Target::Closed(upvalue) => match upvalue.get() {
            UpValueState::Closed(value) => value,
            _ => return Err(Error::ChangedTarget),
        },
        Target::Upper(index) => registers
            .projection_read(true, index)
            .ok_or(Error::ChangedTarget)?,
        Target::Register(index) => registers
            .projection_read(false, index)
            .ok_or(Error::ChangedTarget)?,
    };
    let value = if cell.register == DETACHED {
        cell.value
    } else {
        *scratch
            .get(cell.register as usize)
            .ok_or(Error::InvalidSlot)?
    };
    if value.tag > abi::REFERENCE {
        return Err(Error::InvalidSlot);
    }
    Ok(value.value(reference))
}

const _: () = assert!(std::mem::size_of::<Cell>() == 32);
const _: () = assert!(std::mem::offset_of!(Cell, register) == 16);
const _: () = assert!(std::mem::offset_of!(Cell, dirty) == 24);
const _: () = assert!(std::mem::size_of::<Counts>() == 8);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RegisterIndex;

    #[test]
    fn duplicate_closed_cells_share_pending_values_and_keep_counts_on_refresh() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let cell = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(7)));
            let mut canonical = [Value::Nil];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                let mut scratch = [Slot::from_value(Value::Nil)];
                let mut projection =
                    Projection::<2>::new(&registers, &[cell, cell], &scratch).unwrap();
                assert_eq!(projection.cell_count, 1);
                assert_eq!(projection.bindings, [0, 0]);
                projection
                    .write(0, &mut scratch, Slot::from_value(Value::Integer(42)))
                    .unwrap();
                assert_eq!(projection.read(1, &scratch).unwrap().bits, 42);
                assert!(matches!(
                    cell.get(),
                    UpValueState::Closed(Value::Integer(7))
                ));
                assert_eq!(
                    projection.refresh(&registers, &[cell, cell], &scratch),
                    Err(Error::PendingWrites)
                );
                projection.flush(ctx, &mut registers, &scratch).unwrap();
                assert!(matches!(
                    cell.get(),
                    UpValueState::Closed(Value::Integer(42))
                ));
                cell.set(&ctx, UpValueState::Closed(Value::Integer(99)));
                projection
                    .refresh(&registers, &[cell, cell], &scratch)
                    .unwrap();
                assert_eq!(projection.read(0, &scratch).unwrap().bits, 99);
                assert_eq!(
                    projection.counts,
                    Counts {
                        reads: 2,
                        writes: 1
                    }
                );
            });
        });
    }

    #[test]
    fn current_frame_aliases_follow_scratch_and_materialize_before_observation() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let mut canonical = [Value::Integer(7), Value::Nil];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                let cell = registers.open_test_upvalue(&ctx, RegisterIndex(0));
                let mut scratch = [
                    Slot::from_value(Value::Integer(11)),
                    Slot::from_value(Value::Nil),
                ];
                let mut projection =
                    Projection::<2>::new(&registers, &[cell, cell], &scratch).unwrap();
                assert_eq!(projection.cell_count, 1);
                assert_eq!(projection.read(0, &scratch).unwrap().bits, 11);
                projection
                    .write(1, &mut scratch, Slot::from_value(Value::Integer(42)))
                    .unwrap();
                assert_eq!(scratch[0].bits, 42);
                assert!(matches!(registers.stack_frame[0], Value::Integer(7)));
                scratch[0] = Slot::from_value(Value::Integer(43));
                assert_eq!(projection.read(0, &scratch).unwrap().bits, 43);
                projection.flush(ctx, &mut registers, &scratch).unwrap();
                assert!(matches!(registers.stack_frame[0], Value::Integer(43)));
            });
        });
    }

    #[test]
    fn upper_stack_aliases_deduplicate_distinct_cells_at_the_same_location() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let mut canonical = [Value::Integer(7), Value::Integer(11), Value::Nil];
            let mut pc = 0;
            LuaRegisters::projection_split_frame(
                ctx,
                &mut pc,
                &mut canonical,
                2,
                |mut registers| {
                    let first = registers.projection_open_at(ctx, 1);
                    let second = registers.projection_open_at(ctx, 1);
                    assert!(!Gc::ptr_eq(first.into_inner(), second.into_inner()));
                    let mut scratch = [Slot::from_value(Value::Nil)];
                    let mut projection =
                        Projection::<2>::new(&registers, &[first, second], &scratch).unwrap();
                    assert_eq!(projection.cell_count, 1);
                    assert_eq!(projection.cells[0].register, DETACHED);
                    projection
                        .write(1, &mut scratch, Slot::from_value(Value::Integer(42)))
                        .unwrap();
                    assert_eq!(projection.read(0, &scratch).unwrap().bits, 42);
                    assert!(matches!(
                        registers.projection_read(true, 1),
                        Some(Value::Integer(11))
                    ));
                    projection.flush(ctx, &mut registers, &scratch).unwrap();
                    assert!(matches!(
                        registers.projection_read(true, 1),
                        Some(Value::Integer(42))
                    ));
                    assert_eq!(scratch[0].tag, abi::NIL);
                },
            );
        });
    }

    #[test]
    fn ordinary_scratch_writes_materialize_linked_cells_without_false_upvalue_counts() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let mut canonical = [Value::Integer(7)];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                let cell = registers.open_test_upvalue(&ctx, RegisterIndex(0));
                let mut scratch = [Slot::from_value(Value::Integer(7))];
                let mut projection = Projection::<1>::new(&registers, &[cell], &scratch).unwrap();
                scratch[0] = Slot::from_value(Value::Integer(42));
                assert_eq!(projection.cells[0].dirty, 0);
                projection.flush(ctx, &mut registers, &scratch).unwrap();
                assert!(matches!(registers.stack_frame[0], Value::Integer(42)));
                assert_eq!(projection.counts, Counts::default());
            });
        });
    }

    #[test]
    fn helper_rebinding_refreshes_groups_without_retaining_stale_cell_values() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let old = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(7)));
            let new = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(11)));
            let mut canonical = [Value::Nil];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                let mut scratch = [Slot::from_value(Value::Nil)];
                let mut projection =
                    Projection::<2>::new(&registers, &[old, old], &scratch).unwrap();
                projection
                    .write(0, &mut scratch, Slot::from_value(Value::Integer(42)))
                    .unwrap();
                projection.flush(ctx, &mut registers, &scratch).unwrap();
                projection
                    .refresh(&registers, &[new, old], &scratch)
                    .unwrap();
                assert_eq!(projection.bindings, [0, 1]);
                assert_eq!(projection.read(0, &scratch).unwrap().bits, 11);
                assert_eq!(projection.read(1, &scratch).unwrap().bits, 42);
                projection
                    .write(0, &mut scratch, Slot::from_value(Value::Integer(99)))
                    .unwrap();
                projection.flush(ctx, &mut registers, &scratch).unwrap();
                assert!(matches!(
                    old.get(),
                    UpValueState::Closed(Value::Integer(42))
                ));
                assert!(matches!(
                    new.get(),
                    UpValueState::Closed(Value::Integer(99))
                ));
                assert_eq!(
                    projection.counts,
                    Counts {
                        reads: 2,
                        writes: 2
                    }
                );
            });
        });
    }

    #[test]
    fn cells_outside_scratch_materialize_into_canonical_registers() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let mut canonical = [Value::Nil; 8];
            canonical[7] = Value::Integer(17);
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                let cell = registers.open_test_upvalue(&ctx, RegisterIndex(7));
                let mut scratch = [Slot::from_value(Value::Integer(11))];
                let mut projection = Projection::<1>::new(&registers, &[cell], &scratch).unwrap();
                assert_eq!(projection.cells[0].register, DETACHED);
                assert_eq!(projection.read(0, &scratch).unwrap().bits, 17);
                projection
                    .write(0, &mut scratch, Slot::from_value(Value::Integer(42)))
                    .unwrap();
                projection.flush(ctx, &mut registers, &scratch).unwrap();
                assert!(matches!(registers.stack_frame[7], Value::Integer(42)));
                assert_eq!(scratch[0].bits, 11);
            });
        });
    }

    #[test]
    fn foreign_stacks_remain_unresolved_without_borrowing_them() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let mut foreign = [Value::Integer(17)];
            let mut foreign_pc = 0;
            LuaRegisters::with_test_frame(
                ctx,
                &mut foreign_pc,
                &mut foreign,
                |mut foreign_registers| {
                    let cell = foreign_registers.open_test_upvalue(&ctx, RegisterIndex(0));
                    let mut canonical = [Value::Nil];
                    let mut pc = 0;
                    LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                        let mut scratch = [Slot::from_value(Value::Nil)];
                        let mut projection =
                            Projection::<1>::new(&registers, &[cell], &scratch).unwrap();
                        assert_eq!(projection.bindings[0], FALLBACK);
                        assert_eq!(projection.cell_count, 0);
                        assert!(projection.read(0, &scratch).is_none());
                        assert!(projection
                            .write(0, &mut scratch, Slot::from_value(Value::Integer(42)))
                            .is_none());
                        projection.flush(ctx, &mut registers, &scratch).unwrap();
                        assert_eq!(projection.counts, Counts::default());
                    });
                },
            );
        });
    }

    #[test]
    fn invalid_targets_and_slots_are_preflighted_before_any_commit() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let first = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(7)));
            let second = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(11)));
            let mut canonical = [Value::Nil];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                let mut scratch = [Slot::from_value(Value::Nil)];
                let mut projection =
                    Projection::<2>::new(&registers, &[first, second], &scratch).unwrap();
                for index in 0..2 {
                    projection
                        .write(index, &mut scratch, Slot::from_value(Value::Integer(42)))
                        .unwrap();
                }
                projection.cells[1].value.tag = u64::MAX;
                assert_eq!(
                    projection.flush(ctx, &mut registers, &scratch),
                    Err(Error::InvalidSlot)
                );
                assert!(matches!(
                    first.get(),
                    UpValueState::Closed(Value::Integer(7))
                ));
                assert!(matches!(
                    second.get(),
                    UpValueState::Closed(Value::Integer(11))
                ));
                assert_eq!(projection.cells[0].dirty, 1);
                projection.cells[1].value = Slot::from_value(Value::Integer(42));
                let open = registers.projection_open_at(ctx, 0);
                second.set(&ctx, open.get());
                assert_eq!(
                    projection.flush(ctx, &mut registers, &scratch),
                    Err(Error::ChangedTarget)
                );
                assert!(matches!(
                    first.get(),
                    UpValueState::Closed(Value::Integer(7))
                ));
                second.set(&ctx, UpValueState::Closed(Value::Integer(11)));
                projection.flush(ctx, &mut registers, &scratch).unwrap();
                assert!(matches!(
                    first.get(),
                    UpValueState::Closed(Value::Integer(42))
                ));
                assert!(matches!(
                    second.get(),
                    UpValueState::Closed(Value::Integer(42))
                ));
            });
        });
    }

    #[test]
    fn malformed_scalars_and_missing_bindings_have_no_effects() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let cell = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(7)));
            let mut canonical = [Value::Nil];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |registers| {
                let mut scratch = [Slot::from_value(Value::Nil)];
                let mut projection = Projection::<1>::new(&registers, &[cell], &scratch).unwrap();
                for value in [
                    Slot {
                        tag: abi::NIL,
                        bits: 1,
                    },
                    Slot {
                        tag: abi::BOOLEAN,
                        bits: 2,
                    },
                    Slot {
                        tag: abi::REFERENCE,
                        bits: 0,
                    },
                    Slot {
                        tag: u64::MAX,
                        bits: 0,
                    },
                ] {
                    assert!(projection.write(0, &mut scratch, value).is_none());
                }
                assert!(projection
                    .write(
                        usize::MAX,
                        &mut scratch,
                        Slot::from_value(Value::Integer(42))
                    )
                    .is_none());
                assert!(projection.read(usize::MAX, &scratch).is_none());
                assert_eq!(projection.counts, Counts::default());
                assert_eq!(projection.cells[0].dirty, 0);
                assert_eq!(projection.cells[0].value.bits, 7);
            });
        });
    }

    #[test]
    fn references_fallback_and_scalar_replacement_preserves_payload_bits() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let table = crate::Table::new(&ctx);
            let cell = UpValue::new(&ctx, UpValueState::Closed(Value::Table(table)));
            let mut canonical = [Value::Nil];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                let mut scratch = [Slot::from_value(Value::Nil)];
                let mut projection = Projection::<1>::new(&registers, &[cell], &scratch).unwrap();
                assert!(projection.read(0, &scratch).is_none());
                for bits in [0, (-0.0f64).to_bits(), 1, u64::MAX, 0x7ff8_0000_0000_1234] {
                    let value = Slot {
                        tag: abi::NUMBER,
                        bits,
                    };
                    projection.write(0, &mut scratch, value).unwrap();
                    assert_eq!(projection.read(0, &scratch).unwrap().bits, bits);
                    projection.flush(ctx, &mut registers, &scratch).unwrap();
                    let UpValueState::Closed(Value::Number(value)) = cell.get() else {
                        panic!("scalar write was not committed")
                    };
                    assert_eq!(value.to_bits(), bits);
                    projection.refresh(&registers, &[cell], &scratch).unwrap();
                    assert_eq!(projection.read(0, &scratch).unwrap().bits, bits);
                }
                cell.set(&ctx, UpValueState::Closed(Value::Table(table)));
                projection.refresh(&registers, &[cell], &scratch).unwrap();
                assert!(projection.read(0, &scratch).is_none());
                assert_eq!(
                    projection.counts,
                    Counts {
                        reads: 10,
                        writes: 5
                    }
                );
            });
        });
    }

    #[test]
    fn bounded_storage_admits_the_last_index_and_refuses_oversized_frames() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let cell = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(7)));
            let mut canonical = [Value::Nil];
            let mut pc = 0;
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |registers| {
                let scratch = [Slot::from_value(Value::Nil)];
                let mut projection =
                    Projection::<256>::new(&registers, &[cell; 256], &scratch).unwrap();
                assert_eq!(projection.read(255, &scratch).unwrap().bits, 7);
                assert_eq!(projection.cell_count, 1);
                assert!(matches!(
                    Projection::<1>::new(&registers, &[cell, cell], &scratch),
                    Err(Error::Capacity)
                ));
                assert!(matches!(
                    Projection::<257>::new(&registers, &[cell], &scratch),
                    Err(Error::Capacity)
                ));
                assert!(matches!(
                    Projection::<1>::new(&registers, &[cell], &[scratch[0]; 2]),
                    Err(Error::Capacity)
                ));
                assert!(Projection::<0>::new(&registers, &[], &scratch).is_ok());
                assert!(std::mem::size_of::<Projection<'_, 256>>() <= 256 * 64 + 32);
            });
        });
    }
}
