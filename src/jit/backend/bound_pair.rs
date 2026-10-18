use super::*;
use crate::jit::{entry_flow::preferred::Plan, flow::FlowGraph, owner::Shared, work};

pub(in crate::jit) struct Binding {
    pub pc: usize,
    pub code: Shared<Code>,
}

pub(super) fn compile(
    source: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    limits: work::Limits,
) -> Result<Code, JitError> {
    let mut ordinary = compile_in(
        source,
        total.clone(),
        limit,
        metadata.clone(),
        limits,
        Failure::None,
    )?;
    let Ok(graph) = FlowGraph::new(source) else {
        return Ok(ordinary);
    };
    let plan = Plan::new(source, &graph);
    drop(graph);
    let Some(plan) = plan else {
        return Ok(ordinary);
    };
    let Ok(_ordinary_owner) =
        Reservation::new(metadata.0.clone(), Shared::<Code>::allocation_bytes())
    else {
        return Ok(ordinary);
    };
    let expansion = work::Expansion::admit(source, limits)?;
    let remaining = work::Limits {
        instructions: limits.instructions.saturating_sub(expansion.instructions),
        blocks: limits.blocks.saturating_sub(expansion.blocks),
        relocations: limits.relocations.saturating_sub(ordinary.relocations),
    };
    if let Ok(specialized) = compile_in(
        source,
        total,
        limit,
        metadata.clone(),
        remaining,
        Failure::RequireBoundBudget(plan.resume(), 64),
    ) {
        if let Ok(code) = Shared::try_new(specialized, metadata) {
            ordinary.bound_entry = Some(Binding {
                pc: plan.resume() as usize,
                code,
            });
        }
    }
    Ok(ordinary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::resources::{owned, Ledger};
    use crate::{Closure, Lua};

    fn snapshot() -> Snapshot {
        Lua::empty().enter(|ctx| {
            let closure = Closure::load(
                ctx,
                None,
                b"local sum=0 for i=1,80 do sum=sum+host_increment(i) end return sum",
            )
            .unwrap();
            let source = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
            Snapshot {
                prepared_keys: None,
                operations: owned(&source.operations),
                constants: owned(&source.constants),
                registers: source.registers,
                upvalues: source.upvalues,
                prototypes: source.prototypes,
            }
        })
    }

    #[test]
    fn pair_selects_only_matching_pc_and_effective_budget() {
        let source = snapshot();
        let mapped = MappingCounter::new(Ledger::new(usize::MAX));
        let metadata = Ledger::new(2 << 20);
        let ordinary = super::super::compile_in(
            &source,
            mapped.clone(),
            8 << 20,
            BudgetAllocator(metadata.clone()),
            work::Limits::from(&crate::jit::JitConfig::default()),
            Failure::None,
        )
        .unwrap();
        let mut pair = compile(
            &source,
            mapped.clone(),
            8 << 20,
            BudgetAllocator(metadata.clone()),
            work::Limits::from(&crate::jit::JitConfig::default()),
        )
        .unwrap();
        let binding = pair.bound_entry.as_ref().unwrap();
        assert_eq!(pair.entries, ordinary.entries);
        for pc in (0..=source.operations.len()).chain([usize::MAX]) {
            for budget in [0, 1, 63, 64, 65, u32::MAX] {
                let expected = if pc == binding.pc && budget >= 64 {
                    binding.code.entry
                } else {
                    pair.entry
                };
                assert!(std::ptr::fn_addr_eq(
                    pair.selected_entry(pc, budget),
                    expected
                ));
                let mut actual = vec![Slot::canonical(); source.registers];
                let mut expected = actual.clone();
                let a = pair.invoke(&mut actual, pc, budget);
                let b = ordinary.invoke(&mut expected, pc, budget);
                assert_eq!(
                    (a.pc, a.instructions, a.reason),
                    (b.pc, b.instructions, b.reason)
                );
                assert_eq!(
                    actual.iter().map(|s| (s.tag, s.bits)).collect::<Vec<_>>(),
                    expected.iter().map(|s| (s.tag, s.bits)).collect::<Vec<_>>()
                );
            }
        }
        let before = mapped.load(Ordering::Relaxed);
        assert!(pair.discard_optional_entries());
        assert!(!pair.discard_optional_entries());
        assert!(mapped.load(Ordering::Relaxed) < before);
        assert_eq!(pair.entries, ordinary.entries);
        drop((pair, ordinary));
        assert_eq!(mapped.load(Ordering::Relaxed), 0);
        assert_eq!(mapped.requested(), 0);
        assert_eq!(metadata.current(), 0);
    }

    #[test]
    fn optional_image_refusal_preserves_ordinary_coverage_and_releases_charges() {
        let source = snapshot();
        let limits = work::Limits::from(&crate::jit::JitConfig::default());
        let expansion = work::Expansion::admit(&source, limits).unwrap();
        let baseline_memory = MappingCounter::new(Ledger::new(usize::MAX));
        let ordinary = super::super::compile(&source, baseline_memory.clone(), 8 << 20).unwrap();
        let ordinary_bytes = baseline_memory.load(Ordering::Relaxed);
        for fault in 0..3 {
            let mapped = MappingCounter::new(Ledger::new(usize::MAX));
            let metadata = Ledger::new(2 << 20);
            let mut limits = limits;
            if fault == 1 {
                limits.instructions = expansion.instructions;
            }
            if fault == 2 {
                limits.blocks = expansion.blocks;
            }
            let pair = compile(
                &source,
                mapped.clone(),
                if fault == 0 { ordinary_bytes } else { 8 << 20 },
                BudgetAllocator(metadata.clone()),
                limits,
            )
            .unwrap();
            assert!(pair.bound_entry.is_none());
            assert_eq!(pair.entries, ordinary.entries);
            assert_eq!(mapped.load(Ordering::Relaxed), ordinary_bytes);
            drop(pair);
            assert_eq!(mapped.load(Ordering::Relaxed), 0);
            assert_eq!(mapped.requested(), 0);
            assert_eq!(metadata.current(), 0);
        }
    }

    #[test]
    fn child_lease_survives_parent_drop_and_parent_owner_refusal_discards_child() {
        let source = snapshot();
        for refuse_parent in [false, true] {
            let mapped = MappingCounter::new(Ledger::new(usize::MAX));
            let metadata = Ledger::new(2 << 20);
            let pair = compile(
                &source,
                mapped.clone(),
                8 << 20,
                BudgetAllocator(metadata.clone()),
                work::Limits::from(&crate::jit::JitConfig::default()),
            )
            .unwrap();
            let pc = pair.bound_entry.as_ref().unwrap().pc;
            if refuse_parent {
                metadata.set_limit(metadata.current() + Shared::<Code>::allocation_bytes() - 1);
                let parent = pair.into_shared(BudgetAllocator(metadata.clone())).unwrap();
                assert!(parent.bound_entry.is_none());
                assert!(metadata.refusals() > 0);
                let mut slots = vec![Slot::canonical(); source.registers];
                let exit = parent.invoke(&mut slots, pc, 0);
                assert_eq!(
                    (exit.pc, exit.instructions, exit.reason),
                    (pc as u64, 0, ExitKind::Budget as u32)
                );
                drop(parent);
            } else {
                let child = pair.bound_entry.as_ref().unwrap().code.clone();
                let before = mapped.load(Ordering::Relaxed);
                drop(pair);
                assert!(mapped.load(Ordering::Relaxed) > 0);
                assert!(mapped.load(Ordering::Relaxed) < before);
                let mut slots = vec![Slot::canonical(); source.registers];
                let exit = child.invoke(&mut slots, pc, 64);
                assert_eq!(exit.pc, pc as u64);
                assert_eq!(exit.instructions, 0);
                drop(child);
            }
            assert_eq!(mapped.load(Ordering::Relaxed), 0);
            assert_eq!(mapped.requested(), 0);
            assert_eq!(metadata.current(), 0);
        }
    }
}
