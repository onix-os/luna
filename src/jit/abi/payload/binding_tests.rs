use super::*;

fn reference(value: &mut Value<'_>) -> Payload {
    let (tag, pointer) = match value {
        Value::Integer(value) => (INTEGER, std::ptr::from_mut(value).cast()),
        Value::Number(value) => (NUMBER, std::ptr::from_mut(value).cast()),
        Value::Boolean(value) => (BOOLEAN, std::ptr::from_mut(value).cast()),
        Value::Nil => (NIL, std::ptr::null_mut()),
        _ => (REFERENCE, std::ptr::null_mut()),
    };
    Payload { tag, pointer }
}

#[test]
fn numeric_binding_matches_reference_for_every_value_kind_and_rebind() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        let kinds = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::INFINITY),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
            Value::String(ctx.intern(b"binding")),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(closure.into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 42)),
        ];
        for initial in kinds {
            let mut values = [initial, Value::Integer(1234)];
            let expected = reference(&mut values[0]);
            let bits = unsafe { load(expected) }.map(|slot| (slot.tag, slot.bits));
            let mut frame = Frame::new(&mut values);
            let actual = frame.bind(0).unwrap();
            assert_eq!(
                (actual.tag, actual.pointer),
                (expected.tag, expected.pointer)
            );
            assert_eq!(
                unsafe { load(actual) }.map(|slot| (slot.tag, slot.bits)),
                bits
            );
            assert!(frame.bind(2).is_none());
            assert!(frame.bind(usize::MAX).is_none());
            let mut slots = [actual, frame.bind(1).unwrap()];
            for next in kinds {
                let mut oracle = next;
                let expected = reference(&mut oracle);
                let bits = unsafe { load(expected) }.map(|slot| (slot.tag, slot.bits));
                assert!(frame.store(&mut slots, 0, next));
                let actual = slots[0];
                assert_eq!(actual.tag, expected.tag);
                assert_eq!(actual.pointer.is_null(), expected.pointer.is_null());
                assert_eq!(
                    unsafe { load(actual) }.map(|slot| (slot.tag, slot.bits)),
                    bits
                );
                if let Some(slot) = unsafe { load(actual) } {
                    assert!(unsafe { store(actual, slot) });
                }
                assert_eq!(unsafe { load(slots[1]) }.unwrap().bits, 1234);
            }
        }
        assert!(Frame::new(&mut []).bind(0).is_none());
    });
}
