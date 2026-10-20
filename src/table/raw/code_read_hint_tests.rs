use super::*;
use crate::jit::entry_bytes::Hint;
use std::sync::atomic::{AtomicU8, Ordering};

fn cached<'gc>(
    table: &RawTable<'gc>,
    mc: &Mutation<'gc>,
    key: String<'gc>,
    hint: &AtomicU8,
) -> Value<'gc> {
    table.get_canonical_string(mc, key, Some(Hint::new(hint)))
}

fn same<'gc>(actual: Value<'gc>, expected: Value<'gc>) {
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
        _ => panic!("cached read mismatch: {actual:?}, {expected:?}"),
    }
}

#[test]
fn code_hint_checks_every_offset_and_current_table_key() {
    crate::Lua::empty().enter(|ctx| {
        let key = String::from_slice(&ctx, b"target");
        let equal = String::from_buffer(&ctx, b"target".to_vec().into_boxed_slice());
        let missing = ctx.intern(b"absent");
        let hint = AtomicU8::new(0);
        for count in [0, 1, 4, 16, 128, 300] {
            let mut table = RawTable::new(&ctx);
            for i in 0..count {
                table
                    .set(
                        &ctx,
                        ctx.intern(format!("key-{i}").as_bytes()).into(),
                        Value::Integer(i),
                    )
                    .unwrap();
            }
            table.set(&ctx, key.into(), Value::Integer(71)).unwrap();
            for offset in 0..=u8::MAX {
                for query in [key, equal, missing] {
                    hint.store(offset, Ordering::Relaxed);
                    same(
                        cached(&table, &ctx, query, &hint),
                        table.get(&ctx, query.into()),
                    );
                }
            }
            let mut peer = RawTable::new(&ctx);
            peer.set(&ctx, key.into(), Value::Integer(93)).unwrap();
            for current in [&table, &peer, &table, &peer] {
                same(
                    cached(current, &ctx, equal, &hint),
                    current.get(&ctx, equal.into()),
                );
            }
        }
    });
}

#[test]
fn code_hint_tracks_growth_removal_and_weak_transitions() {
    crate::Lua::empty().enter(|ctx| {
        let key = String::from_buffer(&ctx, std::vec![b'x'; 257].into_boxed_slice());
        let equal = String::from_slice(&ctx, key.as_bytes());
        let hint = AtomicU8::new(255);
        let callback = Callback::from_fn(&ctx, |_, _, _| panic!("lookup executed callback"));
        for mode in 0..4 {
            let mut table = RawTable::new(&ctx);
            if mode & 1 != 0 {
                table.make_keys_weak(&ctx);
            }
            if mode & 2 != 0 {
                table.make_values_weak(&ctx);
            }
            for value in [
                Value::Nil,
                Value::Boolean(false),
                Value::Integer(-7),
                Value::Number(-0.0),
                Value::Number(f64::from_bits(0x7ff8_0000_0000_1234)),
                Value::String(ctx.intern(b"value")),
                Value::Table(Table::new(&ctx)),
                callback.into(),
                UserData::new_static(&ctx, ()).into(),
                Thread::new(ctx).into(),
            ] {
                table.set(&ctx, key.into(), value).unwrap();
                for query in [key, equal] {
                    same(
                        cached(&table, &ctx, query, &hint),
                        table.get(&ctx, query.into()),
                    );
                }
            }
            for i in 0..300 {
                table
                    .set(
                        &ctx,
                        ctx.intern(format!("growth-{i}").as_bytes()).into(),
                        Value::Integer(i),
                    )
                    .unwrap();
                same(
                    cached(&table, &ctx, key, &hint),
                    table.get(&ctx, key.into()),
                );
            }
            table
                .map
                .raw_table_mut()
                .shrink_to(0, |(key, _)| key_hash(key));
            same(
                cached(&table, &ctx, key, &hint),
                table.get(&ctx, key.into()),
            );
            table.set(&ctx, key.into(), Value::Nil).unwrap();
            same(cached(&table, &ctx, key, &hint), Value::Nil);
            table.clear();
            same(cached(&table, &ctx, key, &hint), Value::Nil);
            assert_eq!(Hint::new(&hint).load(), None);
        }
    });
}

#[test]
fn code_hint_never_roots_weak_values_across_collection() {
    for mode in ["k", "v", "kv"] {
        let mut lua = crate::Lua::empty();
        let hint = AtomicU8::new(0);
        let root = lua.enter(|ctx| {
            let key = ctx.intern(b"entry");
            let table = Table::new(&ctx);
            let meta = Table::new(&ctx);
            meta.set_field(ctx, "__mode", mode);
            table.set_metatable(ctx, Some(meta));
            table.set(ctx, key, Table::new(&ctx)).unwrap();
            let inner = table.into_inner().borrow();
            same(
                cached(&inner.raw_table, &ctx, key, &hint),
                table.get_raw(&ctx, key.into()),
            );
            ctx.stash(table)
        });
        lua.gc_collect();
        lua.gc_collect();
        lua.enter(|ctx| {
            let table = ctx.fetch(&root);
            let key = String::from_slice(&ctx, b"entry");
            let inner = table.into_inner().borrow();
            let actual = cached(&inner.raw_table, &ctx, key, &hint);
            same(actual, table.get_raw(&ctx, key.into()));
            assert_eq!(actual.is_nil(), mode != "k");
        });
    }
}

#[test]
fn code_hint_can_cross_arenas_without_retaining_gc_provenance() {
    let hint = AtomicU8::new(255);
    for state in 0..16 {
        crate::Lua::empty().enter(|ctx| {
            let mut table = RawTable::new(&ctx);
            let key = String::from_slice(&ctx, b"shared-key");
            same(cached(&table, &ctx, key, &hint), Value::Nil);
            table.set(&ctx, key.into(), Value::Integer(state)).unwrap();
            same(cached(&table, &ctx, key, &hint), Value::Integer(state));
        });
    }
}

#[test]
fn canonical_reads_without_hints_match_long_keys_and_large_maps() {
    crate::Lua::empty().enter(|ctx| {
        for count in [0, 128, 300] {
            for length in [1, 257, 4096] {
                let mut table = RawTable::new(&ctx);
                for i in 0..count {
                    table
                        .set(
                            &ctx,
                            ctx.intern(format!("filler-{i}").as_bytes()).into(),
                            Value::Integer(i),
                        )
                        .unwrap();
                }
                let key = String::from_buffer(&ctx, std::vec![b'x'; length].into_boxed_slice());
                let equal = String::from_buffer(&ctx, std::vec![b'x'; length].into_boxed_slice());
                let hint = AtomicU8::new(255);
                for value in [
                    Value::Nil,
                    Value::Integer(-91),
                    Value::Integer(42),
                    Value::Nil,
                ] {
                    table.set(&ctx, key.into(), value).unwrap();
                    for query in [key, equal] {
                        same(
                            table.get_canonical_string(&ctx, query, None),
                            table.get(&ctx, query.into()),
                        );
                        same(
                            cached(&table, &ctx, query, &hint),
                            table.get(&ctx, query.into()),
                        );
                    }
                }
            }
        }
    });
}
