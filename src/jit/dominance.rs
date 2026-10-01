use allocator_api2::vec::Vec;
use cranelift_codegen::ir::{Block, Function};

use super::{preds::Predecessors, resources::BudgetAllocator, JitError};

#[derive(Clone, Copy, Default)]
struct Node {
    parent: Option<Block>,
    rank: u32,
}

struct Visit {
    block: Block,
    next: usize,
}

pub(super) struct Dominators {
    nodes: Vec<Node, BudgetAllocator>,
}

fn invalid() -> JitError {
    JitError::Compilation("invalid frontend dominance graph".into())
}

fn charge(remaining: &mut usize) -> Result<(), JitError> {
    *remaining = remaining
        .checked_sub(1)
        .ok_or(JitError::ResourceLimit("frontend dominance work"))?;
    Ok(())
}

impl Dominators {
    #[cfg(test)]
    pub(super) fn storage_bytes(count: usize) -> (usize, usize) {
        use std::mem::size_of;
        (
            count * size_of::<Node>(),
            count * (size_of::<Node>() + size_of::<Visit>() + size_of::<Block>()),
        )
    }

    pub fn new(
        function: &Function,
        graph: &Predecessors,
        allocator: BudgetAllocator,
    ) -> Result<Self, JitError> {
        let work = function
            .dfg
            .num_blocks()
            .checked_add(graph.raw_capacity())
            .and_then(|count| count.checked_add(1))
            .and_then(|count| count.checked_mul(64))
            .ok_or(JitError::ResourceLimit("frontend dominance work"))?;
        Self::with_limit(function, graph, allocator, work)
    }

    pub(super) fn with_limit(
        function: &Function,
        graph: &Predecessors,
        allocator: BudgetAllocator,
        mut work: usize,
    ) -> Result<Self, JitError> {
        let count = function.dfg.num_blocks();
        if count >= u32::MAX as usize {
            return Err(invalid());
        }
        let refused = |_| JitError::ResourceLimit("frontend dominance storage");
        let mut nodes = Vec::new_in(allocator.clone());
        nodes.try_reserve_exact(count).map_err(refused)?;
        nodes.resize(count, Node::default());
        let mut stack = Vec::new_in(allocator.clone());
        stack.try_reserve_exact(count).map_err(refused)?;
        let mut order = Vec::new_in(allocator);
        order.try_reserve_exact(count).map_err(refused)?;
        let Some(entry) = function.layout.entry_block() else {
            return Ok(Self { nodes });
        };
        nodes[entry.as_u32() as usize].rank = u32::MAX;
        stack.push(Visit {
            block: entry,
            next: 0,
        });
        while let Some(visit) = stack.last_mut() {
            charge(&mut work)?;
            let destinations = function
                .layout
                .last_inst(visit.block)
                .map(|inst| {
                    function.dfg.insts[inst].branch_destination(
                        &function.dfg.jump_tables,
                        &function.dfg.exception_tables,
                    )
                })
                .unwrap_or(&[]);
            if let Some(destination) = destinations.get(visit.next) {
                visit.next += 1;
                let target = destination.block(&function.dfg.value_lists);
                let node = nodes
                    .get_mut(target.as_u32() as usize)
                    .ok_or_else(invalid)?;
                if !function.layout.is_block_inserted(target) {
                    return Err(invalid());
                }
                if node.rank == 0 {
                    node.rank = u32::MAX;
                    stack.push(Visit {
                        block: target,
                        next: 0,
                    });
                }
            } else {
                order.push(visit.block);
                stack.pop();
            }
        }
        order.reverse();
        for (rank, &block) in order.iter().enumerate() {
            nodes[block.as_u32() as usize].rank = rank as u32 + 1;
        }
        nodes[entry.as_u32() as usize].parent = Some(entry);
        let mut changed = true;
        while changed {
            changed = false;
            for &block in order.iter().skip(1) {
                charge(&mut work)?;
                let mut parent = None;
                for predecessor in graph.pred_iter(block) {
                    charge(&mut work)?;
                    if nodes[predecessor.block.as_u32() as usize].parent.is_none() {
                        continue;
                    }
                    parent = Some(match parent {
                        None => predecessor.block,
                        Some(previous) => {
                            Self::intersect(&nodes, previous, predecessor.block, &mut work)?
                        }
                    });
                }
                let parent = parent.ok_or_else(invalid)?;
                let node = &mut nodes[block.as_u32() as usize];
                changed |= node.parent != Some(parent);
                node.parent = Some(parent);
            }
        }
        nodes[entry.as_u32() as usize].parent = None;
        Ok(Self { nodes })
    }

    fn intersect(
        nodes: &[Node],
        mut left: Block,
        mut right: Block,
        work: &mut usize,
    ) -> Result<Block, JitError> {
        while left != right {
            charge(work)?;
            let a = nodes[left.as_u32() as usize];
            let b = nodes[right.as_u32() as usize];
            if a.rank > b.rank {
                left = a.parent.ok_or_else(invalid)?;
                if nodes[left.as_u32() as usize].rank >= a.rank {
                    return Err(invalid());
                }
            } else {
                right = b.parent.ok_or_else(invalid)?;
                if nodes[right.as_u32() as usize].rank >= b.rank {
                    return Err(invalid());
                }
            }
        }
        Ok(left)
    }

    pub fn is_reachable(&self, block: Block) -> bool {
        self.nodes
            .get(block.as_u32() as usize)
            .is_some_and(|node| node.rank != 0)
    }

    pub fn idom(&self, block: Block) -> Option<Block> {
        self.nodes
            .get(block.as_u32() as usize)
            .and_then(|node| node.parent)
    }

    pub fn dominates(&self, left: Block, mut right: Block) -> bool {
        if left == right {
            return (left.as_u32() as usize) < self.nodes.len();
        }
        if !self.is_reachable(left) || !self.is_reachable(right) {
            return false;
        }
        for _ in 0..self.nodes.len() {
            let Some(parent) = self.idom(right) else {
                return false;
            };
            if parent == left {
                return true;
            }
            right = parent;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use cranelift_codegen::{
        cursor::{Cursor, FuncCursor},
        dominator_tree::DominatorTree,
        flowgraph::ControlFlowGraph,
        ir::{types, BlockCall, InstBuilder, JumpTableData},
    };

    use super::*;
    use crate::jit::resources::Ledger;

    fn fixture(mut seed: u64, count: usize) -> Function {
        let mut function = Function::new();
        let blocks: std::vec::Vec<_> = (0..=count).map(|_| function.dfg.make_block()).collect();
        let layout: std::vec::Vec<_> = (0..count)
            .map(|index| blocks[if index == 0 { 0 } else { count - index }])
            .collect();
        for &block in &layout {
            function.layout.append_block(block);
        }
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 32) as usize
        };
        for &block in &layout {
            let mut cursor = FuncCursor::new(&mut function);
            cursor.goto_bottom(block);
            let value = cursor.ins().iconst(types::I32, 1);
            let left = layout[next() % count];
            let right = layout[next() % count];
            match next() % 4 {
                0 => {
                    cursor.ins().return_(&[]);
                }
                1 => {
                    cursor.ins().jump(left, &[]);
                }
                2 => {
                    cursor.ins().brif(value, left, &[], right, &[]);
                }
                _ => {
                    let default = BlockCall::new(left, [], &mut cursor.func.dfg.value_lists);
                    let other = BlockCall::new(right, [], &mut cursor.func.dfg.value_lists);
                    let table = cursor
                        .func
                        .dfg
                        .jump_tables
                        .push(JumpTableData::new(default, &[other, other, default]));
                    cursor.ins().br_table(value, table);
                }
            }
        }
        function
    }

    #[test]
    fn seeded_reachability_idoms_and_reachable_pairs_match_cranelift() {
        for seed in 0..256 {
            let function = fixture(seed, 3 + seed as usize % 14);
            let ledger = Ledger::new(65536);
            let graph = Predecessors::new(&function, BudgetAllocator(ledger.clone())).unwrap();
            let baseline = ledger.current();
            let dominators =
                Dominators::new(&function, &graph, BudgetAllocator(ledger.clone())).unwrap();
            let cfg = ControlFlowGraph::with_function(&function);
            let reference = DominatorTree::with_function(&function, &cfg);
            for left in cfg.blocks() {
                assert_eq!(
                    dominators.is_reachable(left),
                    reference.is_reachable(left),
                    "seed={seed} block={left}"
                );
                assert_eq!(
                    dominators.idom(left),
                    reference.idom(left),
                    "seed={seed} block={left}"
                );
                for right in cfg.blocks() {
                    if reference.is_reachable(left) && reference.is_reachable(right) {
                        assert_eq!(
                            dominators.dominates(left, right),
                            reference.dominates(left, right, &function.layout),
                            "seed={seed} left={left} right={right}"
                        );
                    } else {
                        assert_eq!(dominators.dominates(left, right), left == right);
                    }
                }
            }
            assert_eq!(
                ledger.current(),
                baseline + function.dfg.num_blocks() * size_of::<Node>()
            );
            assert_eq!(
                ledger.peak(),
                baseline
                    + function.dfg.num_blocks()
                        * (size_of::<Node>() + size_of::<Visit>() + size_of::<Block>())
            );
            drop(dominators);
            assert_eq!(ledger.current(), baseline);
            drop(graph);
            assert_eq!(ledger.current(), 0);
        }
    }

    #[test]
    fn every_partial_allocation_failure_rolls_back_to_graph_baseline() {
        for allocation in 0..3 {
            let function = fixture(1, 8);
            let ledger = Ledger::new(65536);
            let graph = Predecessors::new(&function, BudgetAllocator(ledger.clone())).unwrap();
            let baseline = ledger.current();
            ledger.fail_after(allocation);
            assert!(matches!(
                Dominators::new(&function, &graph, BudgetAllocator(ledger.clone())),
                Err(JitError::ResourceLimit("frontend dominance storage"))
            ));
            assert_eq!(ledger.current(), baseline);
        }
    }

    #[test]
    fn storage_quota_refuses_before_later_allocation_and_restores_usage() {
        let function = fixture(3, 8);
        let ledger = Ledger::new(65536);
        let graph = Predecessors::new(&function, BudgetAllocator(ledger.clone())).unwrap();
        let baseline = ledger.current();
        ledger.set_limit(
            baseline + function.dfg.num_blocks() * (size_of::<Node>() + size_of::<Visit>()) - 1,
        );
        assert!(matches!(
            Dominators::new(&function, &graph, BudgetAllocator(ledger.clone())),
            Err(JitError::ResourceLimit("frontend dominance storage"))
        ));
        assert_eq!(ledger.current(), baseline);
    }

    #[test]
    fn work_exhaustion_is_typed_and_reclaims_temporary_and_node_storage() {
        let function = fixture(7, 8);
        let ledger = Ledger::new(65536);
        let graph = Predecessors::new(&function, BudgetAllocator(ledger.clone())).unwrap();
        let baseline = ledger.current();
        assert!(matches!(
            Dominators::with_limit(&function, &graph, BudgetAllocator(ledger.clone()), 0),
            Err(JitError::ResourceLimit("frontend dominance work"))
        ));
        assert_eq!(ledger.current(), baseline);
    }

    #[test]
    fn empty_function_needs_no_storage_or_work() {
        let ledger = Ledger::new(0);
        let function = Function::new();
        let graph = Predecessors::new(&function, BudgetAllocator(ledger.clone())).unwrap();
        let dominators =
            Dominators::with_limit(&function, &graph, BudgetAllocator(ledger.clone()), 0).unwrap();
        assert!(!dominators.is_reachable(Block::from_u32(0)));
        assert_eq!(dominators.idom(Block::from_u32(0)), None);
        assert!(!dominators.dominates(Block::from_u32(0), Block::from_u32(0)));
        assert_eq!(ledger.current(), 0);
    }

    #[test]
    fn exact_storage_limit_succeeds_and_one_byte_less_refuses() {
        for shortage in [0, 1] {
            let function = fixture(7, 8);
            let ledger = Ledger::new(65536);
            let graph = Predecessors::new(&function, BudgetAllocator(ledger.clone())).unwrap();
            let baseline = ledger.current();
            let (retained, working) = Dominators::storage_bytes(function.dfg.num_blocks());
            ledger.set_limit(baseline + working - shortage);
            let result = Dominators::new(&function, &graph, BudgetAllocator(ledger.clone()));
            if shortage == 0 {
                let dominators = result.unwrap();
                assert_eq!(ledger.current(), baseline + retained);
                assert_eq!(ledger.peak(), baseline + working);
                assert_eq!(ledger.refusals(), 0);
                drop(dominators);
            } else {
                assert!(matches!(
                    result,
                    Err(JitError::ResourceLimit("frontend dominance storage"))
                ));
                assert_eq!(ledger.refusals(), 1);
            }
            assert_eq!(ledger.current(), baseline);
        }
    }

    #[test]
    fn reachable_nonlayout_target_is_rejected_and_releases_storage() {
        let mut function = Function::new();
        let entry = function.dfg.make_block();
        let target = function.dfg.make_block();
        function.layout.append_block(entry);
        let mut cursor = FuncCursor::new(&mut function);
        cursor.goto_bottom(entry);
        cursor.ins().jump(target, &[]);
        let ledger = Ledger::new(65536);
        let graph = Predecessors::new(&function, BudgetAllocator(ledger.clone())).unwrap();
        let baseline = ledger.current();
        assert!(matches!(
            Dominators::new(&function, &graph, BudgetAllocator(ledger.clone())),
            Err(JitError::Compilation(reason)) if reason == "invalid frontend dominance graph"
        ));
        assert_eq!(ledger.current(), baseline);
    }

    #[test]
    fn every_work_prefix_reclaims_dfs_and_fixed_point_storage() {
        let mut function = Function::new();
        let blocks: std::vec::Vec<_> = (0..7).map(|_| function.dfg.make_block()).collect();
        for &block in &blocks[..6] {
            function.layout.append_block(block);
        }
        for index in 0..6 {
            let mut cursor = FuncCursor::new(&mut function);
            cursor.goto_bottom(blocks[index]);
            let condition = cursor.ins().iconst(types::I32, 1);
            match index {
                0 => {
                    cursor.ins().brif(condition, blocks[1], &[], blocks[2], &[]);
                }
                1 | 2 => {
                    cursor.ins().jump(blocks[3], &[]);
                }
                3 => {
                    cursor.ins().brif(condition, blocks[3], &[], blocks[5], &[]);
                }
                _ => {
                    cursor.ins().return_(&[]);
                }
            }
        }
        let ledger = Ledger::new(65536);
        let graph = Predecessors::new(&function, BudgetAllocator(ledger.clone())).unwrap();
        let baseline = ledger.current();
        let mut accepted = None;
        for work in 0..256 {
            match Dominators::with_limit(&function, &graph, BudgetAllocator(ledger.clone()), work) {
                Err(JitError::ResourceLimit("frontend dominance work")) => {}
                Ok(dominators) => {
                    assert_eq!(dominators.idom(blocks[3]), Some(blocks[0]));
                    assert_eq!(dominators.idom(blocks[5]), Some(blocks[3]));
                    assert!(!dominators.is_reachable(blocks[4]));
                    assert!(!dominators.is_reachable(blocks[6]));
                    drop(dominators);
                    accepted = Some(work);
                }
                Err(error) => panic!("unexpected refusal for work={work}: {error:?}"),
            }
            assert_eq!(ledger.current(), baseline, "work={work}");
            if accepted.is_some() {
                break;
            }
        }
        assert!(accepted.is_some_and(|work| work > 11));
        assert_eq!(ledger.refusals(), 0);
    }
}
