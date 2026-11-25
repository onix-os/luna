use crate::jit::{abi, access::Access, ir::Snapshot, readonly_lookup::Plan};
use crate::Value;

#[derive(Clone, Copy)]
pub(in crate::jit) struct Publication {
    plan: Plan,
    first: usize,
}

impl Publication {
    pub(in crate::jit) fn recognize(source: &Snapshot) -> Option<Self> {
        let plan = Plan::recognize(source)?;
        let mut first = usize::from(plan.destination());
        for &operation in &source.operations[plan.helper_pc as usize + 1..plan.call_pc as usize] {
            let writes = Access::new(operation, source).writes;
            for index in 0..source.registers {
                if writes.contains(index as u8) {
                    first = first.min(index);
                }
            }
        }
        Some(Self { plan, first })
    }

    pub(in crate::jit) fn start(self, pc: usize) -> usize {
        if (self.plan.helper_pc as usize..=self.plan.call_pc as usize).contains(&pc) {
            self.first
        } else {
            0
        }
    }

    pub(in crate::jit) fn publish(self, pc: usize, slots: &[abi::Slot], frame: &mut [Value<'_>]) {
        let width = usize::from(self.plan.registers);
        let first = if slots.len() == width && frame.len() >= width {
            self.start(pc)
        } else {
            0
        };
        abi::frame_publication::publish(&slots[first..], &mut frame[first..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{opcode::Operation, types::*, Closure, Lua};

    fn source(ctx: crate::Context<'_>) -> Snapshot {
        let closure = Closure::load(
            ctx,
            None,
            b"local sum=0 for i=1,5000 do sum=sum+host_increment(i) end return sum",
        )
        .unwrap();
        Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap()
    }

    fn same(a: Value<'_>, b: Value<'_>) {
        match (a, b) {
            (Value::Nil, Value::Nil) => {}
            (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
            (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
            (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
            _ => panic!("different publication values"),
        }
    }

    #[test]
    fn suffix_bound_covers_every_write_and_only_forward_entry_pcs() {
        Lua::empty().enter(|ctx| {
            let source = source(ctx);
            let publication = Publication::recognize(&source).unwrap();
            assert!(publication.first > 0);
            for pc in (0..source.operations.len()).chain([usize::MAX]) {
                let first = publication.start(pc);
                if pc < publication.plan.helper_pc as usize
                    || pc > publication.plan.call_pc as usize
                {
                    assert_eq!(first, 0);
                } else {
                    assert_eq!(first, publication.first);
                    for &op in &source.operations[pc..publication.plan.call_pc as usize] {
                        let writes = Access::new(op, &source).writes;
                        assert!((0..first).all(|index| !writes.contains(index as u8)));
                    }
                }
            }
            let mut changed = source;
            let index = publication.plan.helper_pc as usize + 1;
            for (op, expected) in [
                (
                    Operation::Move {
                        dest: RegisterIndex(0),
                        source: RegisterIndex(1),
                    },
                    0,
                ),
                (
                    Operation::LoadConstant {
                        dest: RegisterIndex(1),
                        constant: ConstantIndex16(0),
                    },
                    1,
                ),
                (
                    Operation::LoadBool {
                        dest: RegisterIndex(2),
                        value: true,
                        skip_next: false,
                    },
                    2,
                ),
                (
                    Operation::LoadNil {
                        dest: RegisterIndex(1),
                        count: 3,
                    },
                    1,
                ),
            ] {
                changed.operations[index] = op;
                let actual = Publication::recognize(&changed).unwrap();
                assert_eq!(
                    actual.first,
                    expected.min(usize::from(actual.plan.destination()))
                );
            }
            changed.operations[index] = Operation::LoadBool {
                dest: RegisterIndex(0),
                value: false,
                skip_next: true,
            };
            assert!(Publication::recognize(&changed).is_none());
        });
    }

    #[test]
    fn callback_resume_keeps_full_publication_of_the_accumulator() {
        Lua::empty().enter(|ctx| {
            let source = source(ctx);
            let publication = Publication::recognize(&source).unwrap();
            let resume = publication.plan.call_pc as usize + 1;
            assert!(matches!(
                source.operations[resume],
                Operation::Add {
                    dest: RegisterIndex(0),
                    ..
                }
            ));
            assert_eq!(publication.start(resume), 0);
            let mut frame = vec![Value::Integer(19); source.registers];
            let mut slots = vec![abi::Slot::from_value(Value::Integer(19)); source.registers];
            slots[0] = abi::Slot::from_value(Value::Integer(77));
            publication.publish(resume, &slots, &mut frame);
            same(frame[0], Value::Integer(77));
        });
    }

    #[test]
    fn suffix_publication_matches_full_scalar_state_and_unequal_span_tails() {
        Lua::empty().enter(|ctx| {
            let source = source(ctx);
            let publication = Publication::recognize(&source).unwrap();
            let scalar = [
                Value::Nil,
                Value::Boolean(true),
                Value::Integer(i64::MIN),
                Value::Integer(i64::MAX),
                Value::Number(-0.0),
                Value::Number(f64::from_bits(0xfff8_0000_0000_1234)),
            ];
            for pc in (0..source.operations.len()).chain([usize::MAX]) {
                for initial in scalar {
                    for replacement in scalar {
                        let mut expected = vec![initial; source.registers + 3];
                        let mut actual = expected.clone();
                        let mut slots = vec![abi::Slot::from_value(initial); source.registers];
                        for slot in &mut slots[publication.start(pc)..] {
                            *slot = abi::Slot::from_value(replacement);
                        }
                        abi::frame_publication::publish(&slots, &mut expected);
                        publication.publish(pc, &slots, &mut actual);
                        for (a, b) in actual.into_iter().zip(expected) {
                            same(a, b);
                        }
                    }
                }
            }
            for slots in [0, 1, source.registers - 1, source.registers + 1] {
                for frame in [0, 1, source.registers - 1, source.registers + 2] {
                    let slots = vec![abi::Slot::from_value(Value::Integer(77)); slots];
                    let mut expected = vec![Value::Nil; frame];
                    let mut actual = expected.clone();
                    abi::frame_publication::publish(&slots, &mut expected);
                    publication.publish(publication.plan.helper_pc as usize, &slots, &mut actual);
                    for (a, b) in actual.into_iter().zip(expected) {
                        same(a, b);
                    }
                }
            }
        });
    }
}
