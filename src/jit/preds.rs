use allocator_api2::vec::Vec;
use cranelift_codegen::{
    flowgraph::BlockPredecessor,
    ir::{Block, Function, Inst},
};

use super::{resources::BudgetAllocator, JitError};

#[derive(Clone, Copy)]
struct Edge {
    target: Block,
    block: Block,
    inst: Inst,
}

pub(super) struct Predecessors {
    edges: Vec<Edge, BudgetAllocator>,
}

impl Predecessors {
    pub fn raw_capacity(&self) -> usize {
        self.edges.capacity()
    }

    pub fn empty(allocator: BudgetAllocator) -> Self {
        Self {
            edges: Vec::new_in(allocator),
        }
    }

    pub fn new(function: &Function, allocator: BudgetAllocator) -> Result<Self, JitError> {
        let refused = || JitError::ResourceLimit("frontend predecessor graph");
        let mut count = 0usize;
        for block in function.layout.blocks() {
            if let Some(inst) = function.layout.last_inst(block) {
                count = count
                    .checked_add(
                        function.dfg.insts[inst]
                            .branch_destination(
                                &function.dfg.jump_tables,
                                &function.dfg.exception_tables,
                            )
                            .len(),
                    )
                    .ok_or_else(refused)?;
            }
        }
        let mut graph = Self::empty(allocator);
        graph
            .edges
            .try_reserve_exact(count)
            .map_err(|_| refused())?;
        for block in function.layout.blocks() {
            let Some(inst) = function.layout.last_inst(block) else {
                continue;
            };
            for destination in function.dfg.insts[inst]
                .branch_destination(&function.dfg.jump_tables, &function.dfg.exception_tables)
            {
                let target = destination.block(&function.dfg.value_lists);
                if target.as_u32() as usize >= function.dfg.num_blocks() {
                    return Err(JitError::Compilation("invalid predecessor target".into()));
                }
                graph.edges.push(Edge {
                    target,
                    block,
                    inst,
                });
            }
        }
        graph
            .edges
            .sort_unstable_by_key(|edge| (edge.target.as_u32(), edge.inst.as_u32()));
        graph.edges.dedup_by_key(|edge| (edge.target, edge.inst));
        Ok(graph)
    }

    pub fn pred_iter(&self, block: Block) -> impl Iterator<Item = BlockPredecessor> + '_ {
        let start = self
            .edges
            .partition_point(|edge| edge.target.as_u32() < block.as_u32());
        self.edges[start..]
            .iter()
            .take_while(move |edge| edge.target == block)
            .map(|edge| BlockPredecessor::new(edge.block, edge.inst))
    }
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use cranelift_codegen::{
        cursor::{Cursor, FuncCursor},
        flowgraph::ControlFlowGraph,
        ir::{types, BlockCall, InstBuilder, JumpTableData},
    };

    use super::*;
    use crate::jit::resources::Ledger;

    fn fixture(duplicate: bool) -> Function {
        let mut function = Function::new();
        let blocks: [_; 5] = std::array::from_fn(|_| function.dfg.make_block());
        for block in [blocks[0], blocks[2], blocks[3], blocks[4]] {
            function.layout.append_block(block);
        }
        let mut cursor = FuncCursor::new(&mut function);
        cursor.goto_bottom(blocks[0]);
        let value = cursor.ins().iconst(types::I32, 1);
        cursor.ins().brif(
            value,
            blocks[2],
            &[],
            if duplicate { blocks[2] } else { blocks[3] },
            &[],
        );
        cursor.goto_bottom(blocks[2]);
        cursor.ins().jump(blocks[4], &[]);
        cursor.goto_bottom(blocks[3]);
        let default = BlockCall::new(blocks[4], [], &mut cursor.func.dfg.value_lists);
        let repeat = BlockCall::new(blocks[2], [], &mut cursor.func.dfg.value_lists);
        let table = cursor
            .func
            .dfg
            .jump_tables
            .push(JumpTableData::new(default, &[repeat, repeat, default]));
        cursor.ins().br_table(value, table);
        cursor.goto_bottom(blocks[4]);
        cursor.ins().return_(&[]);
        function
    }

    #[test]
    fn predecessor_order_duplicates_dead_and_unused_blocks_match_cranelift() {
        for duplicate in [false, true] {
            let function = fixture(duplicate);
            let ledger = Ledger::new(4096);
            let graph = Predecessors::new(&function, BudgetAllocator(ledger.clone())).unwrap();
            let reference = ControlFlowGraph::with_function(&function);
            for block in reference.blocks() {
                assert_eq!(
                    graph.pred_iter(block).collect::<std::vec::Vec<_>>(),
                    reference.pred_iter(block).collect::<std::vec::Vec<_>>()
                );
            }
            assert_eq!(graph.edges.capacity(), 7);
            assert_eq!(ledger.current(), 7 * size_of::<Edge>());
            assert_eq!(graph.edges.len(), if duplicate { 4 } else { 5 });
            drop(graph);
            assert_eq!(ledger.current(), 0);
        }
    }

    #[test]
    fn exact_quota_and_underlying_allocation_refusals_release_all_storage() {
        let function = fixture(false);
        for underlying in [false, true] {
            let ledger = Ledger::new(if underlying {
                4096
            } else {
                7 * size_of::<Edge>() - 1
            });
            if underlying {
                ledger.fail_after(0);
            }
            assert!(matches!(
                Predecessors::new(&function, BudgetAllocator(ledger.clone())),
                Err(JitError::ResourceLimit("frontend predecessor graph"))
            ));
            assert_eq!(ledger.current(), 0);
            assert_eq!(crate::jit::resources::LedgerRef::strong_count(&ledger), 1);
        }
    }

    #[test]
    fn empty_graph_needs_no_allocation_or_charge() {
        let ledger = Ledger::new(0);
        let graph = Predecessors::new(&Function::new(), BudgetAllocator(ledger.clone())).unwrap();
        assert_eq!(graph.edges.capacity(), 0);
        assert_eq!(ledger.current(), 0);
    }

    #[test]
    fn malformed_destination_refusal_releases_reserved_storage() {
        let mut function = fixture(false);
        let target = Block::from_u32(function.dfg.num_blocks() as u32 + 1);
        let destination = BlockCall::new(target, [], &mut function.dfg.value_lists);
        let block = function.layout.entry_block().unwrap();
        let inst = function.layout.last_inst(block).unwrap();
        function.dfg.insts[inst] = cranelift_codegen::ir::InstructionData::Jump {
            opcode: cranelift_codegen::ir::Opcode::Jump,
            destination,
        };
        let ledger = Ledger::new(4096);
        assert!(matches!(
            Predecessors::new(&function, BudgetAllocator(ledger.clone())),
            Err(JitError::Compilation(message)) if message == "invalid predecessor target"
        ));
        assert_eq!(ledger.current(), 0);
        assert!(ledger.peak() > 0);
    }

    #[test]
    fn seeded_cycles_tables_and_creation_layout_orders_match_cranelift() {
        let mut state = 0u64;
        for _ in 0..64 {
            let mut function = Function::new();
            let blocks: [_; 9] = std::array::from_fn(|_| function.dfg.make_block());
            let layout = [
                blocks[0], blocks[7], blocks[2], blocks[8], blocks[3], blocks[1], blocks[5],
                blocks[6],
            ];
            for block in layout {
                function.layout.append_block(block);
            }
            let mut next = || {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                (state >> 32) as usize
            };
            for block in layout {
                let mut cursor = FuncCursor::new(&mut function);
                cursor.goto_bottom(block);
                let value = cursor.ins().iconst(types::I32, 1);
                let left = layout[next() % layout.len()];
                let right = layout[next() % layout.len()];
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
                        let table = cursor.func.dfg.jump_tables.push(JumpTableData::new(
                            default,
                            &[other, default, other, default],
                        ));
                        cursor.ins().br_table(value, table);
                    }
                }
            }
            let ledger = Ledger::new(8192);
            let graph = Predecessors::new(&function, BudgetAllocator(ledger.clone())).unwrap();
            let reference = ControlFlowGraph::with_function(&function);
            for block in reference.blocks() {
                assert_eq!(
                    graph.pred_iter(block).collect::<std::vec::Vec<_>>(),
                    reference.pred_iter(block).collect::<std::vec::Vec<_>>()
                );
            }
            assert_eq!(ledger.current(), graph.edges.capacity() * size_of::<Edge>());
            drop(graph);
            assert_eq!(ledger.current(), 0);
        }
    }
}
