use super::*;
use cranelift_codegen::ir::{
    AbiParam, ExtFuncData, ExternalName, FuncRef, SigRef, Type, UserExternalName,
    UserExternalNameRef, ValueDef,
};
use cranelift_codegen::isa::CallConv;
use std::ops::Range;

struct ValueRecord {
    value: IrValue,
    ty: Type,
    definition: ValueDef,
    resolved: IrValue,
}
struct Target {
    block: Block,
    values: Range<usize>,
}
struct Node {
    inst: Inst,
    data: InstructionData,
    args: Range<usize>,
    results: Range<usize>,
    targets: Range<usize>,
    flags: Option<MemFlagsData>,
}
struct BlockRecord {
    block: Block,
    params: Range<usize>,
    nodes: Range<usize>,
}
struct SignatureRecord {
    id: SigRef,
    convention: CallConv,
    params: Range<usize>,
    returns: Range<usize>,
}

pub(super) struct Witness {
    blocks: BudgetVec<BlockRecord, BudgetAllocator>,
    nodes: BudgetVec<Node, BudgetAllocator>,
    values: BudgetVec<ValueRecord, BudgetAllocator>,
    targets: BudgetVec<Target, BudgetAllocator>,
    imports: BudgetVec<(FuncRef, ExtFuncData), BudgetAllocator>,
    names: BudgetVec<(UserExternalNameRef, UserExternalName), BudgetAllocator>,
    signatures: BudgetVec<SignatureRecord, BudgetAllocator>,
    abi: BudgetVec<AbiParam, BudgetAllocator>,
    params: [AbiParam; 5],
    convention: CallConv,
}

fn push<T>(values: &mut BudgetVec<T, BudgetAllocator>, value: T) -> Result<(), CompileError> {
    values
        .try_reserve(1)
        .map_err(|_| CompileError::ResourceLimit("read-only preservation witness"))?;
    values.push(value);
    Ok(())
}

impl Witness {
    pub(super) fn new(
        function: &Function,
        allocator: BudgetAllocator,
    ) -> Result<Self, CompileError> {
        let params: [AbiParam; 5] = function
            .signature
            .params
            .as_slice()
            .try_into()
            .map_err(|_| invalid())?;
        if !function.signature.returns.is_empty() {
            return Err(invalid());
        }
        let mut this = Self {
            blocks: BudgetVec::new_in(allocator.clone()),
            nodes: BudgetVec::new_in(allocator.clone()),
            values: BudgetVec::new_in(allocator.clone()),
            targets: BudgetVec::new_in(allocator.clone()),
            imports: BudgetVec::new_in(allocator.clone()),
            names: BudgetVec::new_in(allocator.clone()),
            signatures: BudgetVec::new_in(allocator.clone()),
            abi: BudgetVec::new_in(allocator),
            params,
            convention: function.signature.call_conv,
        };
        for block in function.layout.blocks() {
            let params =
                this.record_values(function, function.dfg.block_params(block).iter().copied())?;
            let start = this.nodes.len();
            for inst in function.layout.block_insts(block) {
                let args =
                    this.record_values(function, function.dfg.inst_args(inst).iter().copied())?;
                let results =
                    this.record_values(function, function.dfg.inst_results(inst).iter().copied())?;
                let target_start = this.targets.len();
                for target in function.dfg.insts[inst]
                    .branch_destination(&function.dfg.jump_tables, &function.dfg.exception_tables)
                {
                    if target
                        .args(&function.dfg.value_lists)
                        .any(|arg| !matches!(arg, BlockArg::Value(_)))
                    {
                        return Err(invalid());
                    }
                    let values = this.record_values(
                        function,
                        target.args(&function.dfg.value_lists).map(|arg| match arg {
                            BlockArg::Value(value) => value,
                            _ => unreachable!(),
                        }),
                    )?;
                    push(
                        &mut this.targets,
                        Target {
                            block: target.block(&function.dfg.value_lists),
                            values,
                        },
                    )?;
                }
                let node = Node {
                    inst,
                    data: function.dfg.insts[inst].clone(),
                    args,
                    results,
                    targets: target_start..this.targets.len(),
                    flags: function.dfg.insts[inst].memflags_data(&function.dfg),
                };
                push(&mut this.nodes, node)?;
            }
            let end = this.nodes.len();
            push(
                &mut this.blocks,
                BlockRecord {
                    block,
                    params,
                    nodes: start..end,
                },
            )?;
        }
        for (id, data) in function.dfg.ext_funcs.iter() {
            if !matches!(data.name, ExternalName::User(_)) {
                return Err(invalid());
            }
            push(&mut this.imports, (id, data.clone()))?;
        }
        for (id, name) in function.params.user_named_funcs().iter() {
            push(&mut this.names, (id, name.clone()))?;
        }
        for (id, signature) in function.dfg.signatures.iter() {
            let start = this.abi.len();
            for &param in &signature.params {
                push(&mut this.abi, param)?;
            }
            let middle = this.abi.len();
            for &param in &signature.returns {
                push(&mut this.abi, param)?;
            }
            let end = this.abi.len();
            push(
                &mut this.signatures,
                SignatureRecord {
                    id,
                    convention: signature.call_conv,
                    params: start..middle,
                    returns: middle..end,
                },
            )?;
        }
        Ok(this)
    }

    fn record_values(
        &mut self,
        function: &Function,
        values: impl Iterator<Item = IrValue>,
    ) -> Result<Range<usize>, CompileError> {
        let start = self.values.len();
        for value in values {
            push(
                &mut self.values,
                ValueRecord {
                    value,
                    ty: function.dfg.value_type(value),
                    definition: function.dfg.value_def(value),
                    resolved: function.dfg.resolve_aliases(value),
                },
            )?;
        }
        Ok(start..self.values.len())
    }

    fn same_values(
        &self,
        function: &Function,
        range: Range<usize>,
        values: impl Iterator<Item = IrValue>,
    ) -> bool {
        let mut expected = self.values[range].iter();
        for value in values {
            let Some(old) = expected.next() else {
                return false;
            };
            if old.value != value
                || old.ty != function.dfg.value_type(value)
                || old.definition != function.dfg.value_def(value)
                || old.resolved != function.dfg.resolve_aliases(value)
            {
                return false;
            }
        }
        expected.next().is_none()
    }

    pub(super) fn verify(
        &self,
        function: &Function,
        rewrite: &Rewrite,
    ) -> Result<(), CompileError> {
        if function.signature.params[..5] != self.params
            || function.signature.call_conv != self.convention
            || !function.signature.returns.is_empty()
        {
            return Err(invalid());
        }
        let mut blocks = self.blocks.iter();
        for block in function.layout.blocks() {
            if [rewrite.root, rewrite.check, rewrite.refused].contains(&block) {
                continue;
            }
            let original = blocks.next().ok_or_else(invalid)?;
            if original.block != block
                || !self.same_values(
                    function,
                    original.params.clone(),
                    function.dfg.block_params(block).iter().copied(),
                )
            {
                return Err(invalid());
            }
            let mut nodes = self.nodes[original.nodes.clone()].iter();
            for inst in function.layout.block_insts(block) {
                if rewrite.body.contains(&inst) {
                    continue;
                }
                let old = nodes.next().ok_or_else(invalid)?;
                if old.inst != inst
                    || !self.same_values(
                        function,
                        old.results.clone(),
                        function.dfg.inst_results(inst).iter().copied(),
                    )
                {
                    return Err(invalid());
                }
                if inst == rewrite.call {
                    continue;
                }
                if old.data != function.dfg.insts[inst]
                    || old.flags != function.dfg.insts[inst].memflags_data(&function.dfg)
                    || !self.same_values(
                        function,
                        old.args.clone(),
                        function.dfg.inst_args(inst).iter().copied(),
                    )
                {
                    return Err(invalid());
                }
                let mut targets = self.targets[old.targets.clone()].iter();
                for target in function.dfg.insts[inst]
                    .branch_destination(&function.dfg.jump_tables, &function.dfg.exception_tables)
                {
                    let old = targets.next().ok_or_else(invalid)?;
                    if old.block != target.block(&function.dfg.value_lists)
                        || target
                            .args(&function.dfg.value_lists)
                            .any(|arg| !matches!(arg, BlockArg::Value(_)))
                        || !self.same_values(
                            function,
                            old.values.clone(),
                            target.args(&function.dfg.value_lists).map(|arg| match arg {
                                BlockArg::Value(value) => value,
                                _ => unreachable!(),
                            }),
                        )
                    {
                        return Err(invalid());
                    }
                }
                if targets.next().is_some() {
                    return Err(invalid());
                }
            }
            if nodes.next().is_some() {
                return Err(invalid());
            }
        }
        if blocks.next().is_some()
            || function
                .dfg
                .ext_funcs
                .iter()
                .ne(self.imports.iter().map(|(id, data)| (*id, data)))
            || function
                .params
                .user_named_funcs()
                .iter()
                .ne(self.names.iter().map(|(id, name)| (*id, name)))
            || function.dfg.signatures.len() != self.signatures.len()
        {
            return Err(invalid());
        }
        for old in &self.signatures {
            let signature = &function.dfg.signatures[old.id];
            if signature.call_conv != old.convention
                || signature.params != self.abi[old.params.clone()]
                || signature.returns != self.abi[old.returns.clone()]
            {
                return Err(invalid());
            }
        }
        let block = function
            .layout
            .inst_block(rewrite.call)
            .ok_or_else(invalid)?;
        let mut nodes = function
            .layout
            .block_insts(block)
            .skip_while(|inst| *inst != rewrite.body[0]);
        if nodes.by_ref().take(6).ne(rewrite.body) || nodes.next() != Some(rewrite.call) {
            return Err(invalid());
        }
        Ok(())
    }
}
