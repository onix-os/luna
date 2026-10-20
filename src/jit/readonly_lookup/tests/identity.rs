#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Identity([u64; 2]);

impl Identity {
    pub(super) fn new(
        helper_pc: u32,
        call_pc: u32,
        destination: u8,
        upvalue: u8,
        constant: u8,
        registers: u16,
    ) -> Self {
        Self([
            (u64::from(helper_pc) << 32) | u64::from(call_pc),
            (u64::from(registers) << 24)
                | (u64::from(destination) << 16)
                | (u64::from(upvalue) << 8)
                | u64::from(constant),
        ])
    }

    pub(super) fn words(self) -> [u64; 2] {
        self.0
    }

    pub(super) fn helper_pc(self) -> u32 {
        (self.0[0] >> 32) as u32
    }

    pub(super) fn call_pc(self) -> u32 {
        self.0[0] as u32
    }

    pub(super) fn destination(self) -> u8 {
        (self.0[1] >> 16) as u8
    }

    pub(super) fn upvalue(self) -> u8 {
        (self.0[1] >> 8) as u8
    }

    pub(super) fn constant(self) -> u8 {
        self.0[1] as u8
    }

    pub(super) fn registers(self) -> u16 {
        (self.0[1] >> 24) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::Identity;

    #[test]
    fn all_fields_round_trip_without_crossing_their_binding_bits() {
        for helper in [0, 1, 1 << 31, u32::MAX] {
            for call in [0, 1, 1 << 31, u32::MAX] {
                for registers in [0, 1, 256, u16::MAX] {
                    for destination in [0, 1, u8::MAX] {
                        for upvalue in [0, 1, u8::MAX] {
                            for constant in [0, 1, u8::MAX] {
                                let identity = Identity::new(
                                    helper,
                                    call,
                                    destination,
                                    upvalue,
                                    constant,
                                    registers,
                                );
                                assert_eq!(identity.helper_pc(), helper);
                                assert_eq!(identity.call_pc(), call);
                                assert_eq!(identity.destination(), destination);
                                assert_eq!(identity.upvalue(), upvalue);
                                assert_eq!(identity.constant(), constant);
                                assert_eq!(identity.registers(), registers);
                                let words = identity.words();
                                assert_eq!(words[0] & 0xffff_ffff, u64::from(call));
                                assert_eq!(words[0] >> 32, u64::from(helper));
                                assert_eq!(words[1] & 0xff, u64::from(constant));
                                assert_eq!((words[1] >> 8) & 0xff, u64::from(upvalue));
                                assert_eq!((words[1] >> 16) & 0xff, u64::from(destination));
                                assert_eq!((words[1] >> 24) & 0xffff, u64::from(registers));
                                assert_eq!(words[1] >> 40, 0);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn identity_remains_two_plain_machine_words() {
        assert_eq!(std::mem::size_of::<Identity>(), 16);
        assert_eq!(
            std::mem::align_of::<Identity>(),
            std::mem::align_of::<u64>()
        );
        assert!(!std::mem::needs_drop::<Identity>());
    }
}
