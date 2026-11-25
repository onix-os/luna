use super::*;
use std::{
    cell::RefCell,
    panic::{catch_unwind, AssertUnwindSafe},
    rc::Rc,
};

fn storage<'gc>(ctx: Context<'gc>, length: usize) -> Gc<'gc, RefLock<StackVec<'gc>>> {
    let mut values = StackVec::new_in(MetricsAlloc::new(&ctx));
    values.extend((0..length).map(|index| Value::Integer(index as i64)));
    Gc::new(&ctx, RefLock::new(values))
}

fn replace<'gc>(
    stack: &mut Stack<'gc, '_>,
    ctx: Context<'gc>,
    values: impl IntoMultiValue<'gc>,
    reference: bool,
) {
    if reference {
        stack.clear();
        stack.into_back(ctx, values);
    } else {
        stack.replace(ctx, values);
    }
}

fn identical<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
    match (actual, expected) {
        (Value::Nil, Value::Nil) => {}
        (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
        (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
        (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
        (Value::String(a), Value::String(b)) => assert!(Gc::ptr_eq(a.into_inner(), b.into_inner())),
        (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
        (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
        (Value::Thread(a), Value::Thread(b)) => assert_eq!(a, b),
        (Value::UserData(a), Value::UserData(b)) => assert_eq!(a, b),
        (a, b) => panic!("value mismatch: {a:?} != {b:?}"),
    }
}

#[test]
fn replacements_match_clear_then_append_for_all_value_kinds() {
    crate::Lua::empty().enter(|ctx| {
        let values = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8_0000_0000_1234)),
            Value::String(crate::String::from_slice(&ctx, b"replacement")),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(crate::Closure::load(ctx, None, b"return").unwrap().into()),
            Value::Function(
                crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return)).into(),
            ),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 42i64)),
        ];
        for value in values {
            for bottom in [0, 2, 7] {
                for before in [0, 1, 3, 17] {
                    for after in [0, 1, 2, 17] {
                        let expected = storage(ctx, bottom + before);
                        let actual = storage(ctx, bottom + before);
                        for (data, reference) in [(expected, true), (actual, false)] {
                            replace(
                                &mut Stack::new(ctx, data, bottom),
                                ctx,
                                crate::Variadic(std::iter::repeat_n(value, after)),
                                reference,
                            );
                        }
                        let (expected, actual) = (expected.borrow(), actual.borrow());
                        assert_eq!(actual.len(), bottom + after);
                        assert_eq!(actual.capacity(), expected.capacity());
                        for (a, b) in actual.iter().zip(expected.iter()) {
                            identical(*a, *b);
                        }
                        for (index, prefix) in actual[..bottom].iter().enumerate() {
                            identical(*prefix, Value::Integer(index as i64));
                        }
                    }
                }
            }
        }
    });
}

type Events = Rc<RefCell<std::vec::Vec<&'static str>>>;

struct Probe<'gc> {
    storage: Gc<'gc, RefLock<StackVec<'gc>>>,
    events: Events,
    failure: usize,
}

struct ProbeIter<'gc> {
    probe: Probe<'gc>,
    next: usize,
}

impl<'gc> IntoMultiValue<'gc> for Probe<'gc> {
    fn into_multi_value(self, _: Context<'gc>) -> impl Iterator<Item = Value<'gc>> {
        self.events.borrow_mut().push("convert");
        assert!(self.storage.try_borrow().is_err());
        if self.failure == 1 {
            std::panic::panic_any(1usize);
        }
        ProbeIter {
            probe: self,
            next: 0,
        }
    }
}

impl<'gc> Iterator for ProbeIter<'gc> {
    type Item = Value<'gc>;

    fn next(&mut self) -> Option<Self::Item> {
        self.probe.events.borrow_mut().push("next");
        assert!(self.probe.storage.try_borrow().is_err());
        if (self.probe.failure == 2 && self.next == 0)
            || (self.probe.failure == 3 && self.next == 1)
        {
            std::panic::panic_any(self.probe.failure);
        }
        if self.next == 2 {
            return None;
        }
        self.next += 1;
        Some(Value::Integer(100 + self.next as i64))
    }
}

impl Drop for ProbeIter<'_> {
    fn drop(&mut self) {
        self.probe.events.borrow_mut().push("drop");
        assert!(self.probe.storage.try_borrow().is_err());
        if self.probe.failure == 4 {
            std::panic::panic_any(4usize);
        }
    }
}

#[test]
fn conversion_iteration_and_drop_keep_the_original_borrow_and_panic_order() {
    crate::Lua::empty().enter(|ctx| {
        for failure in 0..=4 {
            let mut outcomes = std::vec::Vec::new();
            for reference in [true, false] {
                let storage = storage(ctx, 5);
                let events = Events::default();
                let mut stack = Stack::new(ctx, storage, 2);
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    replace(
                        &mut stack,
                        ctx,
                        Probe {
                            storage,
                            events: events.clone(),
                            failure,
                        },
                        reference,
                    );
                }));
                let panic = outcome
                    .err()
                    .map(|error| *error.downcast::<usize>().unwrap());
                let retained = storage.borrow().to_vec();
                let expected = match failure {
                    1 | 2 => 0,
                    3 => 1,
                    _ => 2,
                };
                assert_eq!(retained.len(), 2 + expected);
                identical(retained[0], Value::Integer(0));
                identical(retained[1], Value::Integer(1));
                assert_eq!(panic, (failure != 0).then_some(failure));
                stack.push_back(Value::Boolean(true));
                assert!(matches!(stack.pop_back(), Some(Value::Boolean(true))));
                outcomes.push((panic, retained, events.borrow().clone()));
            }
            assert_eq!(outcomes[0].0, outcomes[1].0);
            assert_eq!(outcomes[0].2, outcomes[1].2);
            assert_eq!(outcomes[0].1.len(), outcomes[1].1.len());
            for (a, b) in outcomes[0].1.iter().zip(&outcomes[1].1) {
                identical(*a, *b);
            }
        }
    });
}

#[test]
fn existing_borrows_refuse_before_clearing_or_conversion() {
    crate::Lua::empty().enter(|ctx| {
        for reference in [true, false] {
            for exclusive in [true, false] {
                let storage = storage(ctx, 5);
                let events = Events::default();
                let mut stack = Stack::new(ctx, storage, 2);
                let mut attempt = || {
                    catch_unwind(AssertUnwindSafe(|| {
                        replace(
                            &mut stack,
                            ctx,
                            Probe {
                                storage,
                                events: events.clone(),
                                failure: 0,
                            },
                            reference,
                        );
                    }))
                };
                let outcome = if exclusive {
                    let _guard = storage.borrow_mut(&ctx);
                    attempt()
                } else {
                    let _guard = storage.borrow();
                    attempt()
                };
                assert!(outcome.is_err());
                assert!(events.borrow().is_empty());
                assert_eq!(storage.borrow().len(), 5);
                for (index, value) in storage.borrow().iter().enumerate() {
                    identical(*value, Value::Integer(index as i64));
                }
            }
        }
    });
}

#[test]
fn replacement_beyond_the_current_length_keeps_truncation_semantics() {
    crate::Lua::empty().enter(|ctx| {
        for offset in [0, 2, 3, 7] {
            for reference in [true, false] {
                let storage = storage(ctx, 3);
                let mut stack = Stack::new(ctx, storage, 1);
                replace(&mut stack.sub_stack(offset), ctx, 42i64, reference);
                let prefix = (1 + offset).min(3);
                assert_eq!(storage.borrow().len(), prefix + 1);
                identical(storage.borrow()[prefix], Value::Integer(42));
            }
        }
    });
}

struct CrossStack<'gc> {
    current: Gc<'gc, RefLock<StackVec<'gc>>>,
    other: Gc<'gc, RefLock<StackVec<'gc>>>,
    value: Value<'gc>,
}

impl<'gc> IntoMultiValue<'gc> for CrossStack<'gc> {
    fn into_multi_value(self, ctx: Context<'gc>) -> impl Iterator<Item = Value<'gc>> {
        assert!(self.current.try_borrow().is_err());
        self.other.borrow_mut(&ctx).push(self.value);
        std::iter::once(self.value)
    }
}

#[test]
fn conversion_can_mutate_an_independent_stack() {
    crate::Lua::empty().enter(|ctx| {
        for reference in [true, false] {
            let current = storage(ctx, 3);
            let other = storage(ctx, 1);
            let value = Value::Table(crate::Table::new(&ctx));
            replace(
                &mut Stack::new(ctx, current, 1),
                ctx,
                CrossStack {
                    current,
                    other,
                    value,
                },
                reference,
            );
            for values in [current, other] {
                let values = values.borrow();
                assert_eq!(values.len(), 2);
                identical(values[0], Value::Integer(0));
                identical(values[1], value);
            }
        }
    });
}
