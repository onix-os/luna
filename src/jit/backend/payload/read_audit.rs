use super::*;

#[path = "../../../../examples/jit_support/workloads.rs"]
mod workloads;

pub(in crate::jit::backend) fn run(
    function: &Function,
    slots: IrValue,
    host: IrValue,
    registers: usize,
    helpers: &[(u32, FuncRef)],
    allocator: BudgetAllocator,
) -> Result<(), JitError> {
    let mut analysis = numeric_reads::Analysis::new(function, allocator)?;
    let (mut reads, mut candidates, mut admitted) = (0, 0, 0);
    scan(function, slots, host, registers, helpers, |rewrite| {
        if let Rewrite::Read(inst, index) = rewrite {
            let candidate = numeric_reads::candidate(function, inst, slots, index).is_some();
            let accepted = analysis.admit(function, inst, slots, index).is_some();
            reads += 1;
            candidates += usize::from(candidate);
            admitted += usize::from(accepted);
            let result = function.dfg.first_result(inst);
            eprintln!("payload read {inst} result={result} slot={index} candidate={candidate} admitted={accepted}");
        }
        Ok(())
    })?;
    eprintln!("payload reads total={reads} candidates={candidates} admitted={admitted}");
    eprintln!("{function}");
    Ok(())
}

#[test]
fn original_workload_reads() {
    fn visit(name: &str, prototype: &crate::FunctionPrototype<'_>, count: &mut usize) {
        let source = Snapshot::new(prototype, 4096, 2 * 1024 * 1024).unwrap();
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(8 * 1024 * 1024));
        let metadata = BudgetAllocator(crate::jit::resources::Ledger::new(8 * 1024 * 1024));
        let source_bytes = source.operations.allocator().0.current();
        eprintln!("payload audit begin {name}");
        let result = compile_in(
            &source,
            total.clone(),
            1024 * 1024,
            metadata.clone(),
            crate::jit::work::Limits::from(&crate::jit::JitConfig::default()),
            Failure::AuditPayloadReads,
        );
        assert!(
            matches!(result, Err(JitError::Compilation(ref message)) if message == "payload read audit complete")
        );
        assert_eq!(total.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(metadata.0.current(), 0);
        assert_eq!(source.operations.allocator().0.current(), source_bytes);
        eprintln!("payload audit end {name}");
        *count += 1;
        for (index, child) in prototype.prototypes.iter().enumerate() {
            visit(&format!("{name}/child{index}"), child, count);
        }
    }
    let mut count = 0;
    crate::Lua::empty().enter(|ctx| {
        for workload in workloads::WORKLOADS {
            eprintln!(
                "payload workload {} expected={}",
                workload.name, workload.expected
            );
            let closure = crate::Closure::load(ctx, None, workload.source).unwrap();
            visit(workload.name, &closure.prototype(), &mut count);
        }
        for (name, source) in [
            ("oslo_predicate", workloads::PREDICATE_SOURCE),
            ("cold_config", workloads::COLD_SOURCE),
        ] {
            let closure = crate::Closure::load(ctx, None, source).unwrap();
            visit(name, &closure.prototype(), &mut count);
        }
    });
    assert_eq!(count, 11);
}
