use super::*;
use crate::{types::*, Closure, Lua};

const CALLBACK: &[u8] = b"local sum=0 for i=1,5000 do sum=sum+host_increment(i) end return sum";

fn source(ctx: crate::Context<'_>) -> Snapshot {
    let closure = Closure::load(ctx, None, CALLBACK).unwrap();
    Snapshot::new(&closure.prototype(), 4096, 2 * 1024 * 1024).unwrap()
}

#[test]
fn frozen_callback_source_has_one_read_and_a_straight_call_chain() {
    Lua::empty().enter(|ctx| {
        let source = source(ctx);
        let plan = Plan::recognize(&source).unwrap_or_else(|| {
            panic!(
                "prepared={} operations={:?}",
                source.prepared_keys.is_some(),
                source.operations
            )
        });
        let Operation::GetUpTable {
            dest,
            table,
            key: RCIndex::Constant(key),
        } = source.operations[plan.helper_pc as usize]
        else {
            panic!()
        };
        assert_eq!((dest.0, table.0, key.0), (plan.destination, plan.upvalue, plan.constant));
        assert!(matches!(source.operations[plan.call_pc as usize], Operation::Call {func, ..} if func == dest));
        for pc in plan.helper_pc as usize + 1..plan.call_pc as usize {
            assert!(matches!(source.operations[pc], Operation::Move {dest: d, source: s} if d != dest && s != dest));
        }
    });
}

#[test]
fn every_mutating_or_additional_read_helper_refuses_even_if_unreachable() {
    Lua::empty().enter(|ctx| {
        let mut source = source(ctx);
        let original = Plan::recognize(&source).unwrap();
        for op in [
            Operation::NewTable {
                dest: RegisterIndex(0),
                array_size: 0,
                map_size: 0,
            },
            Operation::SetTable {
                table: RegisterIndex(0),
                key: RCIndex::Register(RegisterIndex(0)),
                value: RCIndex::Register(RegisterIndex(0)),
            },
            Operation::GetTable {
                dest: RegisterIndex(0),
                table: RegisterIndex(0),
                key: RCIndex::Register(RegisterIndex(0)),
            },
            Operation::SetUpTable {
                table: UpValueIndex(0),
                key: RCIndex::Constant(ConstantIndex8(original.constant)),
                value: RCIndex::Register(RegisterIndex(0)),
            },
            Operation::GetUpValue {
                dest: RegisterIndex(0),
                source: UpValueIndex(0),
            },
            Operation::SetUpValue {
                dest: UpValueIndex(0),
                source: RegisterIndex(0),
            },
            Operation::SetList {
                base: RegisterIndex(0),
                count: VarCount::constant(0),
            },
            source.operations[original.helper_pc as usize],
        ] {
            source.operations.try_reserve(2).unwrap();
            source.operations.push(op);
            source.operations.push(Operation::Return {
                start: RegisterIndex(0),
                count: VarCount::constant(0),
            });
            source.verify().unwrap();
            assert!(Plan::recognize(&source).is_none(), "{op:?}");
            source.operations.pop();
            source.operations.pop();
            assert_eq!(Plan::recognize(&source), Some(original));
        }
    });
}

#[test]
fn lookup_to_call_refuses_control_flow_and_function_slot_observation() {
    Lua::empty().enter(|ctx| {
        let mut source = source(ctx);
        let original = Plan::recognize(&source).unwrap();
        let pc = original.helper_pc as usize + 1;
        assert!(pc < original.call_pc as usize);
        let previous = source.operations[pc];
        for op in [
            Operation::Move {
                dest: RegisterIndex(0),
                source: RegisterIndex(original.destination),
            },
            Operation::Move {
                dest: RegisterIndex(original.destination),
                source: RegisterIndex(0),
            },
            Operation::Jump {
                offset: 0,
                close_upvalues: Opt254::none(),
            },
            Operation::Test {
                value: RegisterIndex(0),
                is_true: false,
            },
            Operation::LoadBool {
                dest: RegisterIndex(0),
                value: false,
                skip_next: true,
            },
            Operation::Return {
                start: RegisterIndex(0),
                count: VarCount::constant(1),
            },
            Operation::Call {
                func: RegisterIndex(0),
                args: VarCount::constant(0),
                returns: VarCount::constant(0),
            },
        ] {
            source.operations[pc] = op;
            source.verify().unwrap();
            assert!(Plan::recognize(&source).is_none(), "{op:?}");
        }
        source.operations[pc] = previous;
        assert_eq!(Plan::recognize(&source), Some(original));
        source.prepared_keys = None;
        assert!(Plan::recognize(&source).is_none());
    });
}

#[test]
fn every_entry_and_budget_can_observe_the_function_only_at_the_call() {
    Lua::empty().enter(|ctx| {
        let source = source(ctx);
        let plan = Plan::recognize(&source).unwrap();
        let mut examined = 0;
        for start in 0..source.operations.len() {
            for budget in 0..=64 {
                let mut work = vec![(start, budget, false)];
                while let Some((pc, remaining, pending)) = work.pop() {
                    examined += 1;
                    if remaining == 0 || pc >= source.operations.len() {
                        continue;
                    }
                    if pc == plan.helper_pc as usize {
                        assert!(!pending);
                        work.push((pc + 1, remaining - 1, true));
                        continue;
                    }
                    if pending && pc != plan.call_pc as usize {
                        let Operation::Move { dest, source } = source.operations[pc] else {
                            panic!("unmodeled lookup-to-call instruction")
                        };
                        assert_ne!(dest.0, plan.destination);
                        assert_ne!(source.0, plan.destination);
                    }
                    match source.operations[pc] {
                        Operation::Call { .. } | Operation::Return { .. } => {}
                        Operation::NumericForPrep { jump, .. } => {
                            work.push((
                                (pc as isize + 1 + jump as isize) as usize,
                                remaining - 1,
                                pending,
                            ));
                        }
                        Operation::NumericForLoop { jump, .. } => {
                            work.push((
                                (pc as isize + 1 + jump as isize) as usize,
                                remaining - 1,
                                pending,
                            ));
                            work.push((pc + 1, remaining - 1, pending));
                        }
                        _ => work.push((pc + 1, remaining - 1, pending)),
                    }
                }
            }
        }
        assert!(examined > 1000);
    });
}
