use super::*;
use crate::{FunctionPrototype, Lua};

fn snapshot(text: &str) -> Snapshot {
    Lua::empty().enter(|ctx| {
        let prototype = FunctionPrototype::compile(ctx, "array-window", text.as_bytes()).unwrap();
        Snapshot::new(&prototype, 4096, 1024 * 1024).unwrap()
    })
}

fn plans(source: &Snapshot) -> Vec<Plan> {
    (0..source.operations.len())
        .filter_map(|end| Plan::new(source, end))
        .collect()
}

#[test]
fn source_selection_recognizes_array_loops_without_matching_script_text() {
    let source = snapshot("local t={} for i=1,5000 do t[i]=i end local sum=0 for i=1,5000 do sum=sum+t[i] end return sum");
    let selected = plans(&source);
    assert_eq!(selected.len(), 2);
    assert_eq!(selected[0].access, Access::Write);
    assert_eq!(selected[1].access, Access::Read);
    assert_eq!(selected[0].table, selected[1].table);
    for text in [
        "local data={} for n=20,2,-3 do data[n]=n*7 end return data",
        "local data={} for n=4,19,2 do data[n]=false end return data",
        "local data={} local s=0 for n=20,2,-3 do s=s+data[n] end return s",
        "local data={} for n=1,9 do data[n]=data[n]+3 end return data",
    ] {
        assert_eq!(plans(&snapshot(text)).len(), 1, "{text}");
    }
}

#[test]
fn unstable_receivers_keys_and_helper_bodies_are_not_selected() {
    for text in [
        "local t={} for i=1,9 do t[1]=i end return t",
        "local t={} for i=1,9 do t[i]=i t={} end return t",
        "local t={} local u={} for i=1,9 do u[i]=t[i] end return u",
        "local t={} for i=1,9 do t[i]=f(i) end return t",
        "local t={} for i=1,9 do t[i]=i i=i+1 end return t",
        "local t={} for i=1,9 do t[i]='reference' end return t",
        "local t={} for i=1,9 do t[i]=i if i==3 then break end end return t",
    ] {
        assert!(plans(&snapshot(text)).is_empty(), "{text}");
    }
}

#[test]
fn runtime_window_bounds_are_fresh_bounded_and_directional() {
    let source = snapshot("local t={} for i=1,100 do t[i]=i end return t");
    let plan = plans(&source)[0];
    let mut slots = vec![Slot::from_value(Value::Nil); source.registers];
    let base = usize::from(plan.base);
    for (offset, value) in [37, 100, 1, 37].into_iter().enumerate() {
        slots[base + offset] = Slot::from_value(Value::Integer(value));
    }
    for pc in plan.start..=plan.end {
        assert_eq!(plan.window(pc, &slots, 100), Some((37, 64)));
        assert_eq!(plan.window(pc, &slots, 39), Some((37, 3)));
        assert_eq!(plan.window(pc, &slots, 36), None);
    }
    assert_eq!(plan.window(plan.end + 1, &slots, 100), None);
    assert_eq!(plan.window(plan.start, &slots[..base + 3], 100), None);
    slots[base + 2] = Slot::from_value(Value::Integer(-3));
    assert_eq!(plan.window(plan.start, &slots, 100), Some((1, 64)));
    slots[base + 3] = Slot::from_value(Value::Integer(99));
    assert_eq!(plan.window(plan.start, &slots, 100), Some((36, 64)));
    for value in [
        Value::Integer(i64::MIN),
        Value::Integer(0),
        Value::Integer(i64::MAX),
        Value::Number(99.0),
    ] {
        slots[base + 3] = Slot::from_value(value);
        assert_eq!(plan.window(plan.start, &slots, 100), None);
    }
}

#[test]
fn invalid_and_oversized_loop_sources_decline() {
    let mut source = snapshot("local t={} for i=1,9 do t[i]=i end return t");
    let plan = plans(&source)[0];
    assert!(Plan::new(&source, usize::MAX).is_none());
    source.operations[plan.end] = Operation::NumericForLoop {
        base: crate::types::RegisterIndex(255),
        jump: -2,
    };
    assert!(Plan::new(&source, plan.end).is_none());
    let body = "s=s+1 ".repeat(MAX_OPERATIONS);
    let source = snapshot(&format!(
        "local t={{}} local s=0 for i=1,9 do {body} t[i]=s end return t"
    ));
    assert!(plans(&source).is_empty());
}

#[test]
fn budget_windows_preserve_every_key_reachable_in_the_original_window() {
    let budgets: Vec<_> = if cfg!(miri) {
        vec![0, 1, 17, 64, u32::MAX]
    } else {
        (0..=65).chain([u32::MAX]).collect()
    };
    let mut combinations = 0;
    for operations in 2..=MAX_OPERATIONS {
        if cfg!(miri) && ![2, 3, 32].contains(&operations) {
            continue;
        }
        let plan = Plan {
            start: 0,
            end: operations - 1,
            base: 0,
            table: 4,
            access: Access::Write,
        };
        for pc in 0..operations {
            if cfg!(miri) && pc != 0 && pc != plan.end {
                continue;
            }
            for &budget in &budgets {
                for step in [i64::MIN, -64, -63, -3, -1, 0, 1, 2, 3, 63, i64::MAX] {
                    combinations += 1;
                    let slots =
                        [128, 256, step, 128].map(|value| Slot::from_value(Value::Integer(value)));
                    let original = plan.window(pc, &slots, 256).unwrap();
                    let bounded = plan.budget_window(pc, &slots, 256, budget).unwrap();
                    assert!(bounded.1 > 0 && bounded.0 >= original.0);
                    assert!(bounded.0 + bounded.1 as i64 <= original.0 + original.1 as i64);
                    let (mut current, mut key) = (pc, 128i64);
                    for _ in 0..budget.min(64) {
                        if current == plan.end {
                            let Some(next) = key.checked_add(step) else {
                                break;
                            };
                            key = next;
                            current = 0;
                        } else {
                            if key < original.0 || key >= original.0 + original.1 as i64 {
                                break;
                            }
                            assert!(key >= bounded.0 && key < bounded.0 + bounded.1 as i64,
                                "operations={operations} pc={pc} budget={budget} step={step} key={key} original={original:?} bounded={bounded:?}");
                            current += 1;
                        }
                    }
                }
            }
        }
    }
    assert_eq!(combinations, if cfg!(miri) { 330 } else { 388_399 });
    eprintln!("array_budget_window_combinations={combinations}");
}

#[test]
fn budget_windows_shrink_common_slices_and_preserve_refusal_and_overflow() {
    let plan = Plan {
        start: 0,
        end: 2,
        base: 0,
        table: 4,
        access: Access::Read,
    };
    let mut slots = [128, 256, 1, 128].map(|value| Slot::from_value(Value::Integer(value)));
    assert_eq!(plan.window(0, &slots, 256), Some((128, 64)));
    assert_eq!(plan.budget_window(0, &slots, 256, 64), Some((128, 22)));
    assert_eq!(plan.budget_window(0, &slots, 256, 1), Some((128, 1)));
    slots[2] = Slot::from_value(Value::Integer(-1));
    assert_eq!(plan.budget_window(0, &slots, 256, 64), Some((107, 22)));
    slots[2] = Slot::from_value(Value::Integer(0));
    assert_eq!(plan.budget_window(0, &slots, 256, 64), Some((128, 1)));
    for key in [1, 63, 64, 65, 255, 256, i64::MAX] {
        for step in [i64::MIN, -1, 0, 1, i64::MAX] {
            for budget in [0, 1, 2, 3, 17, 64, 65, u32::MAX] {
                slots = [key, 256, step, key].map(|value| Slot::from_value(Value::Integer(value)));
                if let Some(original) = plan.window(0, &slots, usize::MAX) {
                    let bounded = plan.budget_window(0, &slots, usize::MAX, budget).unwrap();
                    assert!(bounded.1 > 0 && bounded.0 >= original.0);
                    assert!(
                        i128::from(bounded.0) + bounded.1 as i128
                            <= i128::from(original.0) + original.1 as i128
                    );
                } else {
                    assert_eq!(plan.budget_window(0, &slots, usize::MAX, budget), None);
                }
                slots[0].bits ^= 1;
                assert_eq!(
                    plan.budget_window(0, &slots, usize::MAX, budget),
                    plan.window(0, &slots, usize::MAX)
                );
            }
        }
    }
    for bad in 0..4 {
        slots = [128, 256, 1, 128].map(|value| Slot::from_value(Value::Integer(value)));
        slots[bad].tag = abi::NUMBER;
        assert_eq!(plan.budget_window(0, &slots, 256, 64), None);
    }
    assert_eq!(plan.budget_window(3, &slots, 256, 64), None);
    assert_eq!(plan.budget_window(0, &slots[..3], 256, 64), None);
    slots = [128, 256, 1, 128].map(|value| Slot::from_value(Value::Integer(value)));
    let forged = Plan {
        end: usize::MAX,
        ..plan
    };
    assert_eq!(
        forged.budget_window(0, &slots, 256, 64),
        forged.window(0, &slots, 256)
    );
}

#[test]
fn fixed_window_covers_reachable_keys_until_its_capacity_boundary() {
    let budgets: Vec<_> = if cfg!(miri) {
        vec![0, 1, 17, 64, u32::MAX]
    } else {
        (0..=65).chain([u32::MAX]).collect()
    };
    let mut combinations = 0;
    for operations in 2..=MAX_OPERATIONS {
        if cfg!(miri) && ![2, 3, 32].contains(&operations) {
            continue;
        }
        let plan = Plan {
            start: 0,
            end: operations - 1,
            base: 0,
            table: 4,
            access: Access::Write,
        };
        for pc in 0..operations {
            if cfg!(miri) && pc != 0 && pc != plan.end {
                continue;
            }
            for &budget in &budgets {
                for step in [i64::MIN, -64, -63, -3, -1, 0, 1, 2, 3, 63, i64::MAX] {
                    combinations += 1;
                    let slots =
                        [128, 256, step, 128].map(|value| Slot::from_value(Value::Integer(value)));
                    let window = plan.window(pc, &slots, 256);
                    let (first, length) = window.unwrap();
                    assert!((1..=64).contains(&length));
                    assert!(first > 0 && first + length as i64 - 1 <= 256);
                    let (mut current, mut index) = (pc, 128i64);
                    let (mut low, mut high) = (128, 128);
                    for _ in 0..budget.min(64) {
                        if current == plan.end {
                            let Some(next) = index.checked_add(step) else {
                                break;
                            };
                            index = next;
                            current = 0;
                        } else {
                            if !(1..=256).contains(&index) {
                                break;
                            }
                            low = low.min(index);
                            high = high.max(index);
                            if high - low < 64 {
                                assert!(index >= first && index < first + length as i64,
                                    "operations={operations} pc={pc} budget={budget} step={step} key={index} window={window:?}");
                            }
                            current += 1;
                        }
                    }
                }
            }
        }
    }
    assert_eq!(combinations, if cfg!(miri) { 330 } else { 388_399 });
    eprintln!("array_fixed_window_combinations={combinations}");
}
