use super::super::super::invoke;
use super::*;

#[test]
fn budgeted_bridge_preserves_original_native_exits_at_every_pc_and_budget() {
    for text in [
        "local t={} for i=1,100 do t[i]=i end return t",
        "local t={} local sum=0 for i=1,100 do sum=sum+t[i] end return sum",
        "local t={} for i=1,100 do t[i]=(t[i]+3)*2-1 end return t",
        "local t={} for i=1,100 do t[i]=false end return t",
        "local t={} for i=1,100 do t[i]=t[i]+0.5 end return t",
        "local t={} for i=1,100 do local value=i t[i]=value end return t",
        "local t={} for i=1,100 do local value=2147483647 t[i]=value end return t",
    ] {
        Lua::empty().enter(|ctx| {
            let closure = Closure::load(ctx, None, text.as_bytes()).unwrap();
            let source = Snapshot::new(&closure.prototype(), 4096, 1024 * 1024).unwrap();
            let plan = (0..source.operations.len()).find_map(|pc| Plan::new(&source, pc)).unwrap();
            with_kernel(&source, plan, |entry| {
                for controls in [
                    [1, 32, 1, 1], [32, 1, -3, 32], [17, 30, 0, 17],
                    [63, 100, 2, 63], [1, 32, 1, 32], [32, 1, -1, 1],
                    [i64::MAX, i64::MAX, 1, 17], [i64::MIN, i64::MIN, -1, 17],
                ] {
                    for pc in plan.start..=plan.end {
                        for budget in (0..=65).chain([u32::MAX]) {
                            let initial: Vec<_> = (1..=128).map(|key| Value::Integer(key * 7)).collect();
                            let original_table = array(ctx, &initial);
                            let bounded_table = array(ctx, &initial);
                            let mut canonical = vec![Value::Integer(3); source.registers];
                            canonical[usize::from(plan.table)] = Value::Table(bounded_table);
                            for (offset, value) in controls.into_iter().enumerate() {
                                canonical[usize::from(plan.base) + offset] = Value::Integer(value);
                            }
                            let mut original_slots: Vec<_> = canonical.iter().copied().map(Slot::from_value).collect();
                            let mut bounded_slots = original_slots.clone();
                            let (first, length) = plan.window(pc, &original_slots, 128).unwrap();
                            let mut original_counts = Counts::default();
                            let expected = with_window::<_, 64>(ctx, original_table, first, length, plan.access, &mut original_counts, |window| {
                                window.with_native(|session| unsafe { session.invoke_kernel(entry, &mut original_slots, pc as u64, budget) })
                            }).unwrap();
                            let bounded = unsafe { invoke::invoke(entry, plan, ctx, &canonical, &mut bounded_slots, pc, budget) };
                            if budget == 0 {
                                assert!(bounded.is_none());
                                assert_eq!(expected.instructions, 0);
                            } else {
                                let actual = bounded.unwrap();
                                assert_eq!((actual.exit.pc, actual.exit.instructions, actual.exit.reason), (expected.pc, expected.instructions, expected.reason), "{text} pc={pc} budget={budget} controls={controls:?}");
                                assert_eq!(actual.counts, original_counts);
                            }
                            same(&original_slots, &bounded_slots);
                            for key in 1..=128 {
                                let original = Slot::from_value(original_table.get_value(ctx, key));
                                let bounded = Slot::from_value(bounded_table.get_value(ctx, key));
                                assert_eq!((original.tag, original.bits), (bounded.tag, bounded.bits), "{text} pc={pc} budget={budget} controls={controls:?} key={key}");
                            }
                        }
                    }
                }
            });
        });
    }
}
