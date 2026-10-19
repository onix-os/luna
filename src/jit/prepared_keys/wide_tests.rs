use super::*;
use crate::{
    jit::{
        abi,
        backend::{self, Failure},
        helper_flow::Fault,
        helpers,
        resources::{BudgetAllocator, Ledger, MappingCounter},
        work,
    },
    thread::LuaRegisters,
    Closure, Value,
};
use std::sync::atomic::Ordering;

fn assert_identical(actual: Value<'_>, expected: Value<'_>) {
    let a = abi::Slot::from_value(actual);
    let b = abi::Slot::from_value(expected);
    assert_eq!((a.tag, a.bits), (b.tag, b.bits));
}

fn compile(
    snapshot: &Snapshot,
    failure: Failure,
    memory: MappingCounter,
) -> Result<backend::Code, JitError> {
    backend::compile_in(
        snapshot,
        memory,
        8 << 20,
        BudgetAllocator(Ledger::new(8 << 20)),
        work::Limits::from(&crate::JitConfig::default()),
        failure,
    )
}

#[test]
fn wide_compiler_rejects_corruption_before_installation() {
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"target=target return target").unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
        let memory = MappingCounter::new(Ledger::new(usize::MAX));
        drop(compile(&source, Failure::WidePrepared(None), memory.clone()).unwrap());
        assert_eq!(memory.load(Ordering::Relaxed), 0);
        for fault in [
            Fault::Symbol,
            Fault::Pointer,
            Fault::Operand,
            Fault::ConstantFlag,
            Fault::PreparedLow,
            Fault::PreparedHigh,
            Fault::SourcePc,
            Fault::Signature,
            Fault::CompletedTest,
            Fault::SuccessTarget,
            Fault::SuccessCount,
            Fault::PanicTest,
            Fault::ExitTarget,
            Fault::ExitPc,
            Fault::ExitCount,
            Fault::Missing,
            Fault::ProgramCounter,
            Fault::Growth,
            Fault::ExtraCall,
            Fault::ExtraStore,
        ] {
            let result = compile(&source, Failure::WidePrepared(Some(fault)), memory.clone());
            assert!(
                matches!(result, Err(JitError::Compilation(ref message)) if message == "invalid helper call data flow"),
                "fault={}", fault as u8
            );
            assert_eq!(memory.load(Ordering::Relaxed), 0);
        }
    });
}

#[test]
fn wide_compiler_matches_all_entries_budgets_and_helper_outcomes() {
    crate::Lua::empty().enter(|ctx| {
        let sources: &[&[u8]] = &[
            b"return target",
            b"target=target return target",
            b"local a=target local b=other return a,b",
            b"local sum=0 for i=1,3 do sum=sum+target end return sum",
        ];
        let mut cases = 0;
        let mut reads = 0;
        for source in sources {
            let closure = Closure::load(ctx, None, source).unwrap();
            let snapshot = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
            let memory = MappingCounter::new(Ledger::new(usize::MAX));
            let codes = [Failure::None, Failure::WidePrepared(None)]
                .map(|failure| compile(&snapshot, failure, memory.clone()).unwrap());
            for case in 0..7 {
                let value = match case {
                    0 | 5 => Value::Integer(2),
                    1 => Value::Number(-0.0),
                    2 | 3 => Value::Nil,
                    4 => Value::Table(crate::Table::new(&ctx)),
                    _ => Value::Boolean(false),
                };
                let mt = crate::Table::new(&ctx);
                mt.set(ctx, "__index", crate::Table::new(&ctx)).unwrap();
                ctx.globals().set_metatable(ctx, (case == 3).then_some(mt));
                for pc in (0..=snapshot.operations.len()).chain([usize::MAX]) {
                    for budget in [0, 1, 2, 4, 64] {
                        let reports = codes.each_ref().map(|code| {
                            ctx.globals().set(ctx, "target", value).unwrap();
                            ctx.globals().set(ctx, "other", 23).unwrap();
                            let mut canonical = vec![Value::Integer(17); snapshot.registers];
                            let mut frame_pc = pc;
                            let report = LuaRegisters::with_test_frame(
                                ctx,
                                &mut frame_pc,
                                &mut canonical,
                                |mut registers| {
                                    let mut slots: std::vec::Vec<_> = registers
                                        .stack_frame
                                        .iter()
                                        .copied()
                                        .map(abi::Slot::from_value)
                                        .collect();
                                    let mut frame = helpers::Frame {
                                        ctx,
                                        closure,
                                        registers: &mut registers,
                                        count: helpers::Counts::default(),
                                        slot_count: slots.len(),
                                        panic: None,
                                        projection: None,
                                    };
                                    let mut host = abi::Host {
                                        data: std::ptr::from_mut(&mut frame).cast(),
                                        projection: std::ptr::null_mut(),
                                    };
                                    let globals = ctx.globals().into_inner();
                                    let borrowed = (case == 5).then(|| globals.borrow_mut(&ctx));
                                    let exit = unsafe {
                                        code.invoke_host(&mut slots, pc, budget, &mut host)
                                    };
                                    drop(borrowed);
                                    let c = frame.count;
                                    (
                                        (exit.pc, exit.instructions, exit.reason),
                                        *frame.registers.pc,
                                        [
                                            c.calls,
                                            c.completed,
                                            c.declined,
                                            c.table_reads,
                                            c.table_writes,
                                            c.upvalue_reads,
                                            c.upvalue_writes,
                                            c.allocations,
                                        ],
                                        frame.panic.is_some(),
                                        slots
                                            .iter()
                                            .map(|slot| (slot.tag, slot.bits))
                                            .collect::<std::vec::Vec<_>>(),
                                    )
                                },
                            );
                            (
                                report,
                                canonical,
                                ctx.globals().get(ctx, "target").unwrap(),
                                ctx.globals().get(ctx, "other").unwrap(),
                            )
                        });
                        assert_eq!(
                            reports[0].0, reports[1].0,
                            "case={case} pc={pc} budget={budget}"
                        );
                        for (&a, &b) in reports[0].1.iter().zip(&reports[1].1) {
                            assert_identical(a, b);
                        }
                        assert_identical(reports[0].2, reports[1].2);
                        assert_identical(reports[0].3, reports[1].3);
                        reads += reports[1].0 .2[3];
                        cases += 1;
                    }
                }
            }
            drop(codes);
            assert_eq!(memory.load(Ordering::Relaxed), 0);
        }
        assert!(cases > 1000 && reads > 0);
        eprintln!("wide_compiler_cases={cases} completed_table_reads={reads}");
    });
}

#[test]
fn wide_compiler_workspace_refusal_releases_charges() {
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return target").unwrap();
        let peak = {
            let ledger = Ledger::new(8 << 20);
            let snapshot = Snapshot::new_in(
                &closure.prototype(),
                4096,
                BudgetAllocator(ledger.clone()),
            ).unwrap();
            drop(compile(
                &snapshot,
                Failure::WidePrepared(None),
                MappingCounter::new(Ledger::new(usize::MAX)),
            ).unwrap());
            drop(snapshot);
            assert_eq!(ledger.current(), 0);
            ledger.peak()
        };
        for limit in [peak - 1, peak] {
            let ledger = Ledger::new(limit);
            let snapshot = Snapshot::new_in(
                &closure.prototype(),
                4096,
                BudgetAllocator(ledger.clone()),
            ).unwrap();
            let memory = MappingCounter::new(Ledger::new(usize::MAX));
            let result = compile(&snapshot, Failure::WidePrepared(None), memory.clone());
            if limit == peak {
                assert!(result.is_ok());
            } else {
                assert!(matches!(result, Err(JitError::ResourceLimit(_))));
            }
            drop(result);
            drop(snapshot);
            assert_eq!(ledger.current(), 0);
            assert!(ledger.peak() <= limit);
            assert_eq!(memory.load(Ordering::Relaxed), 0);
        }
        let closure = Closure::load(ctx, None, b"local key=... return _ENV[key]").unwrap();
        let snapshot = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
        let result = compile(
            &snapshot,
            Failure::WidePrepared(None),
            MappingCounter::new(Ledger::new(usize::MAX)),
        );
        assert!(matches!(result, Err(JitError::Compilation(ref message)) if message == "wide prepared helper unavailable"));
    });
}
