use super::*;
use std::hint::black_box;

#[derive(Clone, Copy)]
struct PreparedHash(u64);

impl PreparedHash {
    fn new(key: String<'_>) -> Self {
        Self(hasher().hash_one(CanonicalKey::String(key)))
    }
}

#[inline(never)]
fn prepared_read<'gc, const TYPED: bool>(
    table: &RawTable<'gc>,
    mc: &Mutation<'gc>,
    key: String<'gc>,
    hash: PreparedHash,
) -> Value<'gc> {
    table
        .map
        .raw_entry()
        .from_hash(hash.0, |stored| {
            if TYPED {
                stored.eq_string(key)
            } else {
                stored.eq(CanonicalKey::String(key))
            }
        })
        .map_or(Value::Nil, |(_, slot)| slot.0.get(mc))
}

fn compare<'gc>(table: &RawTable<'gc>, mc: &Mutation<'gc>, key: String<'gc>, hash: PreparedHash) {
    let expected = table.get(mc, key.into());
    for actual in [
        table.get_prepared_string(mc, key, PreparedStringHash::from_bits(hash.0)),
        prepared_read::<false>(table, mc, key, hash),
        prepared_read::<true>(table, mc, key, hash),
    ] {
        match (actual, expected) {
            (Value::Nil, Value::Nil) => {}
            (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
            (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
            (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
            (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
            _ => panic!("prepared read mismatch: {actual:?} != {expected:?}"),
        }
    }
}

#[test]
fn prepared_hash_follows_equal_keys_mutations_and_weak_modes() {
    crate::Lua::empty().enter(|ctx| {
        for bytes in [
            &b""[..],
            &b"callback"[..],
            &b"\0\xffkey"[..],
            &[b'x'; 257][..],
        ] {
            let keys = [
                String::from_slice(&ctx, bytes),
                String::from_buffer(&ctx, bytes.into()),
            ];
            assert!(!Gc::ptr_eq(keys[0].into_inner(), keys[1].into_inner()));
            let hash = PreparedHash::new(keys[0]);
            assert_eq!(hash.0, PreparedHash::new(keys[1]).0);
            for mode in 0..4 {
                let mut table = RawTable::new(&ctx);
                if mode & 1 != 0 {
                    table.make_keys_weak(&ctx);
                }
                if mode & 2 != 0 {
                    table.make_values_weak(&ctx);
                }
                for key in keys {
                    compare(&table, &ctx, key, hash);
                }
                for i in 0..128 {
                    table
                        .set(
                            &ctx,
                            ctx.intern(format!("growth-{i}").as_bytes()).into(),
                            Value::Integer(i),
                        )
                        .unwrap();
                }
                for value in [
                    Value::Boolean(false),
                    Value::Integer(17),
                    Value::Number(-0.0),
                    Value::Number(f64::from_bits(0x7ff8_0000_0000_1234)),
                    Value::Table(Table::new(&ctx)),
                    Value::Nil,
                ] {
                    table.set(&ctx, keys[1].into(), value).unwrap();
                    for key in keys {
                        compare(&table, &ctx, key, hash);
                    }
                }
                table.reserve_map(table.map.capacity() + 1);
                table.set(&ctx, keys[0].into(), Value::Integer(42)).unwrap();
                for key in keys {
                    compare(&table, &ctx, key, hash);
                }
                table.clear();
                for key in keys {
                    compare(&table, &ctx, key, hash);
                }
            }
        }
    });
}

#[test]
fn prepared_collision_probes_still_check_key_equality() {
    crate::Lua::empty().enter(|ctx| {
        let mut table = RawTable::new(&ctx);
        let keys = [
            ctx.intern(b"first"),
            ctx.intern(b"second"),
            ctx.intern(b"absent"),
        ];
        let hash = PreparedHash::new(keys[0]);
        for (i, key) in keys[..2].iter().copied().enumerate() {
            table.map.raw_table_mut().insert(
                hash.0,
                (
                    Key::Live(CanonicalKey::String(key)),
                    (Slot::Strong(Value::Integer(i as i64)), i),
                ),
                |_| hash.0,
            );
        }
        for mode in 0..3 {
            for (i, key) in keys.into_iter().enumerate() {
                let actual = match mode {
                    0 => prepared_read::<false>(&table, &ctx, key, hash),
                    1 => prepared_read::<true>(&table, &ctx, key, hash),
                    _ => {
                        table.get_prepared_string(&ctx, key, PreparedStringHash::from_bits(hash.0))
                    }
                };
                if i == 2 {
                    assert!(actual.is_nil());
                } else {
                    assert!(matches!(actual, Value::Integer(value) if value == i as i64));
                }
            }
        }
    });
}

#[test]
fn prepared_collision_checks_weak_keys_and_distinct_equal_strings() {
    crate::Lua::empty().enter(|ctx| {
        let keys = [ctx.intern(b"present"), ctx.intern(b"absent")];
        let hash = PreparedHash::new(keys[0]);
        for weak in [false, true] {
            let mut table = RawTable::new(&ctx);
            for (index, key) in [CanonicalKey::Integer(17), CanonicalKey::String(keys[0])]
                .into_iter()
                .enumerate()
            {
                let key = if weak {
                    Key::Weak(weaken(key), hash.0)
                } else {
                    Key::Live(key)
                };
                table.map.raw_table_mut().insert(
                    hash.0,
                    (key, (Slot::Strong(Value::Integer(index as i64)), index)),
                    |_| hash.0,
                );
            }
            for (index, key) in keys.into_iter().enumerate() {
                let distinct = String::from_buffer(&ctx, key.as_bytes().into());
                assert!(!Gc::ptr_eq(key.into_inner(), distinct.into_inner()));
                for query in [key, distinct] {
                    let value = table.get_prepared_string(
                        &ctx,
                        query,
                        PreparedStringHash::from_bits(hash.0),
                    );
                    if index == 0 {
                        assert!(matches!(value, Value::Integer(1)));
                    } else {
                        assert!(value.is_nil());
                    }
                }
            }
        }
    });
}

#[test]
fn prepared_hash_does_not_keep_weak_values_alive() {
    fn static_word<T: Send + Sync + 'static>() {}
    static_word::<PreparedHash>();
    assert_eq!(std::mem::size_of::<PreparedHash>(), 8);
    for mode in ["k", "v", "kv"] {
        let mut lua = crate::Lua::empty();
        let (root, hash) = lua.enter(|ctx| {
            let key = ctx.intern(b"entry");
            let hash = PreparedHash::new(key);
            let table = Table::new(&ctx);
            let meta = Table::new(&ctx);
            meta.set_field(ctx, "__mode", mode);
            table.set_metatable(ctx, Some(meta));
            table.set(ctx, key, Table::new(&ctx)).unwrap();
            compare(&table.into_inner().borrow().raw_table, &ctx, key, hash);
            (ctx.stash(table), hash)
        });
        lua.gc_collect();
        lua.gc_collect();
        lua.enter(|ctx| {
            let table = ctx.fetch(&root);
            let key = String::from_slice(&ctx, b"entry");
            let inner = table.into_inner().borrow();
            compare(&inner.raw_table, &ctx, key, hash);
            let actual = prepared_read::<false>(&inner.raw_table, &ctx, key, hash);
            if mode == "k" {
                assert!(matches!(actual, Value::Table(_)));
            } else {
                assert!(actual.is_nil());
            }
        });
    }
}

#[test]
fn prepared_comparator_distinguishes_every_stored_key_variant() {
    use crate::string::key_hash_tests::collision_string;

    crate::Lua::empty().enter(|ctx| {
        let query = collision_string(&ctx, b"query", 7);
        let equal = collision_string(&ctx, b"query", 7);
        let collision = collision_string(&ctx, b"other", 7);
        assert!(!Gc::ptr_eq(query.into_inner(), equal.into_inner()));
        let keys = [
            (CanonicalKey::Boolean(false), false),
            (CanonicalKey::Boolean(true), false),
            (CanonicalKey::Integer(7), false),
            (CanonicalKey::Number(0.5f64.to_bits()), false),
            (CanonicalKey::String(query), true),
            (CanonicalKey::String(equal), true),
            (CanonicalKey::String(collision), false),
            (CanonicalKey::Table(Table::new(&ctx)), false),
            (
                CanonicalKey::Closure(Closure::load(ctx, None, b"return 1").unwrap()),
                false,
            ),
            (
                CanonicalKey::Callback(Callback::from_fn(&ctx, |_, _, _| {
                    Ok(crate::CallbackReturn::Return)
                })),
                false,
            ),
            (CanonicalKey::Thread(Thread::new(ctx)), false),
            (CanonicalKey::UserData(UserData::new_static(&ctx, 7)), false),
        ];
        for (key, expected) in keys {
            for stored in [Key::Live(key), Key::Weak(weaken(key), 7)] {
                for query in [query, equal] {
                    assert_eq!(stored.eq_string(query), expected, "{stored:?}");
                }
            }
        }
        let dead = Key::Dead(Gc::as_ptr(query.into_inner()).cast(), 7);
        assert!(!dead.eq_string(query));
        assert!(!dead.eq_string(equal));
    });
}

#[test]
fn prepared_stored_hash_collisions_preserve_content_equality() {
    use crate::string::key_hash_tests::collision_string;

    fn assert_expected(actual: Value<'_>, expected: Value<'_>) {
        match (actual, expected) {
            (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
            (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
            (Value::Nil, Value::Nil) => {}
            _ => panic!("collision mismatch: {actual:?} != {expected:?}"),
        }
    }

    crate::Lua::empty().enter(|ctx| {
        for stored_hash in [0, 1, u64::MAX] {
            for mode in 0..4 {
                let mut table = RawTable::new(&ctx);
                if mode & 1 != 0 {
                    table.make_keys_weak(&ctx);
                }
                if mode & 2 != 0 {
                    table.make_values_weak(&ctx);
                }
                let first = collision_string(&ctx, b"present", stored_hash);
                let second = collision_string(&ctx, b"another", stored_hash);
                let hash = PreparedHash::new(first);
                assert_eq!(hash.0, PreparedHash::new(second).0);
                table.set(&ctx, first.into(), Value::Integer(17)).unwrap();
                table
                    .set(&ctx, second.into(), Value::Boolean(false))
                    .unwrap();
                for key in [first, second] {
                    compare(&table, &ctx, key, hash);
                }
                for (bytes, expected) in [
                    (&b"present"[..], Value::Integer(17)),
                    (&b"another"[..], Value::Boolean(false)),
                    (&b"absent!"[..], Value::Nil),
                ] {
                    let query = collision_string(&ctx, bytes, stored_hash);
                    assert!(!Gc::ptr_eq(query.into_inner(), first.into_inner()));
                    assert!(!Gc::ptr_eq(query.into_inner(), second.into_inner()));
                    assert_eq!(query.stored_hash(), first.stored_hash());
                    compare(&table, &ctx, query, hash);
                    assert_expected(
                        table.get_prepared_string(
                            &ctx,
                            query,
                            PreparedStringHash::from_bits(hash.0),
                        ),
                        expected,
                    );
                }
                table.set(&ctx, first.into(), Value::Nil).unwrap();
                for (bytes, expected) in [
                    (&b"present"[..], Value::Nil),
                    (&b"another"[..], Value::Boolean(false)),
                ] {
                    let query = collision_string(&ctx, bytes, stored_hash);
                    compare(&table, &ctx, query, hash);
                    assert_expected(
                        table.get_prepared_string(
                            &ctx,
                            query,
                            PreparedStringHash::from_bits(hash.0),
                        ),
                        expected,
                    );
                }
            }
        }
    });
}

#[inline(never)]
fn profile_reads<'gc, const MODE: usize>(
    table: &RawTable<'gc>,
    mc: &Mutation<'gc>,
    key: String<'gc>,
    hash: PreparedHash,
) -> i64 {
    let mut sum = 0;
    for _ in 0..100_000 {
        let table = black_box(table);
        let key = black_box(key);
        let value = match MODE {
            0 => table.get(mc, key.into()),
            1 => prepared_read::<false>(table, mc, key, PreparedHash::new(key)),
            2 => prepared_read::<false>(table, mc, key, black_box(hash)),
            3 => prepared_read::<true>(table, mc, key, black_box(hash)),
            _ => unreachable!(),
        };
        let Value::Integer(value) = black_box(value) else {
            panic!("expected integer")
        };
        sum += value;
    }
    sum
}

#[test]
#[ignore = "lookup-only instruction diagnostic, not native performance acceptance"]
fn prepared_lookup_profile() {
    let mode: usize = std::env::var("LUNA_PREHASH_MODE").unwrap().parse().unwrap();
    let separate = std::env::var("LUNA_PREHASH_SEPARATE").unwrap() == "1";
    crate::Lua::empty().enter(|ctx| {
        let mut table = RawTable::new(&ctx);
        let key = ctx.intern(b"callback");
        let query = if separate {
            String::from_slice(&ctx, b"callback")
        } else {
            key
        };
        table.set(&ctx, key.into(), Value::Integer(17)).unwrap();
        let hash = PreparedHash::new(query);
        let sum = match mode {
            0 => profile_reads::<0>(&table, &ctx, query, hash),
            1 => profile_reads::<1>(&table, &ctx, query, hash),
            2 => profile_reads::<2>(&table, &ctx, query, hash),
            3 => profile_reads::<3>(&table, &ctx, query, hash),
            _ => panic!("invalid lookup mode"),
        };
        assert_eq!(sum, 1_700_000);
        println!("mode={mode} separate={separate} reads=100000 sum={sum}");
    });
}
