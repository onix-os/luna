use super::*;

#[test]
fn mixed_keys_survive_rehash_weakening_deletion_and_clear() {
    crate::Lua::empty().enter(|ctx| {
        let mut keys = std::vec::Vec::new();
        for index in 0..96 {
            let bytes = format!("key-{index:03}-{}", "x".repeat(index % 33));
            keys.push(Value::String(String::from_slice(&ctx, bytes.as_bytes())));
        }
        keys.extend([
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(0),
            Value::Integer(i64::MAX),
            Value::Number(1.5),
            Value::Number(f64::INFINITY),
            Value::Number(f64::NEG_INFINITY),
            Value::Table(Table::new(&ctx)),
            Value::Function(Closure::load(ctx, None, b"return 1").unwrap().into()),
            Value::Function(
                Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return)).into(),
            ),
            Value::Thread(Thread::new(ctx)),
            Value::UserData(UserData::new_static(&ctx, 17)),
        ]);
        for weak_keys in [false, true] {
            for weak_values in [false, true] {
                let mut table = RawTable::new(&ctx);
                for round in 0..2 {
                    for (index, key) in keys.iter().copied().enumerate() {
                        assert!(table
                            .set(&ctx, key, Value::Integer(index as i64))
                            .unwrap()
                            .is_nil());
                    }
                    if weak_keys {
                        table.make_keys_weak(&ctx);
                    }
                    if weak_values {
                        table.make_values_weak(&ctx);
                    }
                    let capacity = table.map.capacity();
                    table.reserve_map(capacity + 1);
                    assert!(table.map.capacity() > capacity);
                    let mut previous = Value::Nil;
                    for (index, key) in keys.iter().copied().enumerate() {
                        let query = match key {
                            Value::String(value) => {
                                Value::String(String::from_buffer(&ctx, value.as_bytes().into()))
                            }
                            value => value,
                        };
                        identical(table.get(&ctx, query), Value::Integer(index as i64));
                        match table.next(&ctx, previous) {
                            NextValue::Found { key: found, value } => {
                                assert_eq!(
                                    CanonicalKey::new(found).unwrap(),
                                    CanonicalKey::new(key).unwrap()
                                );
                                identical(value, Value::Integer(index as i64));
                                previous = found;
                            }
                            other => panic!("missing insertion-order entry: {other:?}"),
                        }
                    }
                    assert!(matches!(table.next(&ctx, previous), NextValue::Last));
                    for (index, key) in keys.iter().copied().enumerate().step_by(2) {
                        identical(
                            table.set(&ctx, key, Value::Nil).unwrap(),
                            Value::Integer(index as i64),
                        );
                    }
                    table.reserve_map(table.map.capacity() + 1);
                    for (index, key) in keys.iter().copied().enumerate() {
                        let expected = if index % 2 == 0 {
                            Value::Nil
                        } else {
                            Value::Integer(index as i64)
                        };
                        identical(table.get(&ctx, key), expected);
                    }
                    table.clear();
                    assert!(matches!(table.next(&ctx, Value::Nil), NextValue::Last));
                    for key in &keys {
                        assert!(table.get(&ctx, *key).is_nil(), "round {round}");
                    }
                }
            }
        }
    });
}

#[test]
fn string_probes_preserve_content_equality_across_allocations() {
    crate::Lua::empty().enter(|ctx| {
        for bytes in [&b""[..], &b"callback"[..], &[b'x'; 300][..]] {
            let first = String::from_slice(&ctx, bytes);
            let separate = String::from_slice(&ctx, bytes);
            let owned = String::from_buffer(&ctx, bytes.into());
            assert!(!Gc::ptr_eq(first.into_inner(), separate.into_inner()));
            let different = String::from_slice(&ctx, b"different");
            for weak in [false, true] {
                let mut table = RawTable::new(&ctx);
                table.set(&ctx, first.into(), Value::Integer(7)).unwrap();
                if weak {
                    table.make_keys_weak(&ctx);
                }
                for query in [first, separate, owned] {
                    identical(table.get(&ctx, query.into()), Value::Integer(7));
                }
                assert!(table.get(&ctx, different.into()).is_nil());
                identical(
                    table.set(&ctx, separate.into(), Value::Nil).unwrap(),
                    Value::Integer(7),
                );
                for query in [first, separate, owned] {
                    assert!(table.get(&ctx, query.into()).is_nil());
                }
                table.set(&ctx, owned.into(), Value::Integer(9)).unwrap();
                for query in [first, separate, owned] {
                    identical(table.get(&ctx, query.into()), Value::Integer(9));
                }
                assert_eq!(table.map.len(), 1);
            }
        }
    });
}

fn identical<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
    match (actual, expected) {
        (Value::Nil, Value::Nil) => {}
        (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
        (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
        (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
        (Value::String(a), Value::String(b)) => {
            assert!(Gc::ptr_eq(a.into_inner(), b.into_inner()));
        }
        (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
        (actual, expected) => panic!("value mismatch: {actual:?} != {expected:?}"),
    }
}

#[test]
fn array_and_map_access_preserve_keys_values_and_previous_values() {
    crate::Lua::empty().enter(|ctx| {
        let mut array = RawTable::with_capacity(&ctx, 16, 128);
        let mut map = RawTable::with_capacity(&ctx, 0, 128);
        let marker = Value::Table(Table::new(&ctx));
        let string = Value::String(String::from_static(&ctx, b"marker"));
        let keys = [
            Value::Integer(i64::MIN),
            Value::Integer(-1),
            Value::Integer(0),
            Value::Number(-0.0),
            Value::Number(0.0),
            Value::Integer(1),
            Value::Number(1.0),
            Value::Number(1.5),
            Value::Integer(8),
            Value::Number(8.0),
            Value::Integer(16),
            Value::Integer(17),
            Value::Integer(1 << 32),
            Value::Integer(i64::MAX),
            Value::Number(f64::INFINITY),
            Value::Number(f64::NEG_INFINITY),
            Value::Boolean(false),
            marker,
            string,
        ];
        let values = [
            Value::Integer(i64::MIN),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8_0000_0000_1234)),
            marker,
            string,
            Value::Boolean(true),
            Value::Nil,
            Value::Integer(i64::MAX),
        ];
        for (round, value) in values.into_iter().enumerate() {
            for offset in 0..keys.len() {
                let key = keys[(offset + round) % keys.len()];
                identical(
                    array.set(&ctx, key, value).unwrap(),
                    map.set(&ctx, key, value).unwrap(),
                );
                for query in keys {
                    identical(array.get(&ctx, query), map.get(&ctx, query));
                }
            }
            assert_eq!(array.array.len(), 16);
            assert!(map.array.is_empty());
        }
        assert!(matches!(array.get(&ctx, Value::Nil), Value::Nil));
        assert!(matches!(
            array.get(&ctx, Value::Number(f64::NAN)),
            Value::Nil
        ));
        for table in [&mut array, &mut map] {
            assert!(matches!(
                table.set(&ctx, Value::Nil, marker),
                Err(InvalidTableKey::IsNil)
            ));
            assert!(matches!(
                table.set(&ctx, Value::Number(f64::NAN), marker),
                Err(InvalidTableKey::IsNaN)
            ));
        }
        for query in keys {
            identical(array.get(&ctx, query), map.get(&ctx, query));
        }
    });
}

#[test]
fn array_growth_keeps_map_entries_and_numeric_aliases() {
    crate::Lua::empty().enter(|ctx| {
        let mut table = RawTable::new(&ctx);
        for index in (1..=128).rev() {
            assert!(table
                .set(&ctx, Value::Number(index as f64), Value::Integer(index * 3))
                .unwrap()
                .is_nil());
        }
        table.grow_array(128);
        for index in 1..=128 {
            identical(
                table.get(&ctx, Value::Integer(index)),
                Value::Integer(index * 3),
            );
            identical(
                table.set(&ctx, Value::Integer(index), Value::Nil).unwrap(),
                Value::Integer(index * 3),
            );
            assert!(table.get(&ctx, Value::Number(index as f64)).is_nil());
        }
    });
}

#[test]
fn weak_array_holes_fall_back_to_live_map_values() {
    let mut lua = crate::Lua::empty();
    let (table, live) = lua.enter(|ctx| {
        let table = Table::new(&ctx);
        let meta = Table::new(&ctx);
        meta.set_field(ctx, "__mode", "v");
        table.set_metatable(ctx, Some(meta));
        let live = Table::new(&ctx);
        table.set(ctx, 1, live).unwrap();
        table.set(ctx, 3, Table::new(&ctx)).unwrap();
        {
            let mut inner = table.into_inner().borrow_mut(&ctx);
            inner.raw_table.grow_array(4);
            inner.raw_table.array_mut()[1] = Value::Table(Table::new(&ctx));
        }
        (ctx.stash(table), ctx.stash(live))
    });
    lua.gc_collect();
    lua.gc_collect();
    lua.enter(|ctx| {
        let table = ctx.fetch(&table);
        identical(table.get_value(ctx, 1), Value::Table(ctx.fetch(&live)));
        let strong = table.get_value(ctx, 2);
        assert!(matches!(strong, Value::Table(_)));
        assert!(table.get_value(ctx, 3).is_nil());
        identical(table.set(ctx, 2, Value::Nil).unwrap(), strong);
        assert!(table.set(ctx, 2, Table::new(&ctx)).unwrap().is_nil());
    });
    lua.gc_collect();
    lua.gc_collect();
    lua.enter(|ctx| {
        let table = ctx.fetch(&table);
        identical(table.get_value(ctx, 1), Value::Table(ctx.fetch(&live)));
        assert!(table.get_value(ctx, 2).is_nil());
        assert!(table.get_value(ctx, 3).is_nil());
    });
}

#[test]
fn repeated_string_reads_follow_map_mutations() {
    fn repeated_read<'gc>(
        mc: &Mutation<'gc>,
        table: &RawTable<'gc>,
        keys: [String<'gc>; 5],
        expected: Value<'gc>,
    ) {
        for key in keys {
            identical(table.get(mc, key.into()), expected);
        }
    }
    crate::Lua::empty().enter(|ctx| {
        let key = String::from_slice(&ctx, b"entry");
        let equal = String::from_buffer(&ctx, b"entry".to_vec().into_boxed_slice());
        let other = String::from_slice(&ctx, b"other");
        for mode in 0..3 {
            let mut table = RawTable::new(&ctx);
            let queries = [key, key, equal, equal, key];
            for round in 0..3 {
                let value = Value::Number(f64::from_bits(0x8000_0000_0000_0000 + round));
                table.set(&ctx, key.into(), value).unwrap();
                repeated_read(&ctx, &table, queries, value);
                table.set(&ctx, other.into(), Value::Integer(99)).unwrap();
                repeated_read(&ctx, &table, queries, value);
                for index in 1..=64 {
                    table
                        .set(&ctx, Value::Integer(index), Value::Integer(index))
                        .unwrap();
                    repeated_read(&ctx, &table, queries, value);
                }
                table.grow_array(128);
                repeated_read(&ctx, &table, queries, value);
                table.reserve_map(table.map.capacity() + 1);
                repeated_read(&ctx, &table, queries, value);
                identical(table.set(&ctx, equal.into(), Value::Nil).unwrap(), value);
                repeated_read(&ctx, &table, queries, Value::Nil);
                table.reserve_map(table.map.capacity() + 1);
                repeated_read(&ctx, &table, queries, Value::Nil);
                table.set(&ctx, equal.into(), Value::Integer(7)).unwrap();
                repeated_read(&ctx, &table, queries, Value::Integer(7));
                if mode == 1 {
                    table.make_values_weak(&ctx);
                } else if mode == 2 {
                    table.make_keys_weak(&ctx);
                }
                repeated_read(&ctx, &table, queries, Value::Integer(7));
                table.clear();
                repeated_read(&ctx, &table, queries, Value::Nil);
            }
        }
    });
}

#[test]
fn repeated_reads_do_not_root_weak_values_after_conversion() {
    for mode in ["v", "k", "kv"] {
        let mut lua = crate::Lua::empty();
        let (table, key) = lua.enter(|ctx| {
            let table = Table::new(&ctx);
            let key = String::from_slice(&ctx, b"entry");
            let value = Table::new(&ctx);
            table.set(ctx, key, value).unwrap();
            for _ in 0..4 {
                identical(table.get_value(ctx, key), Value::Table(value));
            }
            let meta = Table::new(&ctx);
            meta.set_field(ctx, "__mode", mode);
            table.set_metatable(ctx, Some(meta));
            for _ in 0..4 {
                identical(table.get_value(ctx, key), Value::Table(value));
            }
            (ctx.stash(table), ctx.stash(key))
        });
        lua.gc_collect();
        lua.gc_collect();
        lua.enter(|ctx| {
            let table = ctx.fetch(&table);
            let key = ctx.fetch(&key);
            for _ in 0..4 {
                let value = table.get_value(ctx, key);
                if mode == "k" {
                    assert!(matches!(value, Value::Table(_)));
                } else {
                    assert!(value.is_nil());
                }
            }
        });
        drop(key);
        lua.gc_collect();
        lua.gc_collect();
        lua.enter(|ctx| {
            let table = ctx.fetch(&table);
            if mode == "k" {
                assert!(matches!(
                    table.next(&ctx, Value::Nil),
                    NextValue::Found { .. }
                ));
            } else {
                assert!(matches!(table.next(&ctx, Value::Nil), NextValue::Last));
            }
        });
    }
}

#[test]
fn arbitrary_bucket_hints_match_uncached_lookup() {
    fn check<'gc>(mc: &Mutation<'gc>, table: &RawTable<'gc>, queries: &[Value<'gc>]) {
        for query in queries.iter().copied() {
            let expected = CanonicalKey::new(query)
                .ok()
                .and_then(|key| {
                    table
                        .map
                        .raw_entry()
                        .from_hash(hasher().hash_one(key), |k| k.eq(key))
                })
                .map_or(Value::Nil, |(_, value)| value.0.get(mc));
            for index in (0..=table.map.raw_table().buckets()).chain([usize::MAX]) {
                table
                    .string_bucket
                    .set(u32::try_from(index).unwrap_or(u32::MAX));
                identical(table.get(mc, query), expected);
            }
        }
    }
    crate::Lua::empty().enter(|ctx| {
        let key = String::from_slice(&ctx, b"entry");
        let equal = String::from_buffer(&ctx, b"entry".to_vec().into_boxed_slice());
        let other = String::from_slice(&ctx, b"other");
        let queries = [
            key.into(),
            equal.into(),
            other.into(),
            Value::Boolean(true),
            Value::Nil,
        ];
        for mode in 0..3 {
            let mut table = RawTable::new(&ctx);
            check(&ctx, &table, &queries);
            for round in 0..3 {
                let marker = Value::Table(Table::new(&ctx));
                for (key, value) in [
                    (key.into(), marker),
                    (other.into(), Value::Number(-0.0)),
                    (Value::Boolean(true), Value::Integer(round)),
                ] {
                    table.set(&ctx, key, value).unwrap();
                    check(&ctx, &table, &queries);
                }
                table.reserve_map(table.map.capacity() + 1);
                check(&ctx, &table, &queries);
                table.set(&ctx, equal.into(), Value::Nil).unwrap();
                check(&ctx, &table, &queries);
                table.grow_array(8);
                check(&ctx, &table, &queries);
                table.set(&ctx, equal.into(), marker).unwrap();
                check(&ctx, &table, &queries);
                if mode == 1 {
                    table.make_values_weak(&ctx);
                } else if mode == 2 {
                    table.make_keys_weak(&ctx);
                }
                check(&ctx, &table, &queries);
                table.clear();
                check(&ctx, &table, &queries);
            }
        }
    });
}

#[cfg(target_pointer_width = "64")]
#[test]
fn string_hint_preserves_uncached_table_layout_and_gc_charge() {
    use ottavino_gc_arena::lock::RefLock;

    #[derive(Collect)]
    #[collect(no_drop)]
    struct WithoutHint<'gc> {
        array: vec::Vec<Value<'gc>, MetricsAlloc<'gc>>,
        map: HashMap<Key<'gc>, (Slot<'gc>, usize), (), MetricsAlloc<'gc>>,
        order: vec::Vec<Key<'gc>, MetricsAlloc<'gc>>,
        weak_values: bool,
        weak_keys: bool,
    }

    #[derive(Collect)]
    #[collect(no_drop)]
    struct WithoutHintState<'gc> {
        raw_table: WithoutHint<'gc>,
        metatable: Option<Table<'gc>>,
        readonly: bool,
        intercept_all_writes: bool,
    }

    type Original = WithoutHint<'static>;
    type Current = RawTable<'static>;
    assert_eq!(mem::size_of::<Original>(), mem::size_of::<Current>());
    assert_eq!(mem::align_of::<Original>(), mem::align_of::<Current>());
    assert_eq!(
        mem::offset_of!(Original, array),
        mem::offset_of!(Current, array)
    );
    assert_eq!(
        mem::offset_of!(Original, map),
        mem::offset_of!(Current, map)
    );
    assert_eq!(
        mem::offset_of!(Original, order),
        mem::offset_of!(Current, order)
    );
    assert_eq!(
        mem::size_of::<WithoutHintState<'static>>(),
        mem::size_of::<crate::table::TableState<'static>>()
    );
    crate::Lua::empty().enter(|ctx| {
        let original = WithoutHintState {
            raw_table: WithoutHint {
                array: vec::Vec::new_in(MetricsAlloc::new(&ctx)),
                map: HashMap::with_hasher_in((), MetricsAlloc::new(&ctx)),
                order: vec::Vec::new_in(MetricsAlloc::new(&ctx)),
                weak_values: false,
                weak_keys: false,
            },
            metatable: None,
            readonly: false,
            intercept_all_writes: false,
        };
        let before = ctx.metrics().total_allocation();
        let original = Gc::new(&ctx, RefLock::new(original));
        let original_charge = ctx.metrics().total_allocation() - before;
        let before = ctx.metrics().total_allocation();
        let current = Table::new(&ctx);
        let current_charge = ctx.metrics().total_allocation() - before;
        assert!(original_charge > 0);
        assert_eq!(original_charge, current_charge);
        assert!(!Gc::as_ptr(original).is_null());
        assert!(!Gc::as_ptr(current.into_inner()).is_null());
    });
}
