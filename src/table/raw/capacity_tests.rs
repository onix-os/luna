use super::*;

fn original<'gc>(mc: &Mutation<'gc>, array_capacity: usize, map_capacity: usize) -> RawTable<'gc> {
    let mut array = vec::Vec::new_in(MetricsAlloc::new(mc));
    array.resize(array_capacity, Value::Nil);
    RawTable {
        array,
        map: HashMap::with_capacity_and_hasher_in(map_capacity, (), MetricsAlloc::new(mc)),
        order: vec::Vec::new_in(MetricsAlloc::new(mc)),
        weak_values: false,
        weak_keys: false,
    }
}

fn equal(left: Value<'_>, right: Value<'_>) {
    match (left, right) {
        (Value::Nil, Value::Nil) => {}
        (Value::Integer(left), Value::Integer(right)) => assert_eq!(left, right),
        _ => panic!("unexpected scalar pair: {left:?}, {right:?}"),
    }
}

fn compare<'gc>(mc: &Mutation<'gc>, left: &RawTable<'gc>, right: &RawTable<'gc>) {
    assert_eq!(left.array.len(), right.array.len());
    for (&left, &right) in left.array.iter().zip(&right.array) {
        equal(left, right);
    }
    assert_eq!(left.array.capacity(), right.array.capacity());
    assert_eq!(left.map.capacity(), right.map.capacity());
    assert_eq!(
        left.map.raw_table().buckets(),
        right.map.raw_table().buckets()
    );
    assert_eq!(left.map.len(), right.map.len());
    assert_eq!(left.order.len(), right.order.len());
    assert_eq!(left.order.capacity(), right.order.capacity());
    assert_eq!(left.weak_values, right.weak_values);
    assert_eq!(left.weak_keys, right.weak_keys);
    assert_eq!(left.length(mc), right.length(mc));
}

#[test]
fn constructors_preserve_capacity_accounting_and_growth() {
    crate::Lua::empty().enter(|ctx| {
        for array in [0, 1, 2, 4, 17] {
            for map in [0, 1, 2, 7, 17] {
                let metrics = ctx.metrics();
                let baseline = metrics.total_allocation();
                let mut expected = original(&ctx, array, map);
                let first = metrics.total_allocation();
                let mut actual = RawTable::with_capacity(&ctx, array, map);
                assert_eq!(metrics.total_allocation() - first, first - baseline);
                compare(&ctx, &expected, &actual);
                if array == 0 && map == 0 {
                    assert_eq!(first, baseline);
                }
                for weak in [false, true] {
                    if weak {
                        expected.make_keys_weak(&ctx);
                        actual.make_keys_weak(&ctx);
                        expected.make_values_weak(&ctx);
                        actual.make_values_weak(&ctx);
                    }
                    for i in 1..=40 {
                        for key in [
                            Value::Integer(i),
                            Value::String(ctx.intern(i.to_string().as_bytes())),
                        ] {
                            for value in [Value::Integer(i * 2), Value::Nil, Value::Integer(i)] {
                                let before = metrics.total_allocation();
                                let old = expected.set(&ctx, key, value).unwrap();
                                let middle = metrics.total_allocation();
                                equal(actual.set(&ctx, key, value).unwrap(), old);
                                assert_eq!(
                                    metrics.total_allocation() as i128 - middle as i128,
                                    middle as i128 - before as i128
                                );
                                equal(actual.get(&ctx, key), expected.get(&ctx, key));
                                compare(&ctx, &expected, &actual);
                            }
                        }
                    }
                }
                let before = metrics.total_allocation();
                drop(expected);
                let middle = metrics.total_allocation();
                drop(actual);
                assert_eq!(before - middle, middle - metrics.total_allocation());
            }
        }
    });
}
