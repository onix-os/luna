use super::*;
use std::panic::{catch_unwind, AssertUnwindSafe};

fn storage<'gc>(ctx: Context<'gc>, length: usize) -> Gc<'gc, RefLock<StackVec<'gc>>> {
    let mut values = StackVec::new_in(MetricsAlloc::new(&ctx));
    values.extend((0..length).map(|index| Value::Integer(index as i64)));
    Gc::new(&ctx, RefLock::new(values))
}

#[test]
fn scalar_consumption_removes_the_whole_suffix_only() {
    crate::Lua::empty().enter(|ctx| {
        for bottom in [0, 2, 7] {
            for length in [0, 1, 2, 64] {
                let values = storage(ctx, bottom + length);
                let mut stack = Stack::new(ctx, values, bottom);
                let capacity = stack.capacity();
                let result = stack.consume::<i64>(ctx);
                if length == 0 {
                    assert_eq!(result.unwrap_err().argument, 1);
                } else {
                    assert_eq!(result.unwrap(), bottom as i64);
                }
                assert_eq!(stack.capacity(), capacity);
                assert!(stack.is_empty());
                assert_eq!(values.borrow().len(), bottom);
                for index in 0..bottom {
                    assert!(matches!(values.borrow()[index], Value::Integer(value) if value == index as i64));
                }
                stack.push_back(Value::Boolean(true));
                assert!(matches!(stack.pop_back(), Some(Value::Boolean(true))));
            }
        }
    });
}

#[derive(Debug)]
struct Refuse<const READS: usize>;

impl<'gc, const READS: usize> FromMultiValue<'gc> for Refuse<READS> {
    fn from_multi_value(
        _: Context<'gc>,
        mut values: impl Iterator<Item = Value<'gc>>,
    ) -> Result<Self, TypeError> {
        for _ in 0..READS {
            values.next();
        }
        Err(TypeError {
            expected: "sentinel",
            found: "sentinel",
        })
    }
}

#[test]
fn conversion_errors_count_missing_arguments_and_clear_leftovers() {
    crate::Lua::empty().enter(|ctx| {
        let values = storage(ctx, 5);
        let mut stack = Stack::new(ctx, values, 2);
        let error = stack.consume::<Refuse<0>>(ctx).unwrap_err();
        assert_eq!(error.argument, 1);
        assert_eq!(error.source.expected, "sentinel");
        assert_eq!(values.borrow().len(), 2);
        stack.push_back(Value::Integer(9));
        assert_eq!(stack.consume::<Refuse<5>>(ctx).unwrap_err().argument, 5);
        assert!(stack.is_empty());
        stack.push_back(Value::Integer(9));
        assert_eq!(stack.consume::<(i64, i64)>(ctx).unwrap_err().argument, 2);
        stack.push_back(Value::Integer(9));
        stack.push_back(Value::Boolean(true));
        stack.push_back(Value::Integer(11));
        assert_eq!(stack.consume::<(i64, i64)>(ctx).unwrap_err().argument, 2);
        assert!(matches!(
            values.borrow().as_slice(),
            [Value::Integer(0), Value::Integer(1)]
        ));
    });
}

struct PanicAfterOne;

impl<'gc> FromMultiValue<'gc> for PanicAfterOne {
    fn from_multi_value(
        _: Context<'gc>,
        mut values: impl Iterator<Item = Value<'gc>>,
    ) -> Result<Self, TypeError> {
        assert!(matches!(values.next(), Some(Value::Integer(2))));
        std::panic::panic_any(0x1234_u64);
    }
}

#[test]
fn conversion_panic_clears_suffix_and_releases_the_lock() {
    crate::Lua::empty().enter(|ctx| {
        let values = storage(ctx, 5);
        let mut stack = Stack::new(ctx, values, 2);
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = stack.consume::<PanicAfterOne>(ctx);
        }))
        .unwrap_err();
        assert_eq!(*panic.downcast::<u64>().unwrap(), 0x1234);
        assert!(matches!(
            values.borrow().as_slice(),
            [Value::Integer(0), Value::Integer(1)]
        ));
        stack.push_back(Value::Integer(8));
        assert_eq!(stack.consume::<i64>(ctx).unwrap(), 8);
    });
}

struct LockProbe;

impl<'gc> FromMultiValue<'gc> for LockProbe {
    fn from_multi_value(
        _: Context<'gc>,
        mut values: impl Iterator<Item = Value<'gc>>,
    ) -> Result<Self, TypeError> {
        let Some(Value::UserData(root)) = values.next() else {
            panic!("missing stack root")
        };
        let stack = root
            .downcast::<ottavino_gc_arena::Rootable![Gc<'_, RefLock<StackVec<'_>>>]>()
            .unwrap();
        assert!(stack.try_borrow().is_err());
        Ok(Self)
    }
}

#[test]
fn conversion_keeps_the_stack_exclusively_borrowed() {
    crate::Lua::empty().enter(|ctx| {
        let values = storage(ctx, 2);
        let root = crate::UserData::new::<
            ottavino_gc_arena::Rootable![Gc<'_, RefLock<StackVec<'_>>>],
        >(&ctx, values);
        let mut stack = Stack::new(ctx, values, 2);
        stack.push_back(Value::UserData(root));
        stack.consume::<LockProbe>(ctx).unwrap();
        assert_eq!(values.borrow().len(), 2);
    });
}

#[test]
fn invalid_substack_panics_before_conversion_without_mutation() {
    crate::Lua::empty().enter(|ctx| {
        let values = storage(ctx, 3);
        let mut stack = Stack::new(ctx, values, 1);
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = stack.sub_stack(3).consume::<Refuse<0>>(ctx);
        }));
        assert!(panic.is_err());
        assert_eq!(values.borrow().len(), 3);
    });
}
