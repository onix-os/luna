pub(super) const fn table<T: Copy>(choices: [T; 14]) -> [T; 257] {
    let mut entries = [choices[0]; 257];
    let mut count = 1;
    while count < entries.len() {
        let choice = match count {
            1..=8 => count,
            9..=16 => 9,
            17..=32 => 10,
            33..=64 => 11,
            65..=128 => 12,
            _ => 13,
        };
        entries[count] = choices[choice];
        count += 1;
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::table;

    const TIERS: [(usize, bool); 257] = table([
        (8, false),
        (1, true),
        (2, true),
        (3, true),
        (4, true),
        (5, true),
        (6, true),
        (7, true),
        (8, true),
        (16, false),
        (32, false),
        (64, false),
        (128, false),
        (256, false),
    ]);

    fn original(count: usize) -> Option<(usize, bool)> {
        Some(match count {
            0 => (8, false),
            1 => (1, true),
            2 => (2, true),
            3 => (3, true),
            4 => (4, true),
            5 => (5, true),
            6 => (6, true),
            7 => (7, true),
            8 => (8, true),
            9..=16 => (16, false),
            17..=32 => (32, false),
            33..=64 => (64, false),
            65..=128 => (128, false),
            129..=256 => (256, false),
            _ => return None,
        })
    }

    type Probe = fn(u64, &mut usize) -> (usize, bool, u64);

    fn probe<const CAPACITY: usize, const EXACT: bool>(
        input: u64,
        calls: &mut usize,
    ) -> (usize, bool, u64) {
        *calls += 1;
        (CAPACITY, EXACT, input)
    }

    const PROBES: [Probe; 257] = table([
        probe::<8, false> as Probe,
        probe::<1, true>,
        probe::<2, true>,
        probe::<3, true>,
        probe::<4, true>,
        probe::<5, true>,
        probe::<6, true>,
        probe::<7, true>,
        probe::<8, true>,
        probe::<16, false>,
        probe::<32, false>,
        probe::<64, false>,
        probe::<128, false>,
        probe::<256, false>,
    ]);

    #[test]
    fn every_register_count_preserves_the_original_scratch_tier() {
        for (count, &(capacity, exact)) in TIERS.iter().enumerate() {
            assert_eq!(Some((capacity, exact)), original(count));
            assert!(capacity >= count);
            assert_eq!(exact, (1..=8).contains(&count));
        }
    }

    #[test]
    fn indirect_choices_preserve_inputs_and_invoke_exactly_once() {
        let mut calls = 0;
        for (count, invoke) in PROBES.iter().enumerate() {
            let (capacity, exact) = original(count).unwrap();
            for input in [0, 1, u64::MAX, 0x0123_4567_89ab_cdef] {
                let before = calls;
                assert_eq!(invoke(input, &mut calls), (capacity, exact, input));
                assert_eq!(calls, before + 1);
            }
        }
        assert_eq!(calls, 257 * 4);
    }

    #[test]
    fn oversized_register_counts_have_no_dispatch_entry() {
        for count in [257, 258, 512, u32::MAX as usize, usize::MAX] {
            assert!(original(count).is_none());
            assert!(TIERS.get(count).is_none());
            assert!(PROBES.get(count).is_none());
        }
    }
}
