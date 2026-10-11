use super::*;

fn check(mut entry: impl FnMut(*const Packed, usize, bool) -> u64, index: usize, tag: bool) {
    crate::Lua::empty().enter(|ctx| {
        for initial in kinds(ctx) {
            let expected = Slot::from_value(initial);
            let mut values = vec![Value::Integer(731); index + 1];
            values[index] = initial;
            with_scope::<256, _>(&mut values, index + 1, |scope| unsafe {
                let slots = scope.descriptors.as_ptr();
                assert_eq!(
                    entry(slots, index, tag),
                    if tag { expected.tag } else { expected.bits }
                );
                if matches!(expected.tag, INTEGER | NUMBER) {
                    for bits in [
                        0,
                        1,
                        u64::MAX,
                        i64::MIN as u64,
                        0x7ff8000000000042,
                        0xfff0000000000042,
                    ] {
                        assert!(slots.add(index).read().store_numeric(Slot {
                            tag: expected.tag,
                            bits
                        }));
                        assert_eq!(
                            entry(slots, index, tag),
                            if tag { expected.tag } else { bits }
                        );
                    }
                }
            })
            .unwrap();
            for value in &values[..index] {
                assert!(matches!(value, Value::Integer(731)));
            }
        }
    });
    for encoded in [0, 1, 6, 7, usize::MAX - 1, usize::MAX] {
        let mut descriptors = vec![Packed(std::ptr::without_provenance_mut(OPAQUE)); index + 1];
        descriptors[index] = Packed(std::ptr::without_provenance_mut(encoded));
        assert_eq!(
            entry(descriptors.as_ptr(), index, tag),
            if tag { u64::MAX } else { 0 }
        );
    }
    if tag {
        let mut descriptors = vec![Packed(std::ptr::without_provenance_mut(OPAQUE)); index + 1];
        let encodings = (0..4096usize)
            .chain((3..usize::BITS).flat_map(|bit| (0..8).map(move |low| (1usize << bit) | low)))
            .chain((0..4096usize).map(|offset| usize::MAX - offset));
        for encoded in encodings {
            let descriptor = Packed(std::ptr::without_provenance_mut(encoded));
            descriptors[index] = descriptor;
            assert_eq!(
                entry(descriptors.as_ptr(), index, true),
                descriptor.tag().unwrap_or(u64::MAX),
                "encoded={encoded:#x}"
            );
        }
    }
}

#[cfg(not(miri))]
pub(crate) fn check_native_read(
    entry: unsafe extern "C" fn(*const Packed) -> u64,
    index: usize,
    tag: bool,
) {
    check(|slots, _, _| unsafe { entry(slots) }, index, tag);
}

#[test]
fn packed_read_oracle_checks_initialized_prefixes_mutations_and_invalid_immediates() {
    for index in [0, 1, 255] {
        for tag in [false, true] {
            check(
                |slots, index, tag| unsafe {
                    let descriptor = slots.add(index).read();
                    if tag {
                        descriptor.tag().unwrap_or(u64::MAX)
                    } else {
                        descriptor.load().map_or(0, |slot| slot.bits)
                    }
                },
                index,
                tag,
            );
        }
    }
}
