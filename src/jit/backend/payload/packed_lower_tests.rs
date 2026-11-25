use super::*;

fn allocator() -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(8 * 1024 * 1024))
}

fn expansion() -> Expansion {
    Expansion {
        instructions: 100_000,
        blocks: 10_000,
    }
}

#[test]
fn packed_whole_tag_checks_retain_snapshot_ownership_across_helper_effects() {
    let (mut source, slots, host, helpers) = fixture(false, false, 1, 1);
    let call = source
        .layout
        .blocks()
        .flat_map(|b| source.layout.block_insts(b))
        .find(|&i| source.dfg.insts[i].opcode() == Opcode::Call)
        .unwrap();
    let next = source.layout.next_inst(call).unwrap();
    let tag;
    {
        let mut c = FuncCursor::new(&mut source);
        c.goto_inst(call);
        tag = c.ins().load(types::I64, MemFlagsData::new(), slots, 0);
        for expected in abi::NIL..=abi::REFERENCE {
            let expected = c.ins().iconst(types::I64, expected as i64);
            c.ins().icmp(IntCC::Equal, tag, expected);
        }
        c.goto_inst(next);
        let mask = c.ins().iconst(types::I64, -2);
        let numeric = c.ins().band(tag, mask);
        let expected = c.ins().iconst(types::I64, abi::INTEGER as i64);
        c.ins().icmp(IntCC::Equal, numeric, expected);
    }
    let allocator = allocator();
    let baseline = allocator.0.current();
    let mut candidate = lower(
        &source,
        slots,
        host,
        2,
        &helpers,
        allocator.clone(),
        expansion(),
    )
    .unwrap();
    let checks: Vec<_> = candidate
        .records
        .iter()
        .enumerate()
        .filter_map(|(i, r)| matches!(r, Some(Record::TagCheck(_))).then_some(i))
        .collect();
    assert_eq!(checks.len(), 6);
    let tag_inst = source.dfg.value_def(tag).inst().unwrap();
    for &i in &checks {
        let Some(Record::TagCheck(check)) = &candidate.records[i] else {
            unreachable!()
        };
        assert_eq!(check.tag(), tag_inst);
    }
    let pristine = candidate.function.clone();
    for i in checks {
        let Some(Record::TagCheck(check)) = &candidate.records[i] else {
            unreachable!()
        };
        let first = check.fragment(&candidate.function).unwrap().first;
        let InstructionData::UnaryImm { imm, .. } = &mut candidate.function.dfg.insts[first] else {
            unreachable!()
        };
        *imm = (i64::from(*imm) ^ 1).into();
        cranelift_codegen::verify_function(
            &candidate.function,
            &settings::Flags::new(settings::builder()),
        )
        .unwrap();
        assert!(verify(
            &source,
            &candidate,
            slots,
            host,
            2,
            &helpers,
            allocator.clone(),
            expansion()
        )
        .is_err());
        candidate.function = pristine.clone();
    }
    drop(candidate);
    assert_eq!(allocator.0.current(), baseline);
}

fn fixture(
    scalars: bool,
    float: bool,
    kind: u32,
    destination: u32,
) -> (Function, IrValue, IrValue, [(u32, FuncRef); 1]) {
    let (mut f, slots, host, call, helpers) = packed_calls::tests::fixture(kind, destination);
    let returns: Vec<_> = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Return)
        .collect();
    for inst in returns {
        f.replace(inst).return_(&[]);
    }
    f.signature.returns.clear();
    if scalars {
        let mut c = FuncCursor::new(&mut f);
        c.goto_inst(call);
        let ty = if float { types::F64 } else { types::I64 };
        for _ in 0..3 {
            let tag = c.ins().load(types::I64, MemFlagsData::new(), slots, 0);
            let bits = c.ins().load(ty, MemFlagsData::new(), slots, 8);
            c.ins().store(MemFlagsData::new(), tag, slots, 0);
            c.ins().store(MemFlagsData::new(), bits, slots, 8);
        }
    }
    (f, slots, host, helpers)
}

#[test]
fn packed_whole_lowering_composes_reads_stores_helpers_and_returns() {
    let flags = settings::Flags::new(settings::builder());
    for float in [false, true] {
        let (source, slots, host, helpers) = fixture(true, float, 1, 1);
        cranelift_codegen::verify_function(&source, &flags).unwrap();
        let allocator = allocator();
        let before = allocator.0.current();
        let candidate = lower(
            &source,
            slots,
            host,
            2,
            &helpers,
            allocator.clone(),
            expansion(),
        )
        .unwrap();
        cranelift_codegen::verify_function(&candidate.function, &flags).unwrap();
        let expected = plan(&source, slots, host, 2, &helpers, expansion()).unwrap();
        assert_eq!(
            size(&candidate.function),
            (expected.instructions, expected.blocks)
        );
        assert_eq!(
            candidate.function.dfg.signatures.len(),
            source.dfg.signatures.len() + 2
        );
        assert!(allocator.0.current() > before);
        drop(candidate);
        assert_eq!(allocator.0.current(), before);
    }
}

#[test]
fn packed_whole_admission_is_atomic_at_expansion_and_allocation_limits() {
    let (source, slots, host, helpers) = fixture(true, false, 1, 1);
    let frozen = source.to_string();
    let plan = plan(&source, slots, host, 2, &helpers, expansion()).unwrap();
    let (instructions, blocks) = size(&source);
    let exact = Expansion {
        instructions: instructions + plan.instructions,
        blocks: blocks + plan.blocks,
    };
    for admitted in [
        exact,
        Expansion {
            instructions: exact.instructions - 1,
            ..exact
        },
        Expansion {
            blocks: exact.blocks - 1,
            ..exact
        },
    ] {
        let result = lower(&source, slots, host, 2, &helpers, allocator(), admitted);
        assert_eq!(result.is_ok(), admitted == exact);
        assert_eq!(source.to_string(), frozen);
    }
    let mut succeeded = false;
    let mut failures = 0;
    for count in 0..20 {
        let allocator = allocator();
        let before = allocator.0.current();
        allocator.0.fail_after(count);
        let result = lower(
            &source,
            slots,
            host,
            2,
            &helpers,
            allocator.clone(),
            expansion(),
        );
        succeeded |= result.is_ok();
        failures += usize::from(result.is_err());
        drop(result);
        assert_eq!(allocator.0.current(), before);
        assert_eq!(source.to_string(), frozen);
    }
    assert!(succeeded && failures >= 5);
}

#[test]
fn packed_whole_checker_rejects_unrelated_effect_return_and_order_changes() {
    let (source, slots, host, helpers) = fixture(true, false, 1, 1);
    let allocator = allocator();
    let mut candidate = lower(
        &source,
        slots,
        host,
        2,
        &helpers,
        allocator.clone(),
        expansion(),
    )
    .unwrap();
    let pristine = candidate.function.clone();
    let flags = settings::Flags::new(settings::builder());
    let mut mutations = 0;
    for block in source.layout.blocks() {
        for inst in source.layout.block_insts(block) {
            let target = match candidate.records[inst.index()].as_ref() {
                Some(Record::Return(constant)) => *constant,
                None if matches!(
                    source.dfg.insts[inst],
                    InstructionData::UnaryImm {
                        opcode: Opcode::Iconst,
                        ..
                    }
                ) =>
                {
                    inst
                }
                _ => continue,
            };
            candidate.function = pristine.clone();
            if let InstructionData::UnaryImm { imm, .. } = &mut candidate.function.dfg.insts[target]
            {
                *imm = (i64::from(*imm) + 1).into();
            }
            cranelift_codegen::verify_function(&candidate.function, &flags).unwrap();
            assert!(verify(
                &source,
                &candidate,
                slots,
                host,
                2,
                &helpers,
                allocator.clone(),
                expansion()
            )
            .is_err());
            mutations += 1;
        }
    }
    assert!(mutations >= 5);
    candidate.function = pristine.clone();
    let pair = source
        .layout
        .blocks()
        .find_map(|b| {
            source.layout.block_insts(b).find_map(|first| {
                let second = source.layout.next_inst(first)?;
                (source.dfg.insts[first].opcode() == Opcode::Iconst
                    && source.dfg.insts[second].opcode() == Opcode::Iconst)
                    .then_some((first, second))
            })
        })
        .unwrap();
    candidate.function.layout.remove_inst(pair.1);
    let mut c = FuncCursor::new(&mut candidate.function);
    c.goto_inst(pair.0);
    c.insert_inst(pair.1);
    cranelift_codegen::verify_function(&candidate.function, &flags).unwrap();
    assert!(verify(
        &source,
        &candidate,
        slots,
        host,
        2,
        &helpers,
        allocator.clone(),
        expansion()
    )
    .is_err());
    candidate.function = pristine;
    let first = source.layout.entry_block().unwrap();
    let branch = source.layout.last_inst(first).unwrap();
    let InstructionData::Brif { blocks, .. } = &mut candidate.function.dfg.insts[branch] else {
        panic!("expected branch")
    };
    blocks.swap(0, 1);
    cranelift_codegen::verify_function(&candidate.function, &flags).unwrap();
    assert!(verify(
        &source,
        &candidate,
        slots,
        host,
        2,
        &helpers,
        allocator,
        expansion()
    )
    .is_err());
}

#[cfg(not(miri))]
#[test]
fn packed_whole_generated_helpers_preserve_full_entry_contract() {
    for (kind, destination, scalars, float) in [
        (1, 1, false, false),
        (4, 1, false, false),
        (1, 99, false, false),
        (1, 1, true, false),
        (4, 1, true, true),
        (1, 99, true, true),
    ] {
        let (source, slots, host, helpers) = fixture(scalars, float, kind, destination);
        let candidate = lower(&source, slots, host, 3, &helpers, allocator(), expansion()).unwrap();
        crate::jit::backend::projection_probe::with_projection_probe(
            candidate.function,
            |pointer| {
                let native = unsafe { std::mem::transmute::<*const u8, entry::Entry>(pointer) };
                entry::check_native_helper(native, kind, destination);
            },
        )
        .unwrap();
    }
}

#[cfg(not(miri))]
#[path = "../../../../examples/jit_support/workloads.rs"]
mod workloads;

#[test]
fn packed_whole_admission_preserves_null_guards_but_refuses_host_escape() {
    for mode in 0..5 {
        let (mut source, slots, host, helpers) = fixture(false, false, 1, 1);
        let first = source.layout.entry_block().unwrap();
        let start = source.layout.first_inst(first).unwrap();
        let exit = source.dfg.block_params(first)[3];
        let mut c = FuncCursor::new(&mut source);
        c.goto_inst(start);
        if mode == 4 {
            c.ins().store(MemFlagsData::new(), host, exit, 0);
        } else {
            let zero = c.ins().iconst(types::I64, i64::from(mode == 2));
            let cond = match mode {
                0 => IntCC::Equal,
                3 => IntCC::UnsignedGreaterThan,
                _ => IntCC::NotEqual,
            };
            c.ins().icmp(cond, host, zero);
        }
        cranelift_codegen::verify_function(&source, &settings::Flags::new(settings::builder()))
            .unwrap();
        let frozen = source.to_string();
        let result = lower(&source, slots, host, 2, &helpers, allocator(), expansion());
        assert_eq!(result.is_ok(), mode < 2);
        assert_eq!(source.to_string(), frozen);
    }
}

#[cfg(not(miri))]
#[test]
fn packed_whole_lowers_original_workloads_before_native_publication() {
    fn visit(prototype: &crate::FunctionPrototype<'_>, count: &mut usize) {
        let source = Snapshot::new(prototype, 4096, 2 * 1024 * 1024).unwrap();
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(8 * 1024 * 1024));
        let metadata = allocator();
        let before = source.operations.allocator().0.current();
        let result = compile_in(
            &source,
            total.clone(),
            1024 * 1024,
            metadata.clone(),
            crate::jit::work::Limits::from(&crate::jit::JitConfig::default()),
            Failure::AuditPackedLowering,
        );
        assert!(
            matches!(result, Err(CompileError::Compilation(ref message)) if message == "packed lowering audit complete"),
            "{:?}",
            result.as_ref().err()
        );
        assert_eq!(total.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(metadata.0.current(), 0);
        assert_eq!(source.operations.allocator().0.current(), before);
        *count += 1;
        for child in prototype.prototypes.iter() {
            visit(child, count);
        }
    }
    let mut count = 0;
    crate::Lua::empty().enter(|ctx| {
        for workload in workloads::WORKLOADS {
            eprintln!(
                "packed lowering {} expected={}",
                workload.name, workload.expected
            );
            let closure = crate::Closure::load(ctx, None, workload.source).unwrap();
            visit(&closure.prototype(), &mut count);
        }
        for source in [workloads::PREDICATE_SOURCE, workloads::COLD_SOURCE] {
            let closure = crate::Closure::load(ctx, None, source).unwrap();
            visit(&closure.prototype(), &mut count);
        }
    });
    assert_eq!(count, 11);
}
