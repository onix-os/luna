use super::{backend, ir, resources, Runtime};
use crate::{thread::LuaRegisters, Closure, Fuel, Lua, Value};
use std::panic::{catch_unwind, AssertUnwindSafe};

#[test]
fn no_array_entry_matches_optional_prefix_for_every_entry_and_budget() {
    let sources: &[&[u8]] = &[
        b"return target",
        b"local a,b=target,7 return a+b",
        b"local a,b,c,d,e=1,2,3,4,5 for i=1,8 do a=a+i end return a+b+c+d+e",
    ];
    Lua::empty().enter(|ctx| {
        let values = [
            Value::Nil,
            Value::Boolean(true),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8_1234_5678_9abc)),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(
                crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return)).into(),
            ),
        ];
        let mut comparisons = 0;
        let mut native = 0;
        let mut panics = 0;
        for source in sources {
            let closure = Closure::load(ctx, None, source).unwrap();
            let snapshot = ir::Snapshot::new(&closure.prototype(), 4096, 1 << 20).unwrap();
            let total = resources::MappingCounter::new(resources::Ledger::new(2 << 20));
            let code = backend::compile(&snapshot, total, 1 << 20).unwrap();
            assert!(code.array_kernels.is_none());
            for pc in (0..snapshot.operations.len()).filter(|pc| code.entries[*pc]) {
                for budget in [0, 1, 2, 63, 64, 65, u32::MAX] {
                    for (index, value) in values.iter().copied().enumerate() {
                        ctx.globals().set(ctx, "target", value).unwrap();
                        let initial: Vec<_> = (0..code.registers + 3)
                            .map(|slot| values[(slot + index) % values.len()])
                            .collect();
                        for borrowed in [false, true] {
                            let inner = ctx.globals().into_inner();
                            let _borrow = borrowed.then(|| inner.borrow_mut(&ctx));
                            let results: Vec<_> = [false, true].into_iter().map(|selected| {
                                let runtime = Runtime::new();
                                let mut canonical = initial.clone();
                                let mut current_pc = pc;
                                let mut fuel = Fuel::with(73);
                                let result = catch_unwind(AssertUnwindSafe(|| {
                                    LuaRegisters::with_test_frame_state(
                                        ctx, &mut current_pc, &mut canonical, &mut fuel, false,
                                        |mut registers| {
                                            if selected {
                                                runtime.invoke::<256, false>(
                                                    &code, ctx, closure, &mut registers, budget,
                                                )
                                            } else {
                                                runtime.invoke_transport::<256, false, false, true>(
                                                    &code, ctx, closure, &mut registers, budget,
                                                )
                                            }
                                        },
                                    )
                                }));
                                let stats = runtime.0.borrow().stats;
                                (result.ok(), current_pc, fuel.remaining(), stats, canonical)
                            }).collect();
                            assert_eq!(results[0].0, results[1].0);
                            assert_eq!(results[0].1, results[1].1);
                            assert_eq!(results[0].2, results[1].2);
                            assert_eq!(results[0].3, results[1].3);
                            for (left, right) in results[0].4.iter().zip(&results[1].4) {
                                match (left, right) {
                                    (Value::Number(a), Value::Number(b)) => {
                                        assert_eq!(a.to_bits(), b.to_bits());
                                    }
                                    (Value::Nil, Value::Nil) => {}
                                    (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
                                    (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
                                    (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
                                    (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
                                    _ => panic!("unexpected value pair: {left:?} / {right:?}"),
                                }
                            }
                            native += results[0].3.native_instructions;
                            panics += usize::from(results[0].0.is_none());
                            comparisons += 1;
                        }
                    }
                }
            }
        }
        assert!(native > 0 && panics > 0 && comparisons > 1000);
        eprintln!("comparisons={comparisons} native_instructions={native} caught_panics={panics}");
    });
}
