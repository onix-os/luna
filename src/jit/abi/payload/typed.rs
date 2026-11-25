use super::*;

#[path = "typed/plan.rs"]
mod plan;

fn assert_same<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
    match (actual, expected) {
        (Value::Nil, Value::Nil) => {}
        (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
        (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
        (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
        (Value::String(a), Value::String(b)) => assert_eq!(a, b),
        (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
        (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
        (Value::Thread(a), Value::Thread(b)) => assert_eq!(a, b),
        (Value::UserData(a), Value::UserData(b)) => assert_eq!(a, b),
        _ => panic!("value kind changed: {actual:?} != {expected:?}"),
    }
}

struct Integers<'a, 'gc, const N: usize> {
    frame: Frame<'a, 'gc>,
    indices: [usize; N],
    pointers: [*mut i64; N],
}

impl<'a, 'gc, const N: usize> Integers<'a, 'gc, N> {
    fn bind(values: &'a mut [Value<'gc>], indices: [usize; N]) -> Option<Self> {
        if N > 256 || values.len() > 256 {
            return None;
        }
        for (position, index) in indices.iter().copied().enumerate() {
            if index >= values.len() || indices[..position].contains(&index) {
                return None;
            }
        }
        let mut frame = Frame::new(values);
        let mut pointers = [std::ptr::null_mut(); N];
        for (index, pointer) in indices.iter().copied().zip(&mut pointers) {
            let descriptor = frame.bind(index)?;
            if descriptor.tag != INTEGER || descriptor.pointer.is_null() {
                return None;
            }
            *pointer = descriptor.pointer.cast();
        }
        Some(Self {
            frame,
            indices,
            pointers,
        })
    }

    fn read(&self, slot: usize) -> Option<i64> {
        let pointer = *self.pointers.get(slot)?;
        if pointer.is_null() {
            return None;
        }
        Some(unsafe { pointer.read() })
    }

    fn write(&mut self, slot: usize, value: i64) -> bool {
        let Some(&pointer) = self.pointers.get(slot) else {
            return false;
        };
        if pointer.is_null() {
            return false;
        }
        unsafe { pointer.write(value) };
        true
    }

    fn replace(&mut self, index: usize, value: Value<'gc>) -> bool {
        if index >= self.frame.len {
            return false;
        }
        for (selected, pointer) in self.indices.iter().zip(&mut self.pointers) {
            if *selected == index {
                *pointer = std::ptr::null_mut();
            }
        }
        unsafe { self.frame.values.as_ptr().add(index).write(value) };
        true
    }

    fn refresh(&mut self, slot: usize) -> bool {
        let Some(pointer) = self.pointers.get_mut(slot) else {
            return false;
        };
        *pointer = std::ptr::null_mut();
        let descriptor = self.frame.bind(self.indices[slot]).unwrap();
        if descriptor.tag != INTEGER || descriptor.pointer.is_null() {
            return false;
        }
        *pointer = descriptor.pointer.cast();
        true
    }
}

#[test]
fn typed_selection_refuses_nonintegers_duplicates_and_invalid_spans_without_writes() {
    crate::Lua::empty().enter(|ctx| {
        for value in [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Number(-0.0),
            Value::Number(f64::NAN),
            Value::String(ctx.intern(b"typed")),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(crate::Closure::load(ctx, None, b"return 1").unwrap().into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 7)),
        ] {
            for target in 0..7 {
                let mut values = [Value::Integer(31); 7];
                values[target] = value;
                let before = values.map(|value| {
                    let s = Slot::from_value(value);
                    (s.tag, s.bits)
                });
                assert!(Integers::bind(&mut values, [0, 1, 2, 3, 4, 5, 6]).is_none());
                assert_eq!(
                    values.map(|value| {
                        let s = Slot::from_value(value);
                        (s.tag, s.bits)
                    }),
                    before
                );
                assert_same(values[target], value);
            }
        }
        for indices in [[0, 0], [0, 7], [usize::MAX, 1], [1, usize::MAX]] {
            let mut values = [Value::Integer(17); 7];
            assert!(Integers::bind(&mut values, indices).is_none());
            assert!(values
                .iter()
                .all(|value| matches!(value, Value::Integer(17))));
        }
        assert!(Integers::bind(&mut [], []).is_some());
        assert!(Integers::bind(&mut [], [0]).is_none());
        assert!(Integers::bind(&mut [Value::Integer(0); 257], []).is_none());
        assert!(Integers::bind(&mut [Value::Integer(0); 256], [0; 257]).is_none());
    });
}

#[test]
fn typed_payload_siblings_survive_per_cell_replacement_and_refresh() {
    fn exercise<const N: usize>() {
        let mut values = [Value::Integer(7); N];
        let indices: [usize; N] = std::array::from_fn(|index| N - index - 1);
        let mut bound = Integers::bind(&mut values, indices).unwrap();
        for slot in 0..N {
            let sibling = (N > 1).then(|| bound.pointers[(slot + 1) % N]);
            assert!(bound.write(slot, i64::MIN.wrapping_add(slot as i64)));
            assert_eq!(bound.read(slot), Some(i64::MIN.wrapping_add(slot as i64)));
            assert!(bound.replace(indices[slot], Value::Number(-0.0)));
            assert_eq!(bound.read(slot), None);
            assert!(!bound.write(slot, 99));
            assert!(!bound.refresh(slot));
            if let Some(sibling) = sibling {
                let expected = bound.read((slot + 1) % N).unwrap();
                assert_eq!(unsafe { sibling.read() }, expected);
            }
            assert!(bound.replace(indices[slot], Value::Integer(slot as i64)));
            assert_eq!(bound.read(slot), None);
            assert!(bound.refresh(slot));
            assert_eq!(bound.read(slot), Some(slot as i64));
        }
        assert_eq!(bound.read(N), None);
        assert!(!bound.write(N, 4));
        assert!(!bound.refresh(N));
        assert!(!bound.replace(N, Value::Integer(9)));
        drop(bound);
        for (index, value) in values.into_iter().enumerate() {
            assert_same(value, Value::Integer((N - index - 1) as i64));
        }
    }
    exercise::<0>();
    exercise::<1>();
    exercise::<7>();
    exercise::<255>();
    exercise::<256>();
}

#[test]
fn typed_writes_are_canonical_on_unwind_and_admission_is_fresh() {
    let mut values = [Value::Integer(0), Value::Nil, Value::Integer(2)];
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut bound = Integers::bind(&mut values, [2, 0]).unwrap();
        assert!(bound.write(0, i64::MAX));
        assert!(bound.write(1, i64::MIN));
        assert!(bound.replace(1, Value::Boolean(true)));
        std::panic::panic_any("typed canonical unwind");
    }));
    assert_eq!(
        result.unwrap_err().downcast_ref::<&str>(),
        Some(&"typed canonical unwind")
    );
    assert_same(values[0], Value::Integer(i64::MIN));
    assert_same(values[1], Value::Boolean(true));
    assert_same(values[2], Value::Integer(i64::MAX));
    values[2] = Value::Nil;
    assert!(Integers::bind(&mut values, [2, 0]).is_none());
    values[2] = Value::Integer(42);
    let bound = Integers::bind(&mut values, [2, 0]).unwrap();
    assert_eq!(bound.read(0), Some(42));
    assert_eq!(bound.read(1), Some(i64::MIN));
}

#[test]
fn sparse_typed_bindings_preserve_unselected_references_and_raw_siblings() {
    crate::Lua::empty().enter(|ctx| {
        let table = Value::Table(crate::Table::new(&ctx));
        let mut values = [table, Value::Integer(1), Value::Nil, Value::Integer(3)];
        let mut bound = Integers::bind(&mut values, [3, 1]).unwrap();
        let retained = bound.pointers;
        assert!(bound.replace(2, table));
        assert_eq!(unsafe { retained[0].read() }, 3);
        assert_eq!(unsafe { retained[1].read() }, 1);
        assert!(bound.replace(3, Value::Integer(9)));
        assert_eq!(bound.read(0), None);
        assert_eq!(unsafe { retained[1].read() }, 1);
        assert!(bound.refresh(0));
        assert_eq!(bound.read(0), Some(9));
        drop(bound);
        assert_same(values[0], table);
        assert_same(values[2], table);
        assert_same(values[1], Value::Integer(1));
        assert_same(values[3], Value::Integer(9));
    });
}

type Write = for<'gc> unsafe extern "C" fn(*mut Value<'gc>, *mut *mut i64, i64);
type Probe = for<'gc> unsafe extern "C" fn(
    *const *mut i64,
    *mut Value<'gc>,
    *mut *mut i64,
    Write,
    u32,
) -> u32;

unsafe extern "C" fn terminal_write<'gc>(
    destination: *mut Value<'gc>,
    retired: *mut *mut i64,
    value: i64,
) {
    if !retired.is_null() {
        unsafe { retired.write(std::ptr::null_mut()) };
    }
    unsafe { destination.write(Value::Integer(value)) };
}

unsafe extern "C" fn rust_probe<'gc>(
    pointers: *const *mut i64,
    destination: *mut Value<'gc>,
    retired: *mut *mut i64,
    write: Write,
    budget: u32,
) -> u32 {
    if budget == 0 {
        return 0;
    }
    let first = unsafe { pointers.read() };
    let second = unsafe { pointers.add(1).read() };
    let sum = unsafe { first.read().wrapping_add(second.read()) };
    unsafe { first.write(sum) };
    if budget == 1 {
        return 1;
    }
    unsafe { write(destination, retired, sum) };
    2
}

fn check_terminal_probe(probe: Probe) {
    for a in [i64::MIN, -1, 0, 1, i64::MAX] {
        for b in [i64::MIN, -1, 0, 1, i64::MAX] {
            for target in 0..4 {
                for budget in [0, 1, 2, 64, u32::MAX] {
                    let initial = [
                        Value::Nil,
                        Value::Integer(b),
                        Value::Boolean(true),
                        Value::Integer(a),
                    ];
                    let mut expected = initial;
                    let sum = a.wrapping_add(b);
                    if budget > 0 {
                        expected[3] = Value::Integer(sum);
                    }
                    if budget > 1 {
                        expected[target] = Value::Integer(sum);
                    }
                    let mut values = initial;
                    let mut bound = Integers::bind(&mut values, [3, 1]).unwrap();
                    let base = bound.pointers.as_mut_ptr();
                    let target_slot = bound.indices.iter().position(|&index| index == target);
                    let retired =
                        target_slot.map_or(std::ptr::null_mut(), |slot| unsafe { base.add(slot) });
                    let destination = unsafe { bound.frame.values.as_ptr().add(target) };
                    let completed =
                        unsafe { probe(base, destination, retired, terminal_write, budget) };
                    assert_eq!(completed, budget.min(2));
                    for slot in 0..2 {
                        if budget > 1 && target_slot == Some(slot) {
                            assert_eq!(bound.read(slot), None);
                            assert!(!bound.write(slot, 999));
                            assert!(bound.refresh(slot));
                        }
                        let Value::Integer(expected) = expected[bound.indices[slot]] else {
                            unreachable!()
                        };
                        assert_eq!(bound.read(slot), Some(expected));
                    }
                    drop(bound);
                    for (actual, expected) in values.into_iter().zip(expected) {
                        assert_same(actual, expected);
                    }
                }
            }
        }
    }
}

#[test]
fn terminal_scalar_stores_retire_only_their_destination_at_exact_budgets() {
    check_terminal_probe(rust_probe);
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[path = "typed/native.rs"]
mod native;
