use super::*;
use ottavino_gc_arena::{lock::RefLock, Gc, Rootable};
use std::{cell::Cell as Counter, mem, rc::Rc};

pub(super) fn identical<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
    match (actual, expected) {
        (Value::Nil, Value::Nil) => {}
        (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
        (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
        (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
        (Value::String(a), Value::String(b)) => assert_eq!(a.as_ptr(), b.as_ptr()),
        (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
        (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
        (Value::Thread(a), Value::Thread(b)) => assert_eq!(a, b),
        (Value::UserData(a), Value::UserData(b)) => assert_eq!(a, b),
        _ => panic!("value kind changed"),
    }
}

pub(super) fn kinds(ctx: crate::Context<'_>) -> Vec<Value<'_>> {
    vec![
        Value::Nil,
        Value::Boolean(false),
        Value::Boolean(true),
        Value::Integer(i64::MIN),
        Value::Integer(i64::MAX),
        Value::Number(-0.0),
        Value::Number(f64::INFINITY),
        Value::Number(f64::from_bits(0x7ff8000000000042)),
        Value::String(ctx.intern(b"canonical cell")),
        Value::Table(Table::new(&ctx)),
        Value::Function(Closure::load(ctx, None, b"return 42").unwrap().into()),
        Value::Function(
            Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return)).into(),
        ),
        Value::Thread(Thread::new(ctx)),
        Value::UserData(UserData::new_static(&ctx, 42)),
    ]
}

#[test]
fn canonical_cell_layout_preserves_value_and_optional_value_size_bounds() {
    let bound = mem::size_of::<(usize, i64)>();
    assert_eq!(mem::size_of::<Kind>(), mem::size_of::<usize>());
    assert_eq!(mem::size_of::<Payload<'_>>(), 8);
    assert_eq!(mem::size_of::<Cell<'_>>(), bound);
    assert!(mem::size_of::<Option<Cell<'_>>>() <= bound);
    assert!(mem::size_of::<Cell<'_>>() <= mem::size_of::<Value<'_>>());
    assert_eq!(mem::offset_of!(Cell<'_>, kind), 0);
    assert_eq!(
        mem::offset_of!(Cell<'_>, payload),
        mem::align_of::<Payload<'_>>()
    );
    assert!(!mem::needs_drop::<Cell<'_>>());
    assert!(Cell::needs_trace());
}

#[test]
fn canonical_cells_roundtrip_all_typed_references_and_numeric_bits() {
    crate::Lua::empty().enter(|ctx| {
        for value in kinds(ctx) {
            let cell = Cell::new(value);
            identical(cell.value(), value);
            let expected = match value {
                Value::Nil => Some((0, 0)),
                Value::Boolean(v) => Some((1, u64::from(v))),
                Value::Integer(v) => Some((2, v as u64)),
                Value::Number(v) => Some((3, v.to_bits())),
                _ => None,
            };
            assert_eq!(cell.scalar(), expected);
        }
        let mut bits = 0xd1b54a32d192ed03u64;
        for _ in 0..4096 {
            bits ^= bits << 13;
            bits ^= bits >> 7;
            bits ^= bits << 17;
            for value in [
                Value::Integer(bits as i64),
                Value::Number(f64::from_bits(bits)),
            ] {
                identical(Cell::new(value).value(), value);
            }
        }
    });
}

#[test]
fn canonical_scalar_writes_preserve_refusal_and_reference_transitions() {
    crate::Lua::empty().enter(|ctx| {
        for initial in kinds(ctx) {
            for tag in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, usize::MAX] {
                for bits in [0, 1, 2, i64::MIN as u64, u64::MAX, 0x7ff8000000000042] {
                    let mut cell = Cell::new(initial);
                    let expected = match tag {
                        0 => Some(Value::Nil),
                        1 if bits <= 1 => Some(Value::Boolean(bits != 0)),
                        2 => Some(Value::Integer(bits as i64)),
                        3 => Some(Value::Number(f64::from_bits(bits))),
                        _ => None,
                    };
                    assert_eq!(cell.set_scalar(tag, bits), expected.is_some());
                    identical(cell.value(), expected.unwrap_or(initial));
                    cell = Cell::new(initial);
                    identical(cell.value(), initial);
                }
            }
        }
    });
}

#[cfg(target_pointer_width = "64")]
unsafe extern "C" fn scalar_entry(
    cells: *mut Cell<'_>,
    len: usize,
    index: usize,
    tag: usize,
    bits: u64,
) -> u32 {
    if index >= len || tag > 3 || (tag == 1 && bits > 1) {
        return 0;
    }
    unsafe {
        let cell = cells.add(index);
        std::ptr::addr_of_mut!((*cell).payload.bits).write(if tag == 0 { 0 } else { bits });
        std::ptr::addr_of_mut!((*cell).kind)
            .cast::<usize>()
            .write(tag);
    }
    1
}

#[test]
#[cfg(target_pointer_width = "64")]
fn canonical_raw_scalar_entry_writes_the_original_cells_without_binding() {
    check_scalar_entry(scalar_entry);
}

#[cfg(target_pointer_width = "64")]
pub(super) fn check_scalar_entry(
    entry: for<'gc> unsafe extern "C" fn(*mut Cell<'gc>, usize, usize, usize, u64) -> u32,
) {
    assert_eq!(unsafe { entry(std::ptr::null_mut(), 0, 0, 2, 0) }, 0);
    crate::Lua::empty().enter(|ctx| {
        for initial in kinds(ctx) {
            for index in [0, 1, 2, 3, usize::MAX] {
                for tag in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, usize::MAX] {
                    for bits in [0, 1, 2, i64::MIN as u64, u64::MAX, 0x7ff8000000000042] {
                        let mut cells = [Cell::new(initial); 3];
                        let mut expected = cells;
                        let accepted = expected
                            .get_mut(index)
                            .is_some_and(|cell| cell.set_scalar(tag, bits));
                        let address = cells.as_ptr();
                        let len = cells.len();
                        let base = cells.as_mut_ptr();
                        let result = unsafe { entry(base, len, index, tag, bits) };
                        assert_eq!(result, u32::from(accepted));
                        assert_eq!(address, cells.as_ptr());
                        for (actual, expected) in cells.into_iter().zip(expected) {
                            identical(actual.value(), expected.value());
                            assert_eq!(actual.scalar(), expected.scalar());
                        }
                    }
                }
            }
        }
    });
}

struct DropProbe(Rc<Counter<usize>>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

#[test]
fn canonical_cells_are_the_only_roots_and_release_overwritten_references() {
    only_roots(|cell, bits| cell.set_scalar(2, bits));
}

pub(super) fn only_roots(write: impl for<'gc> Fn(&mut Cell<'gc>, u64) -> bool) {
    let drops = Rc::new(Counter::new(0));
    let mut lua = crate::Lua::empty();
    let root = lua.enter(|ctx| {
        let probe = || UserData::new_static(&ctx, DropProbe(drops.clone()));
        let table = Table::new(&ctx);
        table.set_field(ctx, "marker", probe());
        let env = Table::new(&ctx);
        env.set_field(ctx, "marker", probe());
        let closure = Closure::load_with_env(ctx, None, b"return marker", env).unwrap();
        let callback = Callback::from_fn_with(&ctx, probe(), |_, _, _, _| {
            Ok(crate::CallbackReturn::Return)
        });
        let thread = Thread::new(ctx);
        let noop = Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return));
        thread.start(ctx, noop.into(), probe()).unwrap();
        let cells = vec![
            Cell::new(Value::String(String::from_slice(&ctx, b"only cell root"))),
            Cell::new(Value::Table(table)),
            Cell::new(Value::Function(closure.into())),
            Cell::new(Value::Function(callback.into())),
            Cell::new(Value::Thread(thread)),
            Cell::new(Value::UserData(probe())),
        ];
        let storage = Gc::new(&ctx, RefLock::new(cells));
        ctx.stash(UserData::new::<Rootable![Gc<'_, RefLock<Vec<Cell<'_>>>>]>(
            &ctx, storage,
        ))
    });
    for _ in 0..2 {
        lua.gc_collect();
    }
    assert_eq!(drops.get(), 0);
    lua.enter(|ctx| {
        let holder = ctx.fetch(&root);
        let cells = holder.downcast::<Rootable![Gc<'_, RefLock<Vec<Cell<'_>>>>]>().unwrap().borrow();
        assert!(matches!(cells[0].value(), Value::String(s) if s.as_bytes() == b"only cell root"));
        assert!(matches!(cells[1].value(), Value::Table(t) if matches!(t.get_value(ctx, "marker"), Value::UserData(_))));
        assert!(matches!(cells[2].value(), Value::Function(Function::Closure(c)) if !c.prototype().opcodes.is_empty()));
        assert!(matches!(cells[4].value(), Value::Thread(t) if t.mode() != crate::ThreadMode::Stopped));
        assert!(matches!(cells[5].value(), Value::UserData(u) if u.downcast_static::<DropProbe>().is_ok()));
    });
    for index in 1..6 {
        lua.enter(|ctx| {
            let holder = ctx.fetch(&root);
            let storage = holder
                .downcast::<Rootable![Gc<'_, RefLock<Vec<Cell<'_>>>>]>()
                .unwrap();
            assert!(write(&mut storage.borrow_mut(&ctx)[index], index as u64));
        });
        for _ in 0..2 {
            lua.gc_collect();
        }
        assert_eq!(drops.get(), index);
    }
}
