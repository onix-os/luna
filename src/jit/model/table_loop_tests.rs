use std::sync::atomic::Ordering;

use crate::{
    jit::{abi, backend, helpers, ir::Snapshot, resources, work, JitConfig},
    opcode::Operation,
    thread::LuaRegisters,
    Closure, Lua, Table, Value,
};

#[test]
fn typed_stores_preserve_every_helper_boundary_and_fast_entry() {
    for program in [
        &b"local t=... for i=1,7 do t[i]=i end return t"[..],
        &b"local t=... local x=0.5 for i=1,7 do x=x+0.5 t[i]=x end return x"[..],
        &b"local t=... for i=7,1,-1 do t[3]=i t[i]=0.5 end return t"[..],
        &b"local t=... local x=0.5 for i=1,7 do t[x]=i x=x+0.5 end return x"[..],
    ] {
        Lua::empty().enter(|ctx| {
            let closure = Closure::load(ctx, None, program).unwrap();
            let source = Snapshot::new(&closure.prototype(), 4096, 2 * 1024 * 1024).unwrap();
            let (end, base, start) = source
                .operations
                .iter()
                .enumerate()
                .find_map(|(pc, op)| {
                    if let Operation::NumericForLoop { base, jump } = *op {
                        Some((
                            pc,
                            usize::from(base.0),
                            (pc + 1).checked_add_signed(isize::from(jump)).unwrap(),
                        ))
                    } else {
                        None
                    }
                })
                .unwrap();
            let memory = resources::MappingCounter::new(resources::Ledger::new(usize::MAX));
            let codes = [
                backend::Failure::None,
                backend::Failure::RequireTableLoop,
                backend::Failure::ProbeTableLoop,
            ]
            .map(|failure| {
                backend::compile_in(
                    &source,
                    memory.clone(),
                    8 * 1024 * 1024,
                    resources::BudgetAllocator(resources::Ledger::new(2 * 1024 * 1024)),
                    work::Limits::from(&JitConfig::default()),
                    failure,
                )
                .unwrap()
            });
            let mut completed = 0;
            let mut declined = 0;
            let mut panicked = 0;
            let mut fast = 0;
            for pc in 0..=source.operations.len() {
                for budget in [0, 1, 2, 3, 4, 63, 64] {
                    for mode in 0..9 {
                        for seed in [1, i64::MAX, i64::MIN] {
                            let results = [0, 1, 2].map(|variant| {
                                let code = &codes[variant];
                                let table = Table::new(&ctx);
                                if mode == 1 {
                                    table.set_readonly(&ctx, true);
                                }
                                if mode == 2 {
                                    let mt = Table::new(&ctx);
                                    mt.set(ctx, "__newindex", true).unwrap();
                                    table.set_metatable(ctx, Some(mt));
                                }
                                let mut canonical = vec![Value::Integer(-99); source.registers];
                                canonical[0] = if mode == 3 {
                                    Value::Nil
                                } else {
                                    Value::Table(table)
                                };
                                let mut slots = vec![
                                    abi::Slot::from_value(Value::Integer(seed));
                                    source.registers
                                ];
                                slots[0] = abi::Slot::from_value(canonical[0]);
                                for op in &source.operations {
                                    if let Operation::LoadConstant { dest, constant } = *op {
                                        if source.constants[usize::from(constant.0)].tag
                                            == abi::NUMBER
                                        {
                                            slots[usize::from(dest.0)] =
                                                abi::Slot::from_value(Value::Number(0.5));
                                        }
                                    }
                                }
                                slots[base + 1] = abi::Slot::from_value(Value::Integer(7));
                                slots[base + 2] = abi::Slot::from_value(Value::Integer(1));
                                if mode == 5 {
                                    for slot in &mut slots {
                                        if slot.tag == abi::NUMBER {
                                            slot.bits = f64::NAN.to_bits();
                                        }
                                    }
                                }
                                if mode == 6 {
                                    slots[base] = abi::Slot::from_value(Value::Nil);
                                }
                                if mode == 7 {
                                    slots[base] = abi::Slot::from_value(Value::Number(0.5));
                                }
                                if mode == 8 {
                                    slots[0] = abi::Slot::from_value(Value::Integer(1));
                                }
                                let mut frame_pc = pc;
                                LuaRegisters::with_test_frame(
                                    ctx,
                                    &mut frame_pc,
                                    &mut canonical,
                                    |mut registers| {
                                        let mut frame = helpers::Frame {
                                            ctx,
                                            closure,
                                            registers: &mut registers,
                                            count: helpers::Counts::default(),
                                            slot_count: slots.len(),
                                            panic: None,
                                            projection: None,
                                        };
                                        let mut entered = 0u64;
                                        let mut host = abi::Host {
                                            data: std::ptr::addr_of_mut!(frame).cast(),
                                            projection: std::ptr::addr_of_mut!(entered).cast(),
                                        };
                                        let inner = table.into_inner();
                                        let lock = (mode == 4).then(|| inner.borrow_mut(&ctx));
                                        let exit = unsafe {
                                            code.invoke_host(&mut slots, pc, budget, &mut host)
                                        };
                                        drop(lock);
                                        completed += frame.count.completed;
                                        declined += frame.count.declined;
                                        panicked += u64::from(frame.panic.is_some());
                                        fast += entered;
                                        if variant == 2 {
                                            assert_eq!(
                                                entered,
                                                u64::from(
                                                    (start..=end).contains(&pc)
                                                        && budget > 0
                                                        && mode != 6
                                                        && mode != 7
                                                )
                                            );
                                        }
                                        let slot_values: Vec<_> =
                                            slots.iter().map(|s| (s.tag, s.bits)).collect();
                                        let canonical_values: Vec<_> = frame
                                            .registers
                                            .stack_frame
                                            .iter()
                                            .copied()
                                            .map(abi::Slot::from_value)
                                            .map(|s| (s.tag, s.bits))
                                            .collect();
                                        let table_values: Vec<_> = (-1..=8)
                                            .chain([seed])
                                            .map(|key| {
                                                let s = abi::Slot::from_value(
                                                    table.get_raw(&ctx, Value::Integer(key)),
                                                );
                                                (s.tag, s.bits)
                                            })
                                            .collect();
                                        (
                                            (exit.pc, exit.instructions, exit.reason),
                                            slot_values,
                                            canonical_values,
                                            table_values,
                                            *frame.registers.pc,
                                            frame.panic.is_some(),
                                            (
                                                frame.count.calls,
                                                frame.count.completed,
                                                frame.count.declined,
                                                frame.count.table_writes,
                                            ),
                                        )
                                    },
                                )
                            });
                            assert_eq!(
                                results[0], results[1],
                                "pc={pc} budget={budget} mode={mode} seed={seed}"
                            );
                            assert_eq!(
                                results[0], results[2],
                                "probe pc={pc} budget={budget} mode={mode} seed={seed}"
                            );
                        }
                    }
                }
            }
            assert!(completed > 0 && declined > 0 && panicked > 0 && fast > 0);
            drop(codes);
            assert_eq!(memory.load(Ordering::Relaxed), 0);
        });
    }
}
