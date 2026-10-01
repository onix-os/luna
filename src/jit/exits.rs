use super::flow::Lowering;

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Interpreter = 0,
    Guard = 1,
    Budget = 2,
    Panic = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct State {
    pub resume_pc: u32,
    pub frame_pc: u32,
    pub materialized_slots: u16,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Snapshot {
    pc: u32,
    slots: u16,
    outcomes: u8,
}

impl Snapshot {
    pub fn new(pc: u32, slots: u16, lowering: Lowering) -> Self {
        let mut outcomes = 1 << Kind::Budget as u32;
        outcomes |= match lowering {
            Lowering::Direct => 0,
            Lowering::GuardedScalar => 1 << Kind::Guard as u32,
            Lowering::ScalarOrHelper(helper) => {
                (u8::from(helper == super::abi::HELPER_MOVE) << Kind::Guard as u32)
                    | (1 << Kind::Interpreter as u32)
                    | (1 << Kind::Panic as u32)
            }
            Lowering::Helper(_) => (1 << Kind::Interpreter as u32) | (1 << Kind::Panic as u32),
            Lowering::Interpreter => 1 << Kind::Interpreter as u32,
        };
        Self {
            pc,
            slots,
            outcomes,
        }
    }

    pub fn state(self, kind: Kind, written: bool) -> Option<State> {
        if self.slots > 256
            || self.outcomes & (1 << kind as u32) == 0
            || (written && kind != Kind::Panic)
        {
            return None;
        }
        Some(State {
            resume_pc: self.pc,
            frame_pc: if kind == Kind::Panic {
                self.pc.checked_add(1)?
            } else {
                self.pc
            },
            materialized_slots: self.slots,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowering_classes_admit_only_their_exit_kinds() {
        let classes = [
            (Lowering::Direct, [false, false, true, false]),
            (Lowering::GuardedScalar, [false, true, true, false]),
            (Lowering::ScalarOrHelper(1), [true, true, true, true]),
            (Lowering::Helper(1), [true, false, true, true]),
            (Lowering::Interpreter, [true, false, true, false]),
        ];
        for (lowering, permitted) in classes {
            let snapshot = Snapshot::new(17, 256, lowering);
            for (kind, permitted) in [Kind::Interpreter, Kind::Guard, Kind::Budget, Kind::Panic]
                .into_iter()
                .zip(permitted)
            {
                assert_eq!(
                    snapshot.state(kind, false).is_some(),
                    permitted,
                    "{lowering:?} {kind:?}"
                );
            }
        }
        assert_eq!(
            [
                Kind::Interpreter as u32,
                Kind::Guard as u32,
                Kind::Budget as u32,
                Kind::Panic as u32
            ],
            [0, 1, 2, 3]
        );
    }

    #[test]
    fn constant_helpers_do_not_admit_move_host_guards() {
        let constant = Snapshot::new(
            0,
            4,
            Lowering::ScalarOrHelper(super::super::abi::HELPER_CONSTANT),
        );
        assert_eq!(constant.state(Kind::Guard, false), None);
        assert!(constant.state(Kind::Interpreter, false).is_some());
        assert!(constant.state(Kind::Panic, true).is_some());
        let mov = Snapshot::new(
            0,
            4,
            Lowering::ScalarOrHelper(super::super::abi::HELPER_MOVE),
        );
        assert!(mov.state(Kind::Guard, false).is_some());
    }

    #[test]
    fn retry_and_budget_snapshots_keep_current_pc_and_full_frame() {
        for slots in [0, 1, 8, 255, 256] {
            let snapshot = Snapshot::new(41, slots, Lowering::ScalarOrHelper(1));
            for kind in [Kind::Interpreter, Kind::Guard, Kind::Budget] {
                assert_eq!(
                    snapshot.state(kind, false),
                    Some(State {
                        resume_pc: 41,
                        frame_pc: 41,
                        materialized_slots: slots
                    })
                );
                assert_eq!(snapshot.state(kind, true), None);
            }
        }
    }

    #[test]
    fn panic_snapshots_preserve_current_instruction_error_position_after_writes() {
        let snapshot = Snapshot::new(41, 256, Lowering::Helper(1));
        for written in [false, true] {
            assert_eq!(
                snapshot.state(Kind::Panic, written),
                Some(State {
                    resume_pc: 41,
                    frame_pc: 42,
                    materialized_slots: 256
                })
            );
        }
        let snapshot = Snapshot::new(u32::MAX, 1, Lowering::Helper(1));
        assert_eq!(snapshot.state(Kind::Panic, true), None);
        assert_eq!(
            snapshot.state(Kind::Interpreter, false).unwrap().resume_pc,
            u32::MAX
        );
        for kind in [Kind::Interpreter, Kind::Budget, Kind::Panic] {
            assert_eq!(
                Snapshot::new(0, 257, Lowering::Helper(1)).state(kind, false),
                None
            );
        }
    }
}
