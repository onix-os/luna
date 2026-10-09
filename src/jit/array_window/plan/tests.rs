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
