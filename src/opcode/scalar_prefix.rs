use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScalarPrefix {
    pub get_dest: RegisterIndex,
    pub get_source: UpValueIndex,
    pub add_dest: RegisterIndex,
    pub left: RegisterIndex,
    pub right: RegisterIndex,
    pub set_source: RegisterIndex,
    pub set_dest: UpValueIndex,
}

impl ScalarPrefix {
    pub(crate) fn new(code: &[OpCode], registers: usize, upvalues: usize) -> Option<Self> {
        let [OpCode(OpCodeRepr::GetUpValue {
            dest: get_dest,
            source: get_source,
        }), OpCode(OpCodeRepr::AddRR {
            dest: add_dest,
            left,
            right,
        }), OpCode(OpCodeRepr::SetUpValue {
            dest: set_dest,
            source: set_source,
        }), OpCode(OpCodeRepr::Return { .. })] = code
        else {
            return None;
        };
        let plan = Self {
            get_dest: *get_dest,
            get_source: *get_source,
            add_dest: *add_dest,
            left: *left,
            right: *right,
            set_source: *set_source,
            set_dest: *set_dest,
        };
        plan.fits(registers, upvalues).then_some(plan)
    }

    pub(crate) fn fits(self, registers: usize, upvalues: usize) -> bool {
        [
            self.get_dest,
            self.add_dest,
            self.left,
            self.right,
            self.set_source,
        ]
        .into_iter()
        .all(|r| usize::from(r.0) < registers)
            && [self.get_source, self.set_dest]
                .into_iter()
                .all(|u| usize::from(u.0) < upvalues)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_shape_matches_decoded_operations_and_all_extents() {
        for r in [0, 1, 7, 127, 255] {
            for u in [0, 1, 7, 255] {
                let ops = [
                    Operation::GetUpValue {
                        dest: RegisterIndex(r),
                        source: UpValueIndex(u),
                    },
                    Operation::Add {
                        dest: RegisterIndex(0),
                        left: RegisterIndex(r).into(),
                        right: RegisterIndex(1).into(),
                    },
                    Operation::SetUpValue {
                        dest: UpValueIndex(0),
                        source: RegisterIndex(r),
                    },
                    Operation::Return {
                        start: RegisterIndex(0),
                        count: VarCount::constant(0),
                    },
                ];
                let code = ops.map(OpCode::encode);
                for registers in [0, 1, 2, 8, 128, 256] {
                    for upvalues in [0, 1, 2, 8, 256] {
                        let plan = ScalarPrefix::new(&code, registers, upvalues);
                        assert_eq!(
                            plan.is_some(),
                            registers > usize::from(r).max(1) && upvalues > usize::from(u)
                        );
                        if let Some(p) = plan {
                            assert_eq!(
                                ops[0],
                                Operation::GetUpValue {
                                    dest: p.get_dest,
                                    source: p.get_source
                                }
                            );
                            assert_eq!(
                                ops[1],
                                Operation::Add {
                                    dest: p.add_dest,
                                    left: p.left.into(),
                                    right: p.right.into()
                                }
                            );
                            assert_eq!(
                                ops[2],
                                Operation::SetUpValue {
                                    dest: p.set_dest,
                                    source: p.set_source
                                }
                            );
                        }
                    }
                }
                for position in 0..4 {
                    let mut changed = code;
                    changed[position] = OpCode::encode(Operation::Move {
                        dest: RegisterIndex(0),
                        source: RegisterIndex(1),
                    });
                    assert!(ScalarPrefix::new(&changed, 256, 256).is_none());
                }
                assert!(ScalarPrefix::new(&code[..3], 256, 256).is_none());
            }
        }
    }
}
