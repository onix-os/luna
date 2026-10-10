use super::*;
use std::hash::Hasher;

#[derive(Default)]
struct Writes(Vec<Vec<u8>>);

impl Hasher for Writes {
    fn finish(&self) -> u64 {
        0
    }
    fn write(&mut self, bytes: &[u8]) {
        self.0.push(bytes.to_vec());
    }
}

fn writes(value: impl Hash) -> Vec<Vec<u8>> {
    let mut recorder = Writes::default();
    value.hash(&mut recorder);
    recorder.0
}

#[test]
fn string_hash_word_preserves_derived_discriminator_and_seeded_hash() {
    crate::Lua::empty().enter(|ctx| {
        for len in [
            0, 1, 7, 8, 15, 16, 31, 32, 63, 64, 127, 128, 255, 256, 257, 1024,
        ] {
            let bytes: Vec<_> = (0..len).map(|i| (i * 37) as u8).collect();
            let values = [
                String::from_slice(&ctx, &bytes),
                String::from_buffer(&ctx, bytes.clone().into_boxed_slice()),
                ctx.intern(&bytes),
            ];
            for (storage, value) in values.into_iter().enumerate() {
                let key = CanonicalKey::String(value);
                let word = CanonicalKey::<'static, u64>::String(value.stored_hash());
                assert_eq!(writes(key), writes(word), "length {len}");
                assert_eq!(hasher().hash_one(key), hasher().hash_one(word));
                assert_eq!(canonical_hash(key), hasher().hash_one(key));
                #[cfg(target_pointer_width = "64")]
                assert_eq!(
                    value.table_key_hash(),
                    (storage != 1 && len <= 256).then(|| hasher().hash_one(key))
                );
                #[cfg(not(target_pointer_width = "64"))]
                let _ = storage;
                for seed in [0, 1, u64::MAX] {
                    let state = ahash::RandomState::with_seeds(
                        seed,
                        !seed,
                        seed.wrapping_add(7),
                        seed.rotate_left(17),
                    );
                    assert_eq!(state.hash_one(key), state.hash_one(word));
                }
            }
        }
        for value in [
            String::from_static(&ctx, b""),
            String::from_static(&ctx, b"static key"),
        ] {
            assert_eq!(
                writes(CanonicalKey::String(value)),
                writes(CanonicalKey::<'static, u64>::String(value.stored_hash()))
            );
        }
    });
}

#[test]
fn cached_and_indirect_keys_survive_growth_weakening_and_reinsertion() {
    crate::Lua::empty().enter(|ctx| {
        for bytes in [
            &b""[..],
            &b"field"[..],
            &b"\0\xffkey"[..],
            &[b'x'; 256][..],
            &[b'x'; 257][..],
        ] {
            let keys = [
                String::from_slice(&ctx, bytes),
                String::from_buffer(&ctx, bytes.into()),
                String::from_static(&ctx, bytes),
            ];
            for inserted in keys {
                let mut table = RawTable::new(&ctx);
                table
                    .set(&ctx, inserted.into(), Value::Integer(42))
                    .unwrap();
                for i in 0..96 {
                    let key = ctx.intern(format!("growth-{i}").as_bytes());
                    table.set(&ctx, key.into(), Value::Integer(i)).unwrap();
                }
                for weak in [false, true] {
                    if weak {
                        table.make_keys_weak(&ctx);
                        table.make_values_weak(&ctx);
                    }
                    for key in keys {
                        let canonical = CanonicalKey::String(key);
                        assert_eq!(canonical_hash(canonical), hasher().hash_one(canonical));
                        assert!(matches!(table.get(&ctx, key.into()), Value::Integer(42)));
                        assert!(table
                            .map
                            .raw_entry()
                            .from_hash(hasher().hash_one(canonical), |stored| stored.eq(canonical))
                            .is_some());
                    }
                    table.set(&ctx, keys[1].into(), Value::Nil).unwrap();
                    for key in keys {
                        assert!(table.get(&ctx, key.into()).is_nil());
                    }
                    table.set(&ctx, keys[2].into(), Value::Integer(42)).unwrap();
                }
            }
        }
    });
}
