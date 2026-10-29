use super::*;
use crate::jit::readonly_lookup::binding::integer_resume::Plan;

#[test]
fn canonical_resume_refusal_preserves_peers_and_releases_temporary_storage() {
    Lua::empty().enter(|ctx| {
        let env = Table::new(&ctx);
        env.set_field(
            ctx,
            "host_increment",
            Callback::from_fn(&ctx, |_, _, _| panic!("unexpected user")),
        );
        let closure = Closure::load_with_env(ctx, None, SOURCE, env).unwrap();
        let (source, ordinary, read, maps, ledger) = pair(ctx, closure);
        let mapped = maps.load(Ordering::Relaxed);
        let retained = ledger.current();
        let workspace = source.operations.allocator().0.current();
        let mut limits = crate::jit::work::Limits::from(&JitConfig::default());
        limits.instructions = 511;
        assert!(matches!(
            crate::jit::backend::integer_resume::compile(
                &source,
                maps.clone(),
                8 << 20,
                BudgetAllocator(ledger.clone()),
                limits
            ),
            Err(crate::jit::CompileError::ResourceLimit(_))
        ));
        assert!(matches!(
            crate::jit::backend::integer_resume::compile(
                &source,
                maps.clone(),
                0,
                BudgetAllocator(ledger.clone()),
                (&JitConfig::default()).into()
            ),
            Err(crate::jit::CompileError::ResourceLimit(_))
        ));
        assert_eq!(maps.load(Ordering::Relaxed), mapped);
        assert_eq!(ledger.current(), retained);
        assert_eq!(source.operations.allocator().0.current(), workspace);
        drop((ordinary, read));
        assert_eq!(maps.load(Ordering::Relaxed), 0);
        assert_eq!(ledger.current(), 0);
    });
}

#[test]
fn canonical_resume_template_rejects_changed_native_memory_and_control_flow() {
    use crate::jit::readonly_lookup::binding::integer_resume as direct;
    use cranelift_codegen::{
        ir::{condcodes::IntCC, InstructionData},
        settings,
    };
    Lua::empty().enter(|ctx| {
        let env = Table::new(&ctx);
        env.set_field(
            ctx,
            "host_increment",
            Callback::from_fn(&ctx, |_, _, _| panic!("unexpected callback")),
        );
        let closure = Closure::load_with_env(ctx, None, SOURCE, env).unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
        let plan = Plan::recognize(&source).unwrap();
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(settings::builder()))
            .unwrap();
        let target = isa.frontend_config();
        let convention = isa.default_call_conv();
        let original = direct::function(plan, target, convention);
        direct::verify(plan, &original, target, convention).unwrap();
        let mut mutations = 0;
        for block in original.layout.blocks() {
            for inst in original.layout.block_insts(block) {
                let mut changed = original.clone();
                let mutated = match &mut changed.dfg.insts[inst] {
                    InstructionData::UnaryImm { imm, .. } => {
                        *imm = (i64::from(*imm) ^ 1).into();
                        true
                    }
                    InstructionData::Load { offset, .. }
                    | InstructionData::Store { offset, .. } => {
                        let value: i32 = (*offset).into();
                        *offset = (value + 1).into();
                        true
                    }
                    InstructionData::Brif { blocks, .. } => {
                        blocks.swap(0, 1);
                        true
                    }
                    InstructionData::IntCompare { cond, .. } => {
                        *cond = if *cond == IntCC::Equal {
                            IntCC::NotEqual
                        } else {
                            IntCC::Equal
                        };
                        true
                    }
                    _ => false,
                };
                if mutated {
                    assert!(direct::verify(plan, &changed, target, convention).is_err());
                    mutations += 1;
                }
            }
        }
        assert!(mutations > 30);
        eprintln!("canonical_resume_rejected_ir_mutations={mutations}");
    });
}

#[test]
fn canonical_native_resume_matches_existing_readonly_entry_at_every_budget() {
    Lua::empty().enter(|ctx| {
        let env = Table::new(&ctx);
        let callback: Value<'_> =
            Callback::from_fn(&ctx, |_, _, _| panic!("native entry called user")).into();
        env.set_field(ctx, "host_increment", callback);
        for script in [
            SOURCE,
            b"local sum=0 for i=5,1,-1 do sum=sum+host_increment(i) end return sum".as_slice(),
        ] {
            let closure = Closure::load_with_env(ctx, None, script, env).unwrap();
            let (source, ordinary, read, maps, ledger) = pair(ctx, closure);
            let plan = Plan::recognize(&source).unwrap();
            let code = crate::jit::backend::integer_resume::compile(
                &source,
                maps.clone(),
                8 << 20,
                BudgetAllocator(ledger.clone()),
                (&JitConfig::default()).into(),
            )
            .unwrap();
            for corrupted in 0..3 {
                let mut cells = [1i64, 2, 3, 4, 5, 6];
                let before = cells;
                let pointer = cells.as_mut_ptr();
                let mut view = crate::jit::readonly_lookup::binding::integer_resume::View {
                    cookie: plan.cookie(),
                    cells: std::array::from_fn(|i| unsafe { pointer.add(i) }),
                    argument: 77,
                    finish: 88,
                    executed: 0,
                };
                if corrupted < 2 {
                    view.cookie[corrupted] ^= 1;
                } else {
                    view.executed = 1;
                }
                let cookie = view.cookie;
                let marker = view.executed;
                let mut exit = crate::jit::abi::Exit::default();
                unsafe { (code.entry)(&mut view, 64, &mut exit) };
                assert_eq!(exit.pc, plan.entry as u64);
                assert_eq!(exit.instructions, 0);
                assert_eq!(exit.reason, crate::jit::exits::Kind::Guard as u32);
                assert_eq!(cells, before);
                assert_eq!(
                    (view.cookie, view.argument, view.finish, view.executed),
                    (cookie, 77, 88, marker)
                );
            }
            for (index, limit, step) in [
                (0, 3, 1),
                (3, 3, 1),
                (3, 0, -1),
                (0, 0, -1),
                (i64::MAX, i64::MAX, 1),
                (i64::MIN, i64::MIN, -1),
                (1, 1, 0),
            ] {
                for (sum, right) in [(0, 1), (i64::MAX, 1), (i64::MIN, -1), (-7, 9)] {
                    for binding in [
                        callback,
                        Value::Integer(77),
                        Value::Number(-0.0),
                        Value::Table(env),
                    ] {
                        env.set_field(ctx, "host_increment", binding);
                        for budget in 0..=65 {
                            let mut initial = vec![Value::Integer(19); source.registers];
                            for (role, value) in plan
                                .roles()
                                .into_iter()
                                .zip([sum, right, index, limit, step, 19])
                            {
                                initial[usize::from(role)] = Value::Integer(value);
                            }
                            let expected = execute(
                                ctx,
                                closure,
                                &ordinary,
                                Some(&read),
                                plan.entry,
                                budget,
                                &initial,
                            );
                            let mut actual = initial;
                            let mut cursor = plan.entry;
                            let outcome = LuaRegisters::with_test_frame(
                                ctx,
                                &mut cursor,
                                &mut actual,
                                |mut registers| unsafe {
                                    plan.invoke(
                                        code.entry,
                                        closure,
                                        &ordinary.entries,
                                        &mut registers,
                                        plan.entry,
                                        budget,
                                    )
                                },
                            )
                            .unwrap()
                            .unwrap();
                            assert_eq!(
                                (
                                    outcome.exit.pc,
                                    outcome.exit.instructions,
                                    outcome.exit.reason
                                ),
                                (expected.0.pc, expected.0.instructions, expected.0.reason),
                                "budget={budget} index={index} step={step}"
                            );
                            assert_eq!(outcome.reads, expected.3[3]);
                            for (a, b) in actual.into_iter().zip(expected.1) {
                                same(a, b);
                            }
                        }
                    }
                }
            }
            drop((code, ordinary, read));
            assert_eq!(maps.load(Ordering::Relaxed), 0);
            assert_eq!(ledger.current(), 0);
        }
    });
}
