use super::*;
use crate::{types::*, Closure, Lua};

const CALLBACK: &[u8] = b"local sum=0 for i=1,5000 do sum=sum+host_increment(i) end return sum";

fn snapshot(ctx: crate::Context<'_>, text: &[u8]) -> Snapshot {
    let closure = Closure::load(ctx, None, text).unwrap();
    Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap()
}

#[test]
fn source_capability_rejects_reference_loads_including_unreachable_ones() {
    Lua::empty().enter(|ctx| {
        let mut source = snapshot(ctx, CALLBACK);
        let proof = Source::admit(&source).unwrap();
        let plan = proof.plan();
        assert_eq!(Plan::recognize(&source), Some(plan));
        source.operations.try_reserve(2).unwrap();
        source.operations.push(Operation::LoadConstant {
            dest: RegisterIndex(0),
            constant: ConstantIndex16(u16::from(plan.constant)),
        });
        source.operations.push(Operation::Return {
            start: RegisterIndex(0),
            count: VarCount::constant(0),
        });
        source.verify().unwrap();
        assert_eq!(Plan::recognize(&source), Some(plan));
        assert!(Source::admit(&source).is_none());
        source.operations.pop();
        source.operations.pop();
        source.operations[0] = Operation::LoadConstant {
            dest: RegisterIndex(0),
            constant: ConstantIndex16(u16::MAX),
        };
        assert!(Source::admit(&source).is_none());
    });
}

#[test]
fn current_inputs_require_valid_scalars_in_every_slot_and_actual_extents() {
    Lua::empty().enter(|ctx| {
        let source = snapshot(ctx, CALLBACK);
        let proof = Source::admit(&source).unwrap();
        let width = source.registers;
        let mut slots = vec![
            abi::Slot {
                tag: abi::NIL,
                bits: 0
            };
            width
        ];
        let frame = vec![Value::Nil; width + 2];
        for lane in 0..width {
            for tag in [
                abi::NIL,
                abi::BOOLEAN,
                abi::INTEGER,
                abi::NUMBER,
                abi::REFERENCE,
                5,
                u64::MAX,
            ] {
                for bits in [0, 1, u64::MAX, 1 << 63, 0x7ff8_0000_0000_1234] {
                    slots[lane] = abi::Slot { tag, bits };
                    assert_eq!(proof.fits(&slots, &frame), tag <= abi::NUMBER);
                }
            }
            slots[lane] = abi::Slot {
                tag: abi::NIL,
                bits: 0,
            };
        }
        for length in 0..width {
            assert!(!proof.fits(&slots[..length], &frame));
            assert!(!proof.fits(&slots, &frame[..length]));
        }
        slots.push(abi::Slot {
            tag: abi::NIL,
            bits: 0,
        });
        assert!(!proof.fits(&slots, &frame));
    });
}

#[test]
fn every_entry_budget_keeps_the_only_new_reference_unobserved_until_call() {
    Lua::empty().enter(|ctx| {
        let scripts: &[&[u8]] = &[
            CALLBACK,
            b"local sum=0 for i=5,1,-1 do sum=sum+host_increment(i) end return sum",
            b"local x=1 while x do local y=host_increment(x) x=false end return x",
            b"local x=host_increment(false) return x",
        ];
        let mut examined = 0;
        for script in scripts {
            let source = snapshot(ctx, script);
            let plan = Source::admit(&source).unwrap().plan();
            for start in 0..=source.operations.len() + 1 {
                for budget in 0..=64 {
                    let mut pending = vec![(start, budget, false)];
                    let mut seen = std::collections::HashSet::new();
                    while let Some((pc, remaining, reference)) = pending.pop() {
                        if remaining == 0
                            || pc >= source.operations.len()
                            || !seen.insert((pc, remaining, reference))
                        {
                            continue;
                        }
                        examined += 1;
                        let op = source.operations[pc];
                        if matches!(
                            op,
                            Operation::Call { .. }
                                | Operation::TailCall { .. }
                                | Operation::Return { .. }
                        ) {
                            continue;
                        }
                        if reference {
                            assert!(pc > plan.helper_pc as usize && pc < plan.call_pc as usize);
                            match op {
                                Operation::Move { dest, source } => {
                                    assert_ne!(source.0, plan.destination);
                                    assert_ne!(dest.0, plan.destination);
                                }
                                Operation::LoadConstant { dest, .. }
                                | Operation::LoadBool {
                                    dest,
                                    skip_next: false,
                                    ..
                                } => {
                                    assert_ne!(dest.0, plan.destination);
                                }
                                Operation::LoadNil { dest, count } => {
                                    assert!(!(usize::from(dest.0)
                                        ..usize::from(dest.0) + usize::from(count))
                                        .contains(&usize::from(plan.destination)));
                                }
                                _ => panic!("non-straight lookup continuation"),
                            }
                        }
                        if let Operation::LoadConstant { constant, .. } = op {
                            assert!(source.constants[usize::from(constant.0)].tag <= abi::NUMBER);
                        }
                        let reference = reference || pc == plan.helper_pc as usize;
                        let mut next = |pc| pending.push((pc, remaining - 1, reference));
                        let target = |offset: i16| {
                            pc.checked_add(1)
                                .unwrap()
                                .checked_add_signed(isize::from(offset))
                                .unwrap()
                        };
                        match op {
                            Operation::Jump { offset, .. } => next(target(offset)),
                            Operation::NumericForPrep { jump, .. } => next(target(jump)),
                            Operation::NumericForLoop { jump, .. } => {
                                next(pc + 1);
                                next(target(jump));
                            }
                            Operation::Test { .. } | Operation::TestSet { .. } => {
                                next(pc + 1);
                                next(pc + 2);
                            }
                            Operation::LoadBool { skip_next, .. } => {
                                next(pc + 1 + usize::from(skip_next))
                            }
                            _ => next(pc + 1),
                        }
                    }
                }
            }
        }
        assert!(examined > 1000);
        eprintln!("hostless_scalar_flow_states={examined}");
    });
}
