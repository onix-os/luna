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
            for value in values {
                let key = CanonicalKey::String(value);
                let word = CanonicalKey::<'static, u64>::String(value.stored_hash());
                assert_eq!(writes(key), writes(word), "length {len}");
                assert_eq!(hasher().hash_one(key), hasher().hash_one(word));
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
