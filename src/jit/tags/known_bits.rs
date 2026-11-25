#[derive(Clone, Copy, Debug)]
pub(super) struct KnownBits {
    zero: u64,
    one: u64,
}

impl KnownBits {
    pub const UNKNOWN: Self = Self { zero: 0, one: 0 };

    pub fn exact(value: u64) -> Self {
        Self {
            zero: !value,
            one: value,
        }
    }

    pub fn and(self, rhs: Self) -> Self {
        Self {
            zero: self.zero | rhs.zero,
            one: self.one & rhs.one,
        }
    }

    pub fn or(self, rhs: Self) -> Self {
        Self {
            zero: self.zero & rhs.zero,
            one: self.one | rhs.one,
        }
    }

    pub fn xor(self, rhs: Self) -> Self {
        Self {
            zero: (self.zero & rhs.zero) | (self.one & rhs.one),
            one: (self.zero & rhs.one) | (self.one & rhs.zero),
        }
    }

    pub fn equal(self, rhs: Self) -> Option<bool> {
        if (self.one & rhs.zero) | (self.zero & rhs.one) != 0 {
            Some(false)
        } else if self.zero | self.one == u64::MAX && rhs.zero | rhs.one == u64::MAX {
            Some(true)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admits(bits: KnownBits, value: u64) -> bool {
        bits.zero & value == 0 && bits.one & !value == 0
    }

    #[test]
    fn transfers_and_equality_overapproximate_all_four_bit_values() {
        let mut states = Vec::new();
        for zero in 0..16 {
            for one in 0..16 {
                if zero & one == 0 {
                    states.push(KnownBits { zero, one });
                }
            }
        }
        for &left in &states {
            for &right in &states {
                for a in (0..16).filter(|&value| admits(left, value)) {
                    for b in (0..16).filter(|&value| admits(right, value)) {
                        assert!(admits(left.and(right), a & b));
                        assert!(admits(left.or(right), a | b));
                        assert!(admits(left.xor(right), a ^ b));
                        if let Some(equal) = left.equal(right) {
                            assert_eq!(equal, a == b);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn unknown_operands_preserve_only_proven_bits() {
        for value in [0, 1, 2, 3, 4, 1 << 32, 1 << 63, u64::MAX] {
            let exact = KnownBits::exact(value);
            assert_eq!(exact.equal(exact), Some(true));
            assert_eq!(exact.equal(KnownBits::exact(!value)), Some(false));
            assert_eq!(KnownBits::UNKNOWN.equal(exact), None);
            assert_eq!(KnownBits::UNKNOWN.xor(exact).equal(exact), None);
            assert_eq!(
                KnownBits::UNKNOWN
                    .and(KnownBits::exact(0))
                    .equal(KnownBits::exact(0)),
                Some(true)
            );
            if value != 0 {
                assert_eq!(
                    KnownBits::UNKNOWN.or(exact).equal(KnownBits::exact(0)),
                    Some(false)
                );
            }
        }
    }
}
