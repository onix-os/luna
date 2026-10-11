use super::*;

fn compile_packed(snapshot: &Snapshot, total: MappingCounter, metadata: BudgetAllocator) -> Code {
    compile_in(
        snapshot,
        total,
        1024 * 1024,
        metadata,
        crate::jit::work::Limits::from(&crate::jit::JitConfig::default()),
        Failure::RequirePackedPayload,
    )
    .unwrap()
}

#[test]
fn packed_owned_entries_preserve_scalar_table_boolean_exits_and_release_mappings() {
    ordinary_payload_contract(Failure::RequirePackedPayload);
}

#[test]
fn packed_owned_loops_match_every_entry_budget_and_scalar_guard() {
    optimized_payload_contract(Failure::RequirePackedPayload);
}

#[test]
fn packed_owned_entries_reject_legacy_abis_and_invalid_frame_admission() {
    use crate::jit::abi::payload::packed::scoped::bridge::entry;
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"local x=17 return x").unwrap();
        let snapshot = Snapshot::new(&closure.prototype(), 4096, 2 * 1024 * 1024).unwrap();
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(8 * 1024 * 1024));
        let metadata = BudgetAllocator(crate::jit::resources::Ledger::new(2 * 1024 * 1024));
        let code = compile_packed(&snapshot, total.clone(), metadata.clone());
        assert_eq!(code.entry_kind, EntryKind::Packed);
        assert!(code.entry_kind.needs_canonical_frame());
        assert!(!code.entry_kind.is_payload());
        let mut values = vec![crate::Value::Nil; snapshot.registers];
        let mut pc = 0;
        crate::thread::LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: snapshot.registers,
                panic: None,
                projection: None,
            };
            let legacy = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                code.invoke_payload::<256>(&mut frame, 0, 64)
            }));
            assert!(legacy.is_err());
            assert!(matches!(
                code.invoke_packed::<0>(&mut frame, 0, 64),
                Err(entry::Failure::Admission(_))
            ));
            frame.slot_count += 1;
            assert!(matches!(
                code.invoke_packed::<256>(&mut frame, 0, 64),
                Err(entry::Failure::Admission(_))
            ));
            frame.slot_count -= 1;
            frame.panic = Some(Box::new(731u64));
            assert_eq!(
                code.invoke_packed::<256>(&mut frame, 0, 64).err(),
                Some(entry::Failure::Panicked)
            );
            assert_eq!(*frame.panic.take().unwrap().downcast::<u64>().unwrap(), 731);
            assert_eq!(*frame.registers.pc, 0);
            assert!(frame
                .registers
                .stack_frame
                .iter()
                .all(|v| matches!(v, crate::Value::Nil)));
            assert_eq!(frame.count.calls, 0);
        });
        drop(code);
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(metadata.0.current(), 0);
    });
}

#[test]
fn packed_nil_fanout_compiles_and_executes_every_initialized_prefix() {
    for registers in 1usize..=256 {
        let names: Vec<_> = (0..registers - 1).map(|i| format!("r{i}")).collect();
        let script = if names.is_empty() {
            "return 'anchored'".to_owned()
        } else {
            format!("local {} return 'anchored'", names.join(","))
        };
        crate::Lua::empty().enter(|ctx| {
            let closure = crate::Closure::load(ctx, None, script.as_bytes()).unwrap();
            let snapshot = Snapshot::new(&closure.prototype(), 4096, 2 * 1024 * 1024).unwrap();
            assert_eq!(snapshot.registers, registers);
            let baseline = snapshot.operations.allocator().0.current();
            let total = MappingCounter::new(crate::jit::resources::Ledger::new(8 * 1024 * 1024));
            let metadata = BudgetAllocator(crate::jit::resources::Ledger::new(2 * 1024 * 1024));
            let control = compile(&snapshot, total.clone(), 1024 * 1024).unwrap();
            let candidate = Compiled(compile_packed(&snapshot, total.clone(), metadata.clone()));
            for pc in 0..=snapshot.operations.len() {
                for budget in [0, 1, 2, 64, u32::MAX] {
                    let mut expected = vec![crate::Value::Integer(731); registers + 1];
                    let mut actual = expected.clone();
                    let (exit, counts) =
                        run(&control, None, (ctx, closure), &mut expected, pc, budget);
                    let (got, got_counts) = run(
                        &candidate.0,
                        Some(&candidate),
                        (ctx, closure),
                        &mut actual,
                        pc,
                        budget,
                    );
                    assert_eq!(
                        (got.pc, got.instructions, got.reason),
                        (exit.pc, exit.instructions, exit.reason)
                    );
                    assert_eq!(got_counts, counts);
                    for (a, b) in actual.into_iter().zip(expected) {
                        let a = Slot::from_value(a);
                        let b = Slot::from_value(b);
                        assert_eq!((a.tag, a.bits), (b.tag, b.bits));
                    }
                }
            }
            drop((candidate, control));
            assert_eq!(total.load(Ordering::Relaxed), 0);
            assert_eq!(metadata.0.current(), 0);
            assert_eq!(snapshot.operations.allocator().0.current(), baseline);
        });
    }
}

#[test]
fn packed_publication_refusals_reclaim_resources_and_preserve_live_code() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"local a,b,c,d,e,f,g return 17").unwrap();
        let snapshot = Snapshot::new(&closure.prototype(), 4096, 2 * 1024 * 1024).unwrap();
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(8 * 1024 * 1024));
        let metadata = BudgetAllocator(crate::jit::resources::Ledger::new(2 * 1024 * 1024));
        let live = compile_packed(&snapshot, total.clone(), metadata.clone());
        let work = crate::jit::work::Limits::from(&crate::jit::JitConfig::default());
        let bound = expansion(&snapshot, work).unwrap();
        let baseline = (
            total.load(Ordering::Relaxed),
            metadata.0.current(),
            snapshot.operations.allocator().0.current(),
        );
        for limits in [
            crate::jit::work::Limits {
                instructions: bound.instructions - 1,
                ..work
            },
            crate::jit::work::Limits {
                blocks: bound.blocks - 1,
                ..work
            },
        ] {
            let result = compile_in(
                &snapshot,
                total.clone(),
                1024 * 1024,
                metadata.clone(),
                limits,
                Failure::RequirePackedPayload,
            );
            assert!(matches!(result, Err(JitError::ResourceLimit(_))));
        }
        let exact = crate::jit::work::Limits {
            instructions: bound.instructions,
            blocks: bound.blocks,
            relocations: 0,
        };
        drop(
            compile_in(
                &snapshot,
                total.clone(),
                1024 * 1024,
                metadata.clone(),
                exact,
                Failure::RequirePackedPayload,
            )
            .unwrap(),
        );
        let result = compile_in(
            &snapshot,
            total.clone(),
            0,
            metadata.clone(),
            work,
            Failure::RequirePackedPayload,
        );
        assert!(matches!(result, Err(JitError::ResourceLimit(_))));
        let mut failures = 0;
        let mut success = false;
        for count in 0..128 {
            snapshot.operations.allocator().0.fail_after(count);
            let result = compile_in(
                &snapshot,
                total.clone(),
                1024 * 1024,
                metadata.clone(),
                work,
                Failure::RequirePackedPayload,
            );
            failures += usize::from(result.is_err());
            success |= result.is_ok();
            if let Err(error) = &result {
                assert!(matches!(error, JitError::ResourceLimit(_)), "{error:?}");
            }
            drop(result);
            snapshot.operations.allocator().0.fail_after(usize::MAX);
            assert_eq!(
                (
                    total.load(Ordering::Relaxed),
                    metadata.0.current(),
                    snapshot.operations.allocator().0.current()
                ),
                baseline
            );
        }
        assert!(
            success && failures >= 8,
            "failures={failures} success={success}"
        );
        let mut values = vec![crate::Value::Nil; snapshot.registers];
        let mut pc = 0;
        crate::thread::LuaRegisters::with_test_frame(ctx, &mut pc, &mut values, |mut registers| {
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: snapshot.registers,
                panic: None,
                projection: None,
            };
            assert!(live.invoke_packed::<256>(&mut frame, 0, 64).is_ok());
        });
        drop(live);
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(metadata.0.current(), 0);
        assert_eq!(snapshot.operations.allocator().0.current(), baseline.2);
    });
}
