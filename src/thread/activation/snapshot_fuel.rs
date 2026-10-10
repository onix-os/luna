use super::{Fuel, LuaFrame};

#[inline(always)]
pub(super) fn prepare(initial: &Fuel, arguments: u8, prefix: u32) -> Option<Fuel> {
    if prefix > 64 {
        return None;
    }
    let charge = LuaFrame::FUEL_PER_CALL
        + crate::fuel::count_fuel(LuaFrame::FUEL_PER_ITEM, usize::from(arguments))
        + 4
        + prefix as i32;
    let remaining = initial.remaining();
    if remaining <= charge || initial.is_interrupted() {
        return None;
    }
    let mut fuel = initial.clone();
    fuel.set_remaining(remaining - charge - LuaFrame::FUEL_PER_CALL - 3 - 4);
    Some(fuel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_fuel_matches_sequential_charges_at_every_bounded_total() {
        let arguments: Vec<u8> = if cfg!(miri) {
            vec![0, 1, 254, 255]
        } else {
            (0..=u8::MAX).collect()
        };
        let prefixes: Vec<u32> = if cfg!(miri) {
            vec![0, 1, 63, 64]
        } else {
            (0..=64).collect()
        };
        for arguments in arguments {
            for &prefix in &prefixes {
                let charge = LuaFrame::FUEL_PER_CALL
                    + crate::fuel::count_fuel(LuaFrame::FUEL_PER_ITEM, usize::from(arguments))
                    + 4
                    + prefix as i32;
                assert!((8..=327).contains(&charge));
                for remaining in [
                    i32::MIN,
                    i32::MIN + charge - 1,
                    i32::MIN + charge,
                    i32::MIN + charge + 1,
                    -1,
                    0,
                    1,
                    charge - 1,
                    charge,
                    charge + 1,
                    charge + 10,
                    charge + 11,
                    charge + 12,
                    i32::MAX - 1,
                    i32::MAX,
                ] {
                    for interrupted in [false, true] {
                        let mut initial = Fuel::with(remaining);
                        if interrupted {
                            initial.interrupt();
                        }
                        let mut expected = initial.clone();
                        expected.consume(LuaFrame::FUEL_PER_CALL);
                        expected.consume(crate::fuel::count_fuel(
                            LuaFrame::FUEL_PER_ITEM,
                            usize::from(arguments),
                        ));
                        expected.consume(4);
                        expected.consume(prefix as i32);
                        let ready = expected.should_continue();
                        expected.consume(LuaFrame::FUEL_PER_CALL);
                        expected.consume(3);
                        expected.consume(4);
                        let actual = prepare(&initial, arguments, prefix);
                        assert_eq!(
                            actual.map(|fuel| (fuel.remaining(), fuel.is_interrupted())),
                            ready.then_some((expected.remaining(), expected.is_interrupted())),
                            "arguments={arguments} prefix={prefix} fuel={remaining} interrupted={interrupted}"
                        );
                        assert_eq!(initial.remaining(), remaining);
                        assert_eq!(initial.is_interrupted(), interrupted);
                    }
                }
            }
        }
        for prefix in [65, i32::MAX as u32, u32::MAX] {
            assert!(prepare(&Fuel::with(i32::MAX), 0, prefix).is_none());
        }
    }
}
