use std::{
    io,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};

use allocator_api2::vec::Vec as BudgetVec;
use cranelift_codegen::ir::{
    condcodes::{FloatCC, IntCC},
    types, AbiParam, Block, Inst, InstBuilder, MemFlagsData, Value as IrValue,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Switch};
use cranelift_jit::{
    BranchProtection, JITBuilder, JITMemoryKind, JITMemoryProvider, JITModule, SystemMemoryProvider,
};
use cranelift_module::{default_libcall_names, Linkage, Module, ModuleResult};

use super::{
    abi::{self, Entry, Exit, Slot},
    exits::Kind as ExitKind,
    helpers,
    ir::Snapshot,
    resources::BudgetAllocator,
    JitError,
};
use crate::opcode::{Operation, RCIndex};

struct Memory {
    allocations: BudgetVec<(SystemMemoryProvider, usize), BudgetAllocator>,
    total: Arc<AtomicUsize>,
    quota_refused: Arc<AtomicBool>,
    metadata_refused: Arc<AtomicBool>,
    unavailable: Arc<AtomicBool>,
    #[cfg(test)]
    failure: Failure,
    limit: usize,
    page: usize,
}

impl Memory {
    fn release(&mut self) {
        for (mut provider, bytes) in self.allocations.drain(..) {
            unsafe { provider.free_memory() };
            self.total.fetch_sub(bytes, Ordering::Relaxed);
        }
        self.allocations = BudgetVec::new_in(self.allocations.allocator().clone());
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        self.release();
    }
}

impl JITMemoryProvider for Memory {
    fn allocate(&mut self, size: usize, align: u64, kind: JITMemoryKind) -> io::Result<*mut u8> {
        let bytes = size
            .checked_add(self.page - 1)
            .map(|size| size / self.page * self.page)
            .ok_or_else(|| {
                self.quota_refused.store(true, Ordering::Relaxed);
                io::Error::other("native allocation size overflow")
            })?;
        self.allocations.try_reserve_exact(1).map_err(|_| {
            self.metadata_refused.store(true, Ordering::Relaxed);
            io::Error::other("native allocation record quota exhausted")
        })?;
        self.total
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|next| *next <= self.limit)
            })
            .map_err(|_| {
                self.quota_refused.store(true, Ordering::Relaxed);
                io::Error::other("native memory quota exhausted")
            })?;
        #[cfg(test)]
        if self.failure == Failure::Allocate {
            self.total.fetch_sub(bytes, Ordering::Relaxed);
            self.unavailable.store(true, Ordering::Relaxed);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected native allocation denial",
            ));
        }
        let mut provider = SystemMemoryProvider::new();
        match provider.allocate(size, align, kind) {
            Ok(pointer) => {
                self.allocations.push((provider, bytes));
                Ok(pointer)
            }
            Err(error) => {
                unsafe { provider.free_memory() };
                self.total.fetch_sub(bytes, Ordering::Relaxed);
                self.unavailable.store(true, Ordering::Relaxed);
                Err(error)
            }
        }
    }

    unsafe fn free_memory(&mut self) {
        self.release();
    }

    fn finalize(&mut self, protection: BranchProtection) -> ModuleResult<()> {
        #[cfg(test)]
        if self.failure == Failure::Protect {
            self.unavailable.store(true, Ordering::Relaxed);
            return Err(cranelift_module::ModuleError::Backend(anyhow::anyhow!(
                "injected native protection denial"
            )));
        }
        for (provider, _) in &mut self.allocations {
            if let Err(error) = provider.finalize(protection) {
                self.unavailable.store(true, Ordering::Relaxed);
                return Err(error);
            }
        }
        Ok(())
    }
}

pub(super) struct Code {
    module: Option<JITModule>,
    entry: Entry,
    #[cfg(test)]
    byte_len: usize,
    pub registers: usize,
    pub entries: BudgetVec<bool, BudgetAllocator>,
}

impl Drop for Code {
    fn drop(&mut self) {
        if let Some(module) = self.module.take() {
            unsafe { module.free_memory() };
        }
    }
}

impl Code {
    #[cfg(test)]
    pub fn invoke(&self, slots: &mut [Slot], pc: usize, budget: u32) -> Exit {
        unsafe { self.invoke_host(slots, pc, budget, std::ptr::null_mut()) }
    }

    /// Invokes a pinned module using scalar scratch slots and an opaque helper host.
    ///
    /// # Safety
    /// `host` must be null or carry a live, exclusively borrowed helper frame for this call.
    pub unsafe fn invoke_host(
        &self,
        slots: &mut [Slot],
        pc: usize,
        budget: u32,
        host: *mut abi::Host,
    ) -> Exit {
        assert!(slots.len() >= self.registers);
        let mut exit = Exit::default();
        unsafe {
            (self.entry)(
                slots.as_mut_ptr(),
                pc as u64,
                budget.min(64),
                &mut exit,
                host,
            )
        };
        exit
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Failure {
    #[default]
    None,
    Allocate,
    Protect,
    CorruptTag,
    OmitNumericGuards,
    CorruptFloatSelector,
    CorruptFloatPayload,
    CorruptArithmeticOpcode,
    CorruptArithmeticOperands,
    CorruptArithmeticSource,
    CorruptArithmeticDestination,
    CorruptTruthPayload,
    CorruptTruthSource,
    CorruptTruthCondition,
    CorruptTruthTargets,
    CorruptTruthCount,
    CorruptComparisonSame,
    CorruptComparisonGuard,
    CorruptComparisonBound,
    CorruptComparisonSplit,
    CorruptComparisonPhi,
    CorruptComparisonTargets,
    CorruptComparisonCount,
    CorruptComparisonSource,
    CorruptComparisonPolarity,
    CorruptLoop(bool, super::tags::LoopCorruption),
    CorruptTransfer(super::tags::TransferCorruption),
    CorruptHelperFlow(super::helper_flow::Fault),
    CorruptExitFlow(super::exit_flow::Fault),
}

#[cfg(test)]
pub(super) fn compile(
    snapshot: &Snapshot,
    total: Arc<AtomicUsize>,
    limit: usize,
) -> Result<Code, JitError> {
    compile_in(
        snapshot,
        total,
        limit,
        BudgetAllocator(super::resources::Ledger::new(2 * 1024 * 1024)),
        super::work::Limits::from(&super::JitConfig::default()),
        Failure::None,
    )
}

pub(super) fn compile_in(
    snapshot: &Snapshot,
    total: Arc<AtomicUsize>,
    limit: usize,
    metadata: BudgetAllocator,
    work: super::work::Limits,
    #[cfg(test)] failure: Failure,
) -> Result<Code, JitError> {
    let expansion = super::work::Expansion::admit(snapshot, work)?;
    let graph = super::flow::FlowGraph::new(snapshot)?;
    let mut stores = super::tags::Stores::new(&graph, snapshot)?;
    let mut blocks = BudgetVec::new_in(snapshot.operations.allocator().clone());
    blocks
        .try_reserve_exact(snapshot.operations.len())
        .map_err(|_| JitError::ResourceLimit("frontend block map"))?;
    let mut entries = BudgetVec::new_in(metadata.clone());
    entries
        .try_reserve_exact(snapshot.operations.len())
        .map_err(|_| JitError::ResourceLimit("JIT metadata"))?;
    entries.extend(graph.nodes.iter().map(|node| node.lowering.native()));
    let quota_refused = Arc::new(AtomicBool::new(false));
    let metadata_refused = Arc::new(AtomicBool::new(false));
    let unavailable = Arc::new(AtomicBool::new(false));
    let fail = |error: cranelift_module::ModuleError| {
        if metadata_refused.load(Ordering::Relaxed) {
            JitError::ResourceLimit("JIT metadata")
        } else if quota_refused.load(Ordering::Relaxed) {
            JitError::ResourceLimit("native mappings")
        } else if unavailable.load(Ordering::Relaxed) {
            JitError::Unavailable("native memory allocation or protection denied")
        } else {
            JitError::Compilation(error.to_string())
        }
    };
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return Err(JitError::Unavailable(
            "cannot determine native memory page size",
        ));
    }
    let mut jit = JITBuilder::with_flags(
        &[("opt_level", "speed"), ("enable_verifier", "true")],
        default_libcall_names(),
    )
    .map_err(fail)?;
    for (_, name, entry) in helpers::SYMBOLS {
        jit.symbol(name, entry as *const u8);
    }
    jit.memory_provider(Box::new(Memory {
        allocations: BudgetVec::new_in(metadata),
        total,
        quota_refused: quota_refused.clone(),
        metadata_refused: metadata_refused.clone(),
        unavailable: unavailable.clone(),
        #[cfg(test)]
        failure,
        limit,
        page: page as usize,
    }));
    let mut module = JITModule::new(jit);
    let ptr = module.target_config().pointer_type();
    let mut signature = module.make_signature();
    for ty in [ptr, types::I64, types::I32, ptr, ptr] {
        signature.params.push(AbiParam::new(ty));
    }
    let function = module
        .declare_function("luna_slice_v3", Linkage::Local, &signature)
        .map_err(fail)?;
    let mut helper_signature = module.make_signature();
    for ty in [ptr, ptr, types::I32, types::I32, types::I32, types::I32] {
        helper_signature.params.push(AbiParam::new(ty));
    }
    helper_signature.returns.push(AbiParam::new(types::I32));
    let helper_ids = helpers::SYMBOLS
        .iter()
        .map(|(kind, name, _)| {
            module
                .declare_function(name, Linkage::Import, &helper_signature)
                .map(|id| (*kind, id))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(fail)?;
    let mut context = module.make_context();
    context.func.signature = signature;
    let mut fb_context = FunctionBuilderContext::new();
    let (fallback, guard, exhausted, panicked, helper_refs);
    {
        let mut builder = FunctionBuilder::new(&mut context.func, &mut fb_context);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let arguments = builder.block_params(entry).to_vec();
        blocks.extend((0..snapshot.operations.len()).map(|_| builder.create_block()));
        for block in &blocks {
            builder.append_block_param(*block, types::I32);
        }
        fallback = builder.create_block();
        guard = builder.create_block();
        exhausted = builder.create_block();
        panicked = builder.create_block();
        for block in [fallback, guard, exhausted, panicked] {
            builder.append_block_param(block, types::I64);
            builder.append_block_param(block, types::I32);
        }
        let mut switch = Switch::new();
        let mut entries = Vec::new();
        for (pc, block) in blocks.iter().enumerate() {
            let trampoline = builder.create_block();
            switch.set_entry(pc as u128, trampoline);
            entries.push((trampoline, *block));
        }
        let unknown = builder.create_block();
        switch.emit(&mut builder, arguments[1], unknown);
        for (trampoline, block) in entries {
            builder.switch_to_block(trampoline);
            let zero = builder.ins().iconst(types::I32, 0);
            builder.ins().jump(block, &[zero.into()]);
        }
        builder.switch_to_block(unknown);
        let zero = builder.ins().iconst(types::I32, 0);
        builder
            .ins()
            .jump(fallback, &[arguments[1].into(), zero.into()]);
        {
            helper_refs = std::array::from_fn::<_, { helpers::SYMBOLS.len() }, _>(|index| {
                let (kind, id) = helper_ids[index];
                (kind, module.declare_func_in_func(id, builder.func))
            });
            let mut emitter = Emitter {
                builder: &mut builder,
                snapshot,
                graph: &graph,
                blocks: &blocks,
                slots: arguments[0],
                fallback,
                guard,
                panicked,
                host: arguments[4],
                helpers: &helper_refs,
                count: zero,
                pc: 0,
                written: false,
                stores: &mut stores,
                #[cfg(test)]
                omit_numeric_guards: failure == Failure::OmitNumericGuards,
            };
            for (pc, op) in snapshot.operations.iter().copied().enumerate() {
                emitter.pc = pc;
                emitter.written = false;
                emitter.builder.switch_to_block(blocks[pc]);
                emitter.count = emitter.builder.block_params(blocks[pc])[0];
                let pc_value = emitter.exit_pc(ExitKind::Budget);
                let limit = emitter.builder.ins().icmp(
                    IntCC::UnsignedGreaterThanOrEqual,
                    emitter.count,
                    arguments[2],
                );
                let body = emitter.builder.create_block();
                emitter.builder.ins().brif(
                    limit,
                    exhausted,
                    &[pc_value.into(), emitter.count.into()],
                    body,
                    &[],
                );
                emitter.builder.switch_to_block(body);
                emitter.emit(op);
            }
        }
        for (block, reason) in [
            (fallback, ExitKind::Interpreter),
            (guard, ExitKind::Guard),
            (exhausted, ExitKind::Budget),
            (panicked, ExitKind::Panic),
        ] {
            builder.switch_to_block(block);
            let pc = builder.block_params(block)[0];
            let count = builder.block_params(block)[1];
            let reason = builder.ins().iconst(types::I32, i64::from(reason as u32));
            builder
                .ins()
                .store(MemFlagsData::new(), pc, arguments[3], 0);
            builder
                .ins()
                .store(MemFlagsData::new(), count, arguments[3], 8);
            builder
                .ins()
                .store(MemFlagsData::new(), reason, arguments[3], 12);
            builder.ins().return_(&[]);
        }
        builder.seal_all_blocks();
        builder.finalize(module.target_config());
    }
    #[cfg(test)]
    if failure == Failure::CorruptTag {
        stores.corrupt_first(&mut context.func);
    }
    #[cfg(test)]
    if matches!(
        failure,
        Failure::CorruptFloatSelector | Failure::CorruptFloatPayload
    ) {
        stores.corrupt_float_first(&mut context.func, failure == Failure::CorruptFloatPayload);
    }
    #[cfg(test)]
    {
        use super::tags::ArithmeticCorruption as Fault;
        let fault = match failure {
            Failure::CorruptArithmeticOpcode => Some(Fault::Opcode),
            Failure::CorruptArithmeticOperands => Some(Fault::Operands),
            Failure::CorruptArithmeticSource => Some(Fault::Source),
            Failure::CorruptArithmeticDestination => Some(Fault::Destination),
            _ => None,
        };
        if let Some(fault) = fault {
            stores.corrupt_arithmetic_first(&mut context.func, fault, snapshot.registers);
        }
    }
    #[cfg(test)]
    {
        use super::tags::TruthCorruption as Fault;
        let fault = match failure {
            Failure::CorruptTruthPayload => Some(Fault::Payload),
            Failure::CorruptTruthSource => Some(Fault::Source),
            Failure::CorruptTruthCondition => Some(Fault::Condition),
            Failure::CorruptTruthTargets => Some(Fault::Targets),
            Failure::CorruptTruthCount => Some(Fault::Count),
            _ => None,
        };
        if let Some(fault) = fault {
            stores.corrupt_truth(&mut context.func, fault, snapshot.registers);
        }
    }
    #[cfg(test)]
    {
        use super::tags::ComparisonCorruption as Fault;
        let fault = match failure {
            Failure::CorruptComparisonSame => Some(Fault::SameCondition),
            Failure::CorruptComparisonGuard => Some(Fault::MixedGuard),
            Failure::CorruptComparisonBound => Some(Fault::MixedBound),
            Failure::CorruptComparisonSplit => Some(Fault::Split),
            Failure::CorruptComparisonPhi => Some(Fault::Phi),
            Failure::CorruptComparisonTargets => Some(Fault::Targets),
            Failure::CorruptComparisonCount => Some(Fault::Count),
            Failure::CorruptComparisonSource => Some(Fault::Source),
            Failure::CorruptComparisonPolarity => Some(Fault::Polarity),
            _ => None,
        };
        if let Some(fault) = fault {
            stores.corrupt_comparison(&mut context.func, fault, snapshot.registers);
        }
    }
    #[cfg(test)]
    if let Failure::CorruptLoop(prep, fault) = failure {
        stores.corrupt_loop(&mut context.func, prep, fault, snapshot.registers);
    }
    #[cfg(test)]
    if let Failure::CorruptTransfer(fault) = failure {
        stores.corrupt_transfer(&mut context.func, fault, snapshot.registers);
    }
    let block_count = context.func.layout.blocks().count();
    let instructions = context
        .func
        .layout
        .blocks()
        .map(|block| context.func.layout.block_insts(block).count())
        .sum();
    expansion.verify_actual(instructions, block_count)?;
    let parameters: [IrValue; 5] = context
        .func
        .dfg
        .block_params(context.func.layout.entry_block().unwrap())
        .try_into()
        .map_err(|_| JitError::Compilation("invalid native entry parameters".into()))?;
    stores.verify(
        &context.func,
        parameters[0],
        Some(parameters[3]),
        snapshot.registers,
    )?;
    stores.verify_arithmetic(&context.func, parameters[0], snapshot)?;
    stores.verify_truths(&context.func, parameters[0], snapshot, &blocks)?;
    stores.verify_comparisons(&context.func, parameters[0], snapshot, &blocks)?;
    stores.verify_loops(
        &context.func,
        parameters[0],
        snapshot,
        &blocks,
        fallback,
        guard,
    )?;
    stores.verify_transfers(&context.func, parameters[0], snapshot, &blocks, fallback)?;
    #[cfg(test)]
    if let Failure::CorruptHelperFlow(fault) = failure {
        stores
            .helper_calls
            .corrupt(&mut context.func, fault, &helper_refs);
    }
    stores.helper_calls.verify(
        &context.func,
        snapshot,
        super::helper_flow::Boundary {
            slots: parameters[0],
            host: parameters[4],
            blocks: &blocks,
            imports: &helper_refs,
            fallback,
            panicked,
        },
    )?;
    let exit_handlers = [fallback, guard, exhausted, panicked];
    #[cfg(test)]
    let mut exit_handlers = exit_handlers;
    #[cfg(test)]
    if let Failure::CorruptExitFlow(fault) = failure {
        super::exit_flow::corrupt(&mut context.func, &mut exit_handlers, fault);
    }
    super::exit_flow::verify(&context.func, &exit_handlers)?;
    module
        .define_function(function, &mut context)
        .map_err(fail)?;
    #[cfg(test)]
    let byte_len = context.compiled_code().unwrap().code_buffer().len();
    module.finalize_definitions().map_err(fail)?;
    let entry =
        unsafe { std::mem::transmute::<*const u8, Entry>(module.get_finalized_function(function)) };
    Ok(Code {
        module: Some(module),
        entry,
        #[cfg(test)]
        byte_len,
        registers: snapshot.registers,
        entries,
    })
}

struct Emitter<'a, 'b> {
    builder: &'a mut FunctionBuilder<'b>,
    snapshot: &'a Snapshot,
    graph: &'a super::flow::FlowGraph,
    blocks: &'a [Block],
    slots: IrValue,
    fallback: Block,
    guard: Block,
    panicked: Block,
    host: IrValue,
    helpers: &'a [(u32, cranelift_codegen::ir::FuncRef)],
    pc: usize,
    count: IrValue,
    written: bool,
    stores: &'a mut super::tags::Stores,
    #[cfg(test)]
    omit_numeric_guards: bool,
}

impl Emitter<'_, '_> {
    fn exit_state(&self, kind: ExitKind) -> super::exits::State {
        let state = self.graph.nodes[self.pc]
            .exit
            .state(kind, self.written)
            .expect("invalid native exit snapshot");
        assert_eq!(
            usize::from(state.materialized_slots),
            self.snapshot.registers
        );
        assert_eq!(state.resume_pc as usize, self.pc);
        assert_eq!(
            state.frame_pc as usize,
            self.pc + usize::from(kind == ExitKind::Panic)
        );
        state
    }

    fn exit_pc(&mut self, kind: ExitKind) -> IrValue {
        let state = self.exit_state(kind);
        self.constant(u64::from(state.resume_pc))
    }

    fn helper(&mut self, kind: u32, a: u32, b: u32, c: u32) {
        assert!(self.graph.nodes[self.pc].lowering.accepts_helper(kind));
        assert!(
            self.graph.nodes[self.pc]
                .access
                .permits_helper(kind, a, b, c),
            "invalid native helper operands"
        );
        self.exit_state(ExitKind::Interpreter);
        self.exit_state(ExitKind::Panic);
        let args: Vec<_> = [a, b, c, self.pc as u32]
            .into_iter()
            .map(|arg| self.builder.ins().iconst(types::I32, i64::from(arg)))
            .collect();
        let helper = self
            .helpers
            .iter()
            .find_map(|(key, helper)| (*key == kind).then_some(*helper))
            .expect("missing native helper symbol");
        let call = self.builder.ins().call(
            helper,
            &[self.host, self.slots, args[0], args[1], args[2], args[3]],
        );
        let status = self.builder.inst_results(call)[0];
        let completed =
            self.builder
                .ins()
                .icmp_imm_u(IntCC::Equal, status, i64::from(abi::HELPER_COMPLETED));
        let success = self.builder.create_block();
        let declined = self.builder.create_block();
        let completed = self
            .builder
            .ins()
            .brif(completed, success, &[], declined, &[]);
        self.builder.switch_to_block(success);
        let success = self.advance_point(self.pc + 1);
        self.builder.switch_to_block(declined);
        let panic =
            self.builder
                .ins()
                .icmp_imm_u(IntCC::Equal, status, i64::from(abi::HELPER_PANICKED));
        let pc = self.exit_pc(ExitKind::Interpreter);
        let declined = self.builder.ins().brif(
            panic,
            self.panicked,
            &[pc.into(), self.count.into()],
            self.fallback,
            &[pc.into(), self.count.into()],
        );
        self.stores.helper_calls.record(super::helper_flow::Record {
            pc: self.pc,
            call,
            completed,
            success,
            declined,
        });
    }

    fn operand_index(operand: RCIndex) -> u32 {
        match operand {
            RCIndex::Register(index) => u32::from(index.0),
            RCIndex::Constant(index) => abi::CONSTANT_OPERAND | u32::from(index.0),
        }
    }
    fn constant(&mut self, value: u64) -> IrValue {
        self.builder.ins().iconst(types::I64, value as i64)
    }

    fn load(&mut self, register: u8) -> (IrValue, IrValue) {
        assert!(
            self.graph.nodes[self.pc].lowering.native(),
            "invalid native lowering"
        );
        assert!(
            self.graph.nodes[self.pc].access.reads.contains(register),
            "invalid native register read"
        );
        let offset = i32::from(register) * 16;
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), self.slots, offset);
        let bits = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), self.slots, offset + 8);
        (tag, bits)
    }

    fn operand(&mut self, operand: RCIndex) -> (IrValue, IrValue) {
        match operand {
            RCIndex::Register(reg) => self.load(reg.0),
            RCIndex::Constant(index) => {
                let slot = self.snapshot.constants[usize::from(index.0)];
                (self.constant(slot.tag), self.constant(slot.bits))
            }
        }
    }

    fn store(&mut self, register: u8, tag: IrValue, bits: IrValue) -> Inst {
        assert!(
            self.graph.nodes[self.pc].lowering.native(),
            "invalid native lowering"
        );
        assert!(
            self.graph.nodes[self.pc].access.writes.contains(register),
            "invalid native register write"
        );
        self.written = true;
        let inst = self.builder.ins().store(
            MemFlagsData::new(),
            tag,
            self.slots,
            i32::from(register) * 16,
        );
        self.stores
            .record(inst, self.graph.nodes[self.pc].access.scalar_tags());
        self.builder.ins().store(
            MemFlagsData::new(),
            bits,
            self.slots,
            i32::from(register) * 16 + 8,
        )
    }

    fn store_typed(&mut self, register: u8, tag: u64, bits: IrValue) -> Inst {
        assert!(
            self.graph.nodes[self.pc].access.permits_scalar_tag(tag),
            "invalid native result tag"
        );
        let tag = self.constant(tag);
        self.store(register, tag, bits)
    }

    fn require(&mut self, condition: IrValue) {
        self.require_point(condition);
    }

    fn require_point(&mut self, condition: IrValue) -> Inst {
        let next = self.builder.create_block();
        let pc = self.exit_pc(ExitKind::Guard);
        let point = self.builder.ins().brif(
            condition,
            next,
            &[],
            self.guard,
            &[pc.into(), self.count.into()],
        );
        self.builder.switch_to_block(next);
        point
    }

    fn tag_is(&mut self, tag: IrValue, value: u64) -> IrValue {
        self.builder
            .ins()
            .icmp_imm_s(IntCC::Equal, tag, value as i64)
    }

    fn require_numeric(&mut self, tag: IrValue) {
        #[cfg(test)]
        if self.omit_numeric_guards {
            return;
        }
        let int = self.tag_is(tag, abi::INTEGER);
        let float = self.tag_is(tag, abi::NUMBER);
        let numeric = self.builder.ins().bor(int, float);
        self.require(numeric);
    }

    fn as_float(&mut self, tag: IrValue, bits: IrValue) -> IrValue {
        let integer = self.tag_is(tag, abi::INTEGER);
        let converted = self.builder.ins().fcvt_from_sint(types::F64, bits);
        let float = self
            .builder
            .ins()
            .bitcast(types::F64, MemFlagsData::new(), bits);
        let result = self.builder.ins().select(integer, converted, float);
        self.stores.float_input(
            self.builder.func.dfg.value_def(result).unwrap_inst(),
            tag,
            bits,
        );
        result
    }

    fn numeric_input(&mut self, point: IrValue, tag: IrValue, bits: IrValue) {
        self.stores.numeric_input(
            self.builder.func.dfg.value_def(point).unwrap_inst(),
            tag,
            bits,
        );
    }

    fn advance(&mut self, next: usize) {
        self.advance_point(next);
    }

    fn advance_point(&mut self, next: usize) -> Inst {
        assert!(self.graph.permits_edge(self.pc, next));
        let count = self.builder.ins().iadd_imm_s(self.count, 1);
        if let Some(block) = self.blocks.get(next) {
            self.builder.ins().jump(*block, &[count.into()])
        } else {
            let pc = self.constant(next as u64);
            self.builder
                .ins()
                .jump(self.fallback, &[pc.into(), count.into()])
        }
    }

    fn branch(&mut self, condition: IrValue, yes: usize, no: usize) -> Inst {
        assert!(self.graph.permits_edge(self.pc, yes));
        assert!(self.graph.permits_edge(self.pc, no));
        let count = self.builder.ins().iadd_imm_s(self.count, 1);
        self.builder.ins().brif(
            condition,
            self.blocks[yes],
            &[count.into()],
            self.blocks[no],
            &[count.into()],
        )
    }

    fn bail(&mut self) {
        let pc = self.exit_pc(ExitKind::Interpreter);
        self.builder
            .ins()
            .jump(self.fallback, &[pc.into(), self.count.into()]);
    }

    fn truth(&mut self, tag: IrValue, bits: IrValue) -> IrValue {
        let nil = self.tag_is(tag, abi::NIL);
        let boolean = self.tag_is(tag, abi::BOOLEAN);
        let zero = self.builder.ins().icmp_imm_s(IntCC::Equal, bits, 0);
        let false_bool = self.builder.ins().band(boolean, zero);
        let false_value = self.builder.ins().bor(nil, false_bool);
        self.builder.ins().bxor_imm_u(false_value, 1)
    }

    fn emit(&mut self, op: Operation) {
        use Operation::*;
        match op {
            Move { dest, source } => {
                let (tag, bits) = self.load(source.0);
                let scalar =
                    self.builder
                        .ins()
                        .icmp_imm_s(IntCC::NotEqual, tag, abi::REFERENCE as i64);
                let direct = self.builder.create_block();
                let reference = self.builder.create_block();
                let split = self.builder.ins().brif(scalar, direct, &[], reference, &[]);
                self.builder.switch_to_block(reference);
                let available = self.builder.ins().icmp_imm_s(IntCC::NotEqual, self.host, 0);
                self.require(available);
                self.helper(abi::HELPER_MOVE, u32::from(dest.0), u32::from(source.0), 0);
                self.builder.switch_to_block(direct);
                let store = self.store(dest.0, tag, bits);
                self.stores.transfer_write(self.pc, 0, store);
                let next = self.advance_point(self.pc + 1);
                self.stores.transfer_edge(self.pc, next, Some(split));
            }
            LoadConstant { dest, constant } => {
                let slot = self.snapshot.constants[usize::from(constant.0)];
                if slot.tag == abi::REFERENCE {
                    self.helper(
                        abi::HELPER_CONSTANT,
                        u32::from(dest.0),
                        u32::from(constant.0),
                        0,
                    );
                    return;
                }
                let bits = self.constant(slot.bits);
                let store = self.store_typed(dest.0, slot.tag, bits);
                self.stores.transfer_write(self.pc, 0, store);
                let next = self.advance_point(self.pc + 1);
                self.stores.transfer_edge(self.pc, next, None);
            }
            LoadBool {
                dest,
                value,
                skip_next,
            } => {
                let bits = self.constant(u64::from(value));
                let store = self.store_typed(dest.0, abi::BOOLEAN, bits);
                self.stores.transfer_write(self.pc, 0, store);
                let next = self.advance_point(self.pc + 1 + usize::from(skip_next));
                self.stores.transfer_edge(self.pc, next, None);
            }
            LoadNil { dest, count } => {
                let zero = self.constant(0);
                for index in 0..count {
                    let store = self.store_typed(dest.0 + index, abi::NIL, zero);
                    self.stores
                        .transfer_write(self.pc, usize::from(index), store);
                }
                let next = self.advance_point(self.pc + 1);
                self.stores.transfer_edge(self.pc, next, None);
            }
            Jump {
                offset,
                close_upvalues,
            } if close_upvalues.is_none() => {
                let next = self.advance_point(
                    (self.pc + 1)
                        .checked_add_signed(isize::from(offset))
                        .unwrap(),
                );
                self.stores.transfer_edge(self.pc, next, None);
            }
            Test { value, is_true } => {
                let (tag, bits) = self.load(value.0);
                let truth = self.truth(tag, bits);
                let condition = if is_true {
                    truth
                } else {
                    self.builder.ins().bxor_imm_u(truth, 1)
                };
                let point = self.branch(condition, self.pc + 2, self.pc + 1);
                self.stores.truth(self.pc, tag, bits, point);
            }
            Not { dest, source } => {
                let (tag, payload) = self.load(source.0);
                let truth = self.truth(tag, payload);
                let opposite = self.builder.ins().bxor_imm_u(truth, 1);
                let bits = self.builder.ins().uextend(types::I64, opposite);
                let point = self.store_typed(dest.0, abi::BOOLEAN, bits);
                self.stores.truth(self.pc, tag, payload, point);
                self.advance(self.pc + 1);
            }
            Add { dest, left, right }
            | Sub { dest, left, right }
            | Mul { dest, left, right }
            | Div { dest, left, right } => {
                self.arithmetic(op, dest.0, left, right);
            }
            NumericForPrep { base, jump } => self.for_prep(base.0, jump),
            NumericForLoop { base, jump } => self.for_loop(base.0, jump),
            NewTable {
                dest,
                array_size,
                map_size,
            } => self.helper(
                abi::HELPER_NEW_TABLE,
                u32::from(dest.0),
                u32::from(array_size),
                u32::from(map_size),
            ),
            GetTable { dest, table, key } => self.helper(
                abi::HELPER_GET_TABLE,
                u32::from(dest.0),
                u32::from(table.0),
                Self::operand_index(key),
            ),
            SetTable { table, key, value } => self.helper(
                abi::HELPER_SET_TABLE,
                u32::from(table.0),
                Self::operand_index(key),
                Self::operand_index(value),
            ),
            GetUpTable { dest, table, key } => self.helper(
                abi::HELPER_GET_UP_TABLE,
                u32::from(dest.0),
                u32::from(table.0),
                Self::operand_index(key),
            ),
            SetUpTable { table, key, value } => self.helper(
                abi::HELPER_SET_UP_TABLE,
                u32::from(table.0),
                Self::operand_index(key),
                Self::operand_index(value),
            ),
            GetUpValue { dest, source } => self.helper(
                abi::HELPER_GET_UPVALUE,
                u32::from(dest.0),
                u32::from(source.0),
                0,
            ),
            SetUpValue { dest, source } => self.helper(
                abi::HELPER_SET_UPVALUE,
                u32::from(dest.0),
                u32::from(source.0),
                0,
            ),
            Eq {
                skip_if,
                left,
                right,
            }
            | Less {
                skip_if,
                left,
                right,
            }
            | LessEq {
                skip_if,
                left,
                right,
            } => self.compare(op, skip_if, left, right),
            SetList { .. }
            | Call { .. }
            | TailCall { .. }
            | Return { .. }
            | VarArgs { .. }
            | MarkToBeClosed { .. }
            | Jump { .. }
            | TestSet { .. }
            | Closure { .. }
            | GenericForCall { .. }
            | GenericForLoop { .. }
            | Method { .. }
            | Concat { .. }
            | Length { .. }
            | Minus { .. }
            | IDiv { .. }
            | Mod { .. }
            | Pow { .. }
            | BitAnd { .. }
            | BitOr { .. }
            | BitXor { .. }
            | ShiftLeft { .. }
            | ShiftRight { .. }
            | BitNot { .. } => self.bail(),
        }
    }

    fn arithmetic(&mut self, op: Operation, dest: u8, left: RCIndex, right: RCIndex) {
        let (lt, lb) = self.operand(left);
        let (rt, rb) = self.operand(right);
        self.require_numeric(lt);
        self.require_numeric(rt);
        let float = self.builder.create_block();
        if matches!(op, Operation::Div { .. }) {
            self.builder.ins().jump(float, &[]);
        } else {
            let integer = self.builder.create_block();
            let li = self.tag_is(lt, abi::INTEGER);
            let ri = self.tag_is(rt, abi::INTEGER);
            let both = self.builder.ins().band(li, ri);
            self.builder.ins().brif(both, integer, &[], float, &[]);
            self.builder.switch_to_block(integer);
            let bits = match op {
                Operation::Add { .. } => self.builder.ins().iadd(lb, rb),
                Operation::Sub { .. } => self.builder.ins().isub(lb, rb),
                _ => self.builder.ins().imul(lb, rb),
            };
            self.numeric_input(bits, lt, lb);
            self.numeric_input(bits, rt, rb);
            let store = self.store_typed(dest, abi::INTEGER, bits);
            self.stores
                .arithmetic(self.pc, bits, [(lt, lb), (rt, rb)], store, false);
            self.advance(self.pc + 1);
        }
        self.builder.switch_to_block(float);
        let left = self.as_float(lt, lb);
        let right = self.as_float(rt, rb);
        let value = match op {
            Operation::Add { .. } => self.builder.ins().fadd(left, right),
            Operation::Sub { .. } => self.builder.ins().fsub(left, right),
            Operation::Mul { .. } => self.builder.ins().fmul(left, right),
            _ => self.builder.ins().fdiv(left, right),
        };
        let bits = self
            .builder
            .ins()
            .bitcast(types::I64, MemFlagsData::new(), value);
        let store = self.store_typed(dest, abi::NUMBER, bits);
        self.stores
            .arithmetic(self.pc, value, [(lt, lb), (rt, rb)], store, true);
        self.advance(self.pc + 1);
    }

    fn compare(&mut self, op: Operation, skip_if: bool, left: RCIndex, right: RCIndex) {
        let (lt, lb) = self.operand(left);
        let (rt, rb) = self.operand(right);
        self.require_numeric(lt);
        self.require_numeric(rt);
        let same = self.builder.ins().icmp(IntCC::Equal, lt, rt);
        let same_type = self.builder.create_block();
        let mixed = self.builder.create_block();
        let join = self.builder.create_block();
        self.builder.append_block_param(join, types::I8);
        let split = self.builder.ins().brif(same, same_type, &[], mixed, &[]);
        self.builder.switch_to_block(same_type);
        let integer = self.tag_is(lt, abi::INTEGER);
        let (icc, fcc) = match op {
            Operation::Eq { .. } => (IntCC::Equal, FloatCC::Equal),
            Operation::Less { .. } => (IntCC::SignedLessThan, FloatCC::LessThan),
            _ => (IntCC::SignedLessThanOrEqual, FloatCC::LessThanOrEqual),
        };
        let int_result = self.builder.ins().icmp(icc, lb, rb);
        let left = self
            .builder
            .ins()
            .bitcast(types::F64, MemFlagsData::new(), lb);
        let right = self
            .builder
            .ins()
            .bitcast(types::F64, MemFlagsData::new(), rb);
        let float_result = self.builder.ins().fcmp(fcc, left, right);
        let result = self.builder.ins().select(integer, int_result, float_result);
        let same_result = result;
        self.numeric_input(result, lt, lb);
        self.numeric_input(result, rt, rb);
        self.builder.ins().jump(join, &[result.into()]);
        self.builder.switch_to_block(mixed);
        let result = self.mixed_compare(op, lt, lb, rb);
        let mixed_result = result;
        self.numeric_input(result, lt, lb);
        self.numeric_input(result, rt, rb);
        self.builder.ins().jump(join, &[result.into()]);
        self.builder.switch_to_block(join);
        let result = self.builder.block_params(join)[0];
        let skip = if skip_if {
            result
        } else {
            self.builder.ins().bxor_imm_u(result, 1)
        };
        let branch = self.branch(skip, self.pc + 2, self.pc + 1);
        self.stores.comparison(super::tags::Comparison {
            pc: self.pc,
            inputs: [(lt, lb), (rt, rb)],
            same: same_result,
            mixed: mixed_result,
            split,
            branch,
        });
    }

    fn mixed_compare(
        &mut self,
        op: Operation,
        left_tag: IrValue,
        lb: IrValue,
        rb: IrValue,
    ) -> IrValue {
        let left_integer = self.tag_is(left_tag, abi::INTEGER);
        let integer = self.builder.ins().select(left_integer, lb, rb);
        let bits = self.builder.ins().select(left_integer, rb, lb);
        let float = self
            .builder
            .ins()
            .bitcast(types::F64, MemFlagsData::new(), bits);
        let truncated = self.builder.ins().fcvt_to_sint_sat(types::I64, float);
        let integral = self.builder.ins().fcvt_from_sint(types::F64, truncated);
        let tie = self.builder.ins().icmp(IntCC::Equal, integer, truncated);
        let upper = self.builder.ins().f64const(9_223_372_036_854_775_808.0);
        let below_upper = self.builder.ins().fcmp(FloatCC::LessThan, float, upper);
        if matches!(op, Operation::Eq { .. }) {
            let whole = self.builder.ins().fcmp(FloatCC::Equal, float, integral);
            let equal = self.builder.ins().band(tie, whole);
            return self.builder.ins().band(equal, below_upper);
        }
        let int_less = self
            .builder
            .ins()
            .icmp(IntCC::SignedLessThan, integer, truncated);
        let fraction_above = self
            .builder
            .ins()
            .fcmp(FloatCC::GreaterThan, float, integral);
        let tie_less = self.builder.ins().band(tie, fraction_above);
        let less = self.builder.ins().bor(int_less, tie_less);
        let at_upper = self
            .builder
            .ins()
            .fcmp(FloatCC::GreaterThanOrEqual, float, upper);
        let less = self.builder.ins().bor(less, at_upper);
        let int_greater = self
            .builder
            .ins()
            .icmp(IntCC::SignedGreaterThan, integer, truncated);
        let fraction_below = self.builder.ins().fcmp(FloatCC::LessThan, float, integral);
        let tie_greater = self.builder.ins().band(tie, fraction_below);
        let greater = self.builder.ins().bor(int_greater, tie_greater);
        let lower = self.builder.ins().f64const(-9_223_372_036_854_775_808.0);
        let below_lower = self.builder.ins().fcmp(FloatCC::LessThan, float, lower);
        let greater = self.builder.ins().bor(greater, below_lower);
        let forward = self.builder.ins().select(left_integer, less, greater);
        let result = if matches!(op, Operation::LessEq { .. }) {
            let whole = self.builder.ins().fcmp(FloatCC::Equal, float, integral);
            let equal = self.builder.ins().band(tie, whole);
            let equal = self.builder.ins().band(equal, below_upper);
            self.builder.ins().bor(forward, equal)
        } else {
            forward
        };
        let ordered = self.builder.ins().fcmp(FloatCC::Ordered, float, float);
        self.builder.ins().band(result, ordered)
    }

    fn for_prep(&mut self, base: u8, jump: i16) {
        let (it, ib) = self.load(base);
        let (st, sb) = self.load(base + 2);
        self.require_numeric(it);
        self.require_numeric(st);
        let step = self.as_float(st, sb);
        let zero = self.builder.ins().f64const(0.0);
        let nonzero = self.builder.ins().fcmp(FloatCC::NotEqual, step, zero);
        let nonzero = self.require_point(nonzero);
        let integer = self.builder.create_block();
        let float = self.builder.create_block();
        let ii = self.tag_is(it, abi::INTEGER);
        let si = self.tag_is(st, abi::INTEGER);
        let both = self.builder.ins().band(ii, si);
        let split = self.builder.ins().brif(both, integer, &[], float, &[]);
        let target = (self.pc + 1).checked_add_signed(isize::from(jump)).unwrap();
        self.builder.switch_to_block(integer);
        let index = self.builder.ins().isub(ib, sb);
        self.numeric_input(index, it, ib);
        self.numeric_input(index, st, sb);
        let integer_store = self.store_typed(base, abi::INTEGER, index);
        let integer_next = self.advance_point(target);
        self.builder.switch_to_block(float);
        let index = self.as_float(it, ib);
        let index = self.builder.ins().fsub(index, step);
        let bits = self
            .builder
            .ins()
            .bitcast(types::I64, MemFlagsData::new(), index);
        let float_store = self.store_typed(base, abi::NUMBER, bits);
        let float_next = self.advance_point(target);
        self.stores.for_prep(super::tags::ForPrep {
            pc: self.pc,
            inputs: [(it, ib), (st, sb)],
            nonzero,
            split,
            stores: [integer_store, float_store],
            next: [integer_next, float_next],
        });
    }

    fn for_loop(&mut self, base: u8, jump: i16) {
        let (it, ib) = self.load(base);
        let (lt, lb) = self.load(base + 1);
        let (st, sb) = self.load(base + 2);
        for tag in [it, lt, st] {
            self.require_numeric(tag);
        }
        let ii = self.tag_is(it, abi::INTEGER);
        let si = self.tag_is(st, abi::INTEGER);
        let integer = self.builder.create_block();
        let float = self.builder.create_block();
        let both = self.builder.ins().band(ii, si);
        let split = self.builder.ins().brif(both, integer, &[], float, &[]);
        let join = self.builder.create_block();
        self.builder.append_block_param(join, types::I64);
        self.builder.append_block_param(join, types::I64);
        self.builder.append_block_param(join, types::I8);
        self.builder.switch_to_block(integer);
        let (index, overflow) = self.builder.ins().sadd_overflow(ib, sb);
        self.numeric_input(index, it, ib);
        self.numeric_input(index, st, sb);
        let negative = self.builder.ins().icmp_imm_s(IntCC::SignedLessThan, sb, 0);
        let ge = self
            .builder
            .ins()
            .icmp(IntCC::SignedGreaterThanOrEqual, index, lb);
        let le = self
            .builder
            .ins()
            .icmp(IntCC::SignedLessThanOrEqual, index, lb);
        let int_in_range = self.builder.ins().select(negative, ge, le);
        let index_float = self.builder.ins().fcvt_from_sint(types::F64, index);
        let limit_float = self
            .builder
            .ins()
            .bitcast(types::F64, MemFlagsData::new(), lb);
        let ge = self
            .builder
            .ins()
            .fcmp(FloatCC::GreaterThanOrEqual, index_float, limit_float);
        let le = self
            .builder
            .ins()
            .fcmp(FloatCC::LessThanOrEqual, index_float, limit_float);
        let float_in_range = self.builder.ins().select(negative, ge, le);
        let li = self.tag_is(lt, abi::INTEGER);
        let in_range = self.builder.ins().select(li, int_in_range, float_in_range);
        self.numeric_input(in_range, lt, lb);
        let not_overflow = self.builder.ins().bxor_imm_u(overflow, 1);
        let condition = self.builder.ins().band(not_overflow, in_range);
        let tag = self.constant(abi::INTEGER);
        let integer_next = self
            .builder
            .ins()
            .jump(join, &[tag.into(), index.into(), condition.into()]);
        let integer_arm = super::tags::LoopArm {
            tag,
            bits: index,
            condition,
            next: integer_next,
        };
        self.builder.switch_to_block(float);
        let index = self.as_float(it, ib);
        let step = self.as_float(st, sb);
        let limit = self.as_float(lt, lb);
        let index = self.builder.ins().fadd(index, step);
        let zero = self.builder.ins().f64const(0.0);
        let negative = self.builder.ins().fcmp(FloatCC::LessThan, step, zero);
        let ge = self
            .builder
            .ins()
            .fcmp(FloatCC::GreaterThanOrEqual, index, limit);
        let le = self
            .builder
            .ins()
            .fcmp(FloatCC::LessThanOrEqual, index, limit);
        let condition = self.builder.ins().select(negative, ge, le);
        let bits = self
            .builder
            .ins()
            .bitcast(types::I64, MemFlagsData::new(), index);
        let tag = self.constant(abi::NUMBER);
        let float_next = self
            .builder
            .ins()
            .jump(join, &[tag.into(), bits.into(), condition.into()]);
        let float_arm = super::tags::LoopArm {
            tag,
            bits,
            condition,
            next: float_next,
        };
        self.builder.switch_to_block(join);
        let tag = self.builder.block_params(join)[0];
        let bits = self.builder.block_params(join)[1];
        let condition = self.builder.block_params(join)[2];
        let store = self.store(base, tag, bits);
        let taken = self.builder.create_block();
        let done = self.builder.create_block();
        let branch = self.builder.ins().brif(condition, taken, &[], done, &[]);
        self.builder.switch_to_block(taken);
        let visible_store = self.store(base + 3, tag, bits);
        let taken_next =
            self.advance_point((self.pc + 1).checked_add_signed(isize::from(jump)).unwrap());
        self.builder.switch_to_block(done);
        let done_next = self.advance_point(self.pc + 1);
        self.stores.for_loop(super::tags::ForLoop {
            pc: self.pc,
            inputs: [(it, ib), (lt, lb), (st, sb)],
            split,
            arms: [integer_arm, float_arm],
            store,
            branch,
            visible_store,
            next: [taken_next, done_next],
        });
    }
}

#[cfg(test)]
mod exit_tests {
    use super::*;
    use crate::types::{RegisterIndex, VarCount};

    pub(super) fn with_emitter(op: Operation, action: impl FnOnce(&mut Emitter<'_, '_>)) {
        let snapshot = Snapshot {
            operations: super::super::resources::owned(&[
                op,
                Operation::Return {
                    start: RegisterIndex(0),
                    count: VarCount::constant(0),
                },
            ]),
            constants: super::super::resources::owned(&[]),
            registers: 4,
            upvalues: 1,
            prototypes: 0,
        };
        let graph = super::super::flow::FlowGraph::new(&snapshot).unwrap();
        let mut stores = super::super::tags::Stores::new(&graph, &snapshot).unwrap();
        let mut function = cranelift_codegen::ir::Function::new();
        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut function, &mut context);
        let entry = builder.create_block();
        builder.switch_to_block(entry);
        let slots = builder.ins().iconst(types::I64, 0);
        let count = builder.ins().iconst(types::I32, 0);
        let blocks = [entry, entry];
        let mut emitter = Emitter {
            builder: &mut builder,
            snapshot: &snapshot,
            graph: &graph,
            blocks: &blocks,
            slots,
            fallback: entry,
            guard: entry,
            panicked: entry,
            host: slots,
            helpers: &[],
            pc: 0,
            count,
            written: false,
            stores: &mut stores,
            omit_numeric_guards: false,
        };
        action(&mut emitter);
    }

    fn rejects_after_store(op: Operation, kind: ExitKind) {
        with_emitter(op, |emitter| {
            emitter.exit_state(kind);
            let tag = emitter.constant(abi::INTEGER);
            let bits = emitter.constant(42);
            emitter.store(0, tag, bits);
            let condition = emitter.builder.ins().iconst(types::I8, 1);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match kind {
                ExitKind::Guard => emitter.require(condition),
                ExitKind::Interpreter if matches!(op, Operation::NewTable { .. }) => {
                    emitter.helper(abi::HELPER_NEW_TABLE, 0, 0, 0);
                }
                ExitKind::Interpreter => emitter.bail(),
                ExitKind::Budget => {
                    emitter.exit_pc(ExitKind::Budget);
                }
                ExitKind::Panic => unreachable!(),
            }));
            let payload = result.expect_err("emitter admitted a retry exit after scalar effects");
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap();
            assert!(
                message.contains("invalid native exit snapshot"),
                "{message}"
            );
        });
    }

    #[test]
    fn scalar_guard_after_a_store_is_rejected_before_branch_emission() {
        rejects_after_store(
            Operation::Add {
                dest: RegisterIndex(0),
                left: RegisterIndex(1).into(),
                right: RegisterIndex(2).into(),
            },
            ExitKind::Guard,
        );
    }

    #[test]
    fn interpreter_fallback_after_a_store_is_rejected_before_branch_emission() {
        rejects_after_store(
            Operation::Move {
                dest: RegisterIndex(0),
                source: RegisterIndex(1),
            },
            ExitKind::Interpreter,
        );
    }

    #[test]
    fn helper_decline_after_a_store_is_rejected_before_helper_emission() {
        rejects_after_store(
            Operation::NewTable {
                dest: RegisterIndex(0),
                array_size: 0,
                map_size: 0,
            },
            ExitKind::Interpreter,
        );
    }

    #[test]
    fn budget_exit_after_a_store_is_rejected_before_pc_emission() {
        rejects_after_store(
            Operation::LoadBool {
                dest: RegisterIndex(0),
                value: true,
                skip_next: false,
            },
            ExitKind::Budget,
        );
    }
}

#[cfg(test)]
mod access_tests {
    use super::*;
    use crate::types::{RegisterIndex as R, VarCount};

    fn rejects(op: Operation, expected: &str, action: impl FnOnce(&mut Emitter<'_, '_>)) {
        super::exit_tests::with_emitter(op, |emitter| {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| action(emitter)));
            let payload = result.expect_err("emitter admitted an undeclared access");
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap();
            assert!(message.contains(expected), "{message}");
        });
    }

    #[test]
    fn interpreter_barrier_masks_do_not_grant_native_lowering() {
        rejects(
            Operation::Return {
                start: R(0),
                count: VarCount::constant(0),
            },
            "invalid native lowering",
            |emitter| {
                emitter.load(0);
            },
        );
    }

    #[test]
    fn undeclared_in_bounds_register_read_is_rejected_before_load_emission() {
        rejects(
            Operation::Add {
                dest: R(0),
                left: R(1).into(),
                right: R(2).into(),
            },
            "invalid native register read",
            |emitter| {
                emitter.load(3);
            },
        );
    }

    #[test]
    fn undeclared_in_bounds_register_write_is_rejected_before_store_emission() {
        rejects(
            Operation::LoadBool {
                dest: R(0),
                value: true,
                skip_next: false,
            },
            "invalid native register write",
            |emitter| {
                let tag = emitter.constant(abi::BOOLEAN);
                let bits = emitter.constant(1);
                emitter.store(1, tag, bits);
            },
        );
    }

    #[test]
    fn a_known_scalar_result_cannot_change_the_opcode_output_type() {
        rejects(
            Operation::LoadBool {
                dest: R(0),
                value: true,
                skip_next: false,
            },
            "invalid native result tag",
            |emitter| {
                let bits = emitter.constant(1);
                emitter.store_typed(0, abi::INTEGER, bits);
            },
        );
    }

    #[test]
    fn reference_results_cannot_bypass_the_canonical_helper_path() {
        rejects(
            Operation::NewTable {
                dest: R(0),
                array_size: 0,
                map_size: 0,
            },
            "invalid native result tag",
            |emitter| {
                let bits = emitter.constant(0);
                emitter.store_typed(0, abi::REFERENCE, bits);
            },
        );
    }

    #[test]
    fn helper_register_operands_cannot_be_reencoded_as_constants() {
        rejects(
            Operation::GetTable {
                dest: R(0),
                table: R(1),
                key: R(2).into(),
            },
            "invalid native helper operands",
            |emitter| {
                emitter.helper(abi::HELPER_GET_TABLE, 0, 1, abi::CONSTANT_OPERAND | 2);
            },
        );
    }
}

#[cfg(test)]
mod comparison_tests {
    use super::super::tags::ComparisonCorruption as Fault;
    use super::*;
    use crate::types::{ConstantIndex8 as C, RegisterIndex as R, VarCount};

    fn fixture(
        kind: usize,
        skip_if: bool,
        constants: bool,
        fault: Option<Fault>,
    ) -> Result<(), JitError> {
        let left = if constants {
            RCIndex::Constant(C(0))
        } else {
            RCIndex::Register(R(0))
        };
        let right = if constants {
            RCIndex::Constant(C(1))
        } else {
            RCIndex::Register(R(1))
        };
        let op = match kind {
            0 => Operation::Eq {
                left,
                right,
                skip_if,
            },
            1 => Operation::Less {
                left,
                right,
                skip_if,
            },
            _ => Operation::LessEq {
                left,
                right,
                skip_if,
            },
        };
        let snapshot = Snapshot {
            operations: super::super::resources::owned(&[
                op,
                Operation::Return {
                    start: R(0),
                    count: VarCount::constant(0),
                },
                Operation::Return {
                    start: R(0),
                    count: VarCount::constant(0),
                },
            ]),
            constants: super::super::resources::owned(&[
                Slot {
                    tag: abi::INTEGER,
                    bits: i64::MAX as u64,
                },
                Slot {
                    tag: abi::NUMBER,
                    bits: f64::NAN.to_bits(),
                },
            ]),
            registers: 4,
            upvalues: 0,
            prototypes: 0,
        };
        snapshot.verify().unwrap();
        let graph = super::super::flow::FlowGraph::new(&snapshot).unwrap();
        let baseline = snapshot.operations.allocator().0.current();
        let mut stores = super::super::tags::Stores::new(&graph, &snapshot).unwrap();
        let mut function = cranelift_codegen::ir::Function::new();
        function.signature.params.push(AbiParam::new(types::I64));
        let mut context = FunctionBuilderContext::new();
        let slots;
        let blocks;
        {
            let mut builder = FunctionBuilder::new(&mut function, &mut context);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            slots = builder.block_params(entry)[0];
            blocks = [
                builder.create_block(),
                builder.create_block(),
                builder.create_block(),
            ];
            for block in blocks {
                builder.append_block_param(block, types::I32);
            }
            let guard = builder.create_block();
            builder.append_block_param(guard, types::I64);
            builder.append_block_param(guard, types::I32);
            let zero = builder.ins().iconst(types::I32, 0);
            builder.ins().jump(blocks[0], &[zero.into()]);
            builder.switch_to_block(blocks[0]);
            let count = builder.block_params(blocks[0])[0];
            let host = builder.ins().iconst(types::I64, 0);
            let mut emitter = Emitter {
                builder: &mut builder,
                snapshot: &snapshot,
                graph: &graph,
                blocks: &blocks,
                slots,
                fallback: guard,
                guard,
                panicked: guard,
                host,
                helpers: &[],
                pc: 0,
                count,
                written: false,
                stores: &mut stores,
                omit_numeric_guards: false,
            };
            emitter.emit(op);
            for block in [blocks[1], blocks[2], guard] {
                builder.switch_to_block(block);
                builder.ins().return_(&[]);
            }
            builder.seal_all_blocks();
            let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
                .unwrap()
                .finish(cranelift_codegen::settings::Flags::new(
                    cranelift_codegen::settings::builder(),
                ))
                .unwrap();
            builder.finalize(isa.frontend_config());
        }
        if let Some(fault) = fault {
            stores.corrupt_comparison(&mut function, fault, snapshot.registers);
        }
        cranelift_codegen::verify_function(
            &function,
            &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
        )
        .unwrap();
        let result = stores
            .verify(&function, slots, None, snapshot.registers)
            .and_then(|_| stores.verify_comparisons(&function, slots, &snapshot, &blocks));
        let ledger = snapshot.operations.allocator().0.clone();
        drop(stores);
        assert_eq!(ledger.current(), baseline);
        result
    }

    fn refused(result: Result<(), JitError>) {
        assert!(
            matches!(result, Err(JitError::Compilation(ref message)) if message == "invalid scalar tag data flow"),
            "{result:?}"
        );
    }

    #[test]
    fn all_comparison_kinds_polarities_and_operand_forms_match_source() {
        for kind in 0..3 {
            for skip_if in [false, true] {
                for constants in [false, true] {
                    fixture(kind, skip_if, constants, None).unwrap();
                }
            }
        }
    }
    #[test]
    fn same_type_condition_corruption_is_refused() {
        refused(fixture(1, true, false, Some(Fault::SameCondition)));
    }
    #[test]
    fn mixed_nan_and_upper_bound_guard_corruption_is_refused() {
        for kind in 0..3 {
            refused(fixture(kind, true, false, Some(Fault::MixedGuard)));
        }
    }
    #[test]
    fn mixed_boundary_constant_corruption_is_refused() {
        for kind in 0..3 {
            refused(fixture(kind, true, false, Some(Fault::MixedBound)));
        }
    }
    #[test]
    fn split_phi_and_final_branch_corruption_is_refused() {
        for fault in [Fault::Split, Fault::Phi, Fault::Targets] {
            refused(fixture(1, true, false, Some(fault)));
        }
    }
    #[test]
    fn source_and_fuel_corruption_is_refused() {
        for fault in [Fault::Source, Fault::Count] {
            refused(fixture(1, true, false, Some(fault)));
        }
    }

    #[test]
    fn final_polarity_corruption_is_refused_for_each_operation_and_polarity() {
        for kind in 0..3 {
            for skip_if in [false, true] {
                refused(fixture(kind, skip_if, false, Some(Fault::Polarity)));
            }
        }
    }

    #[test]
    fn comparison_record_count_pc_and_capacity_cannot_remove_obligations() {
        for fault in [Fault::Missing, Fault::ProgramCounter, Fault::Growth] {
            refused(fixture(1, true, false, Some(fault)));
        }
    }
}

#[cfg(test)]
mod loop_tests {
    use super::super::tags::LoopCorruption as Fault;
    use super::*;
    use crate::types::{RegisterIndex as R, VarCount};

    fn fixture(prep: bool, base: u8, jump: i16, fault: Option<Fault>) -> Result<(), JitError> {
        let op = if prep {
            Operation::NumericForPrep {
                base: R(base),
                jump,
            }
        } else {
            Operation::NumericForLoop {
                base: R(base),
                jump,
            }
        };
        let snapshot = Snapshot {
            operations: super::super::resources::owned(&[
                op,
                Operation::Return {
                    start: R(0),
                    count: VarCount::constant(0),
                },
                Operation::Return {
                    start: R(0),
                    count: VarCount::constant(0),
                },
            ]),
            constants: super::super::resources::owned(&[]),
            registers: 8,
            upvalues: 0,
            prototypes: 0,
        };
        snapshot.verify().unwrap();
        let graph = super::super::flow::FlowGraph::new(&snapshot).unwrap();
        let baseline = snapshot.operations.allocator().0.current();
        let mut stores = super::super::tags::Stores::new(&graph, &snapshot).unwrap();
        let mut function = cranelift_codegen::ir::Function::new();
        function.signature.params.push(AbiParam::new(types::I64));
        let mut context = FunctionBuilderContext::new();
        let (slots, blocks, fallback, guard);
        {
            let mut builder = FunctionBuilder::new(&mut function, &mut context);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            slots = builder.block_params(entry)[0];
            blocks = [
                builder.create_block(),
                builder.create_block(),
                builder.create_block(),
            ];
            for block in blocks {
                builder.append_block_param(block, types::I32);
            }
            fallback = builder.create_block();
            guard = builder.create_block();
            for block in [fallback, guard] {
                builder.append_block_param(block, types::I64);
                builder.append_block_param(block, types::I32);
            }
            let zero = builder.ins().iconst(types::I32, 0);
            builder.ins().jump(blocks[0], &[zero.into()]);
            builder.switch_to_block(blocks[0]);
            let count = builder.block_params(blocks[0])[0];
            let host = builder.ins().iconst(types::I64, 0);
            let mut emitter = Emitter {
                builder: &mut builder,
                snapshot: &snapshot,
                graph: &graph,
                blocks: &blocks,
                slots,
                fallback,
                guard,
                panicked: guard,
                host,
                helpers: &[],
                pc: 0,
                count,
                written: false,
                stores: &mut stores,
                omit_numeric_guards: false,
            };
            emitter.emit(op);
            for block in [blocks[1], blocks[2], fallback, guard] {
                builder.switch_to_block(block);
                builder.ins().return_(&[]);
            }
            builder.seal_all_blocks();
            let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
                .unwrap()
                .finish(cranelift_codegen::settings::Flags::new(
                    cranelift_codegen::settings::builder(),
                ))
                .unwrap();
            builder.finalize(isa.frontend_config());
        }
        if let Some(fault) = fault {
            stores.corrupt_loop(&mut function, prep, fault, snapshot.registers);
        }
        cranelift_codegen::verify_function(
            &function,
            &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
        )
        .unwrap();
        let result = stores
            .verify(&function, slots, None, snapshot.registers)
            .and_then(|_| {
                stores.verify_loops(&function, slots, &snapshot, &blocks, fallback, guard)
            });
        let ledger = snapshot.operations.allocator().0.clone();
        drop(stores);
        assert_eq!(ledger.current(), baseline);
        result
    }

    fn refused(result: Result<(), JitError>) {
        assert!(
            matches!(result, Err(JitError::Compilation(ref message)) if message == "invalid scalar tag data flow"),
            "{result:?}"
        );
    }

    #[test]
    fn prep_and_step_match_both_bases_backward_self_and_forward_jumps() {
        for prep in [false, true] {
            for base in [0, 4] {
                for jump in [-1, 0, 1] {
                    fixture(prep, base, jump, None).unwrap();
                }
            }
        }
    }
    #[test]
    fn prep_source_arithmetic_nonzero_guard_split_and_store_corruption_is_refused() {
        for fault in [
            Fault::IndexOpcode,
            Fault::FloatOpcode,
            Fault::Guard,
            Fault::Split,
            Fault::Source,
            Fault::Store,
        ] {
            refused(fixture(true, 0, 1, Some(fault)));
        }
    }
    #[test]
    fn step_overflow_direction_limit_and_arithmetic_corruption_is_refused() {
        for fault in [
            Fault::Overflow,
            Fault::Direction,
            Fault::Limit,
            Fault::IndexOpcode,
            Fault::FloatOpcode,
        ] {
            refused(fixture(false, 0, 1, Some(fault)));
        }
    }
    #[test]
    fn split_and_phi_corruption_is_refused() {
        for fault in [Fault::Split, Fault::Phi] {
            refused(fixture(false, 0, 1, Some(fault)));
        }
    }
    #[test]
    fn index_and_visible_variable_destination_corruption_is_refused() {
        for fault in [Fault::Store, Fault::VisibleStore] {
            refused(fixture(false, 0, 1, Some(fault)));
        }
    }
    #[test]
    fn source_branch_and_fuel_corruption_is_refused() {
        for fault in [Fault::Source, Fault::Targets, Fault::Count] {
            refused(fixture(false, 0, 1, Some(fault)));
        }
    }
    #[test]
    fn prep_target_and_fuel_corruption_is_refused() {
        for fault in [Fault::Targets, Fault::Count] {
            refused(fixture(true, 0, 1, Some(fault)));
        }
    }
    #[test]
    fn record_count_pc_and_capacity_cannot_remove_loop_obligations() {
        for prep in [false, true] {
            for fault in [Fault::Missing, Fault::ProgramCounter, Fault::Growth] {
                refused(fixture(prep, 0, 1, Some(fault)));
            }
        }
    }
}

#[cfg(test)]
mod transfer_tests {
    use super::super::tags::TransferCorruption as Fault;
    use super::*;
    use crate::types::{ConstantIndex16 as C, Opt254, RegisterIndex as R, VarCount};

    fn fixture(op: Operation, slot: Slot, fault: Option<Fault>) -> Result<(), JitError> {
        let snapshot = Snapshot {
            operations: super::super::resources::owned(&[
                op,
                Operation::Return {
                    start: R(0),
                    count: VarCount::constant(0),
                },
                Operation::Return {
                    start: R(0),
                    count: VarCount::constant(0),
                },
            ]),
            constants: super::super::resources::owned(&[slot]),
            registers: 256,
            upvalues: 0,
            prototypes: 0,
        };
        snapshot.verify().unwrap();
        let graph = super::super::flow::FlowGraph::new(&snapshot).unwrap();
        let baseline = snapshot.operations.allocator().0.current();
        let mut stores = super::super::tags::Stores::new(&graph, &snapshot).unwrap();
        let mut function = cranelift_codegen::ir::Function::new();
        function.signature.params.push(AbiParam::new(types::I64));
        let mut helper_signature = function.signature.clone();
        helper_signature.params.clear();
        for ty in [
            types::I64,
            types::I64,
            types::I32,
            types::I32,
            types::I32,
            types::I32,
        ] {
            helper_signature.params.push(AbiParam::new(ty));
        }
        helper_signature.returns.push(AbiParam::new(types::I32));
        let signature = function.import_signature(helper_signature);
        let helper = function.import_function(cranelift_codegen::ir::ExtFuncData {
            name: cranelift_codegen::ir::ExternalName::testcase("transfer_helper"),
            signature,
            colocated: false,
            patchable: false,
        });
        let helpers = [(abi::HELPER_MOVE, helper), (abi::HELPER_CONSTANT, helper)];
        let mut context = FunctionBuilderContext::new();
        let (slots, blocks, fallback, guard);
        {
            let mut builder = FunctionBuilder::new(&mut function, &mut context);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            slots = builder.block_params(entry)[0];
            blocks = [
                builder.create_block(),
                builder.create_block(),
                builder.create_block(),
            ];
            for block in blocks {
                builder.append_block_param(block, types::I32);
            }
            fallback = builder.create_block();
            guard = builder.create_block();
            for block in [fallback, guard] {
                builder.append_block_param(block, types::I64);
                builder.append_block_param(block, types::I32);
            }
            let zero = builder.ins().iconst(types::I32, 0);
            builder.ins().jump(blocks[0], &[zero.into()]);
            builder.switch_to_block(blocks[0]);
            let count = builder.block_params(blocks[0])[0];
            let host = builder.ins().iconst(types::I64, 0);
            let mut emitter = Emitter {
                builder: &mut builder,
                snapshot: &snapshot,
                graph: &graph,
                blocks: &blocks,
                slots,
                fallback,
                guard,
                panicked: guard,
                host,
                helpers: &helpers,
                pc: 0,
                count,
                written: false,
                stores: &mut stores,
                omit_numeric_guards: false,
            };
            emitter.emit(op);
            for block in [blocks[1], blocks[2], fallback, guard] {
                builder.switch_to_block(block);
                builder.ins().return_(&[]);
            }
            builder.seal_all_blocks();
            let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
                .unwrap()
                .finish(cranelift_codegen::settings::Flags::new(
                    cranelift_codegen::settings::builder(),
                ))
                .unwrap();
            builder.finalize(isa.frontend_config());
        }
        if let Some(fault) = fault {
            stores.corrupt_transfer(&mut function, fault, snapshot.registers);
        }
        cranelift_codegen::verify_function(
            &function,
            &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
        )
        .unwrap();
        let result = stores
            .verify(&function, slots, None, snapshot.registers)
            .and_then(|_| stores.verify_transfers(&function, slots, &snapshot, &blocks, fallback));
        let ledger = snapshot.operations.allocator().0.clone();
        drop(stores);
        assert_eq!(ledger.current(), baseline);
        result
    }

    fn run(op: Operation, fault: Option<Fault>) -> Result<(), JitError> {
        fixture(
            op,
            Slot {
                tag: abi::INTEGER,
                bits: 42,
            },
            fault,
        )
    }
    fn refused(result: Result<(), JitError>) {
        assert!(
            matches!(result, Err(JitError::Compilation(ref message)) if message == "invalid scalar tag data flow"),
            "{result:?}"
        );
    }
    fn boolean() -> Operation {
        Operation::LoadBool {
            dest: R(1),
            value: true,
            skip_next: false,
        }
    }

    #[test]
    fn scalar_move_handles_aliases_and_boundary_registers() {
        for (dest, source) in [(0, 0), (0, 255), (255, 0), (255, 255)] {
            run(
                Operation::Move {
                    dest: R(dest),
                    source: R(source),
                },
                None,
            )
            .unwrap();
        }
    }
    #[test]
    fn constants_preserve_tags_and_full_payload_bits() {
        for slot in [
            Slot {
                tag: abi::NIL,
                bits: 0,
            },
            Slot {
                tag: abi::BOOLEAN,
                bits: 0,
            },
            Slot {
                tag: abi::BOOLEAN,
                bits: 1,
            },
            Slot {
                tag: abi::INTEGER,
                bits: u64::MAX,
            },
            Slot {
                tag: abi::NUMBER,
                bits: (-0.0f64).to_bits(),
            },
            Slot {
                tag: abi::NUMBER,
                bits: 0x7ff8_0000_0000_0123,
            },
            Slot {
                tag: abi::REFERENCE,
                bits: 0,
            },
        ] {
            fixture(
                Operation::LoadConstant {
                    dest: R(255),
                    constant: C(0),
                },
                slot,
                None,
            )
            .unwrap();
        }
    }
    #[test]
    fn boolean_skips_nil_ranges_and_jump_targets_match_source() {
        for value in [false, true] {
            for skip_next in [false, true] {
                run(
                    Operation::LoadBool {
                        dest: R(255),
                        value,
                        skip_next,
                    },
                    None,
                )
                .unwrap();
            }
        }
        for (dest, count) in [(255, 0), (255, 1), (0, 2), (1, 255)] {
            run(
                Operation::LoadNil {
                    dest: R(dest),
                    count,
                },
                None,
            )
            .unwrap();
        }
        for offset in [-1, 0, 1] {
            run(
                Operation::Jump {
                    offset,
                    close_upvalues: Opt254::none(),
                },
                None,
            )
            .unwrap();
        }
    }
    #[test]
    fn move_source_payload_destination_and_split_corruption_is_refused() {
        for fault in [
            Fault::Source,
            Fault::Payload,
            Fault::Destination,
            Fault::Split,
        ] {
            refused(run(
                Operation::Move {
                    dest: R(0),
                    source: R(1),
                },
                Some(fault),
            ));
        }
    }
    #[test]
    fn scalar_load_value_destination_and_extra_store_corruption_is_refused() {
        for op in [
            boolean(),
            Operation::LoadConstant {
                dest: R(0),
                constant: C(0),
            },
            Operation::LoadNil {
                dest: R(0),
                count: 2,
            },
        ] {
            for fault in [Fault::Payload, Fault::Destination, Fault::ExtraStore] {
                refused(run(op, Some(fault)));
            }
        }
    }
    #[test]
    fn nil_ordinal_and_range_corruption_is_refused() {
        for fault in [Fault::Ordinal, Fault::Destination] {
            refused(run(
                Operation::LoadNil {
                    dest: R(0),
                    count: 2,
                },
                Some(fault),
            ));
        }
    }
    #[test]
    fn target_and_fuel_corruption_including_empty_nil_is_refused() {
        for op in [
            boolean(),
            Operation::LoadBool {
                dest: R(0),
                value: false,
                skip_next: true,
            },
            Operation::LoadNil {
                dest: R(0),
                count: 0,
            },
            Operation::Jump {
                offset: 1,
                close_upvalues: Opt254::none(),
            },
        ] {
            for fault in [Fault::Targets, Fault::Count] {
                refused(run(op, Some(fault)));
            }
        }
    }
    #[test]
    fn record_count_pc_ordinal_and_capacity_cannot_remove_transfer_obligations() {
        for fault in [
            Fault::MissingWrite,
            Fault::MissingEdge,
            Fault::ProgramCounter,
            Fault::Ordinal,
            Fault::GrowthWrite,
            Fault::GrowthEdge,
        ] {
            refused(run(boolean(), Some(fault)));
        }
    }
}

#[cfg(test)]
mod helper_flow_tests {
    use super::super::helper_flow::{Boundary, Fault};
    use super::*;
    use crate::types::{
        ConstantIndex16 as C16, ConstantIndex8 as C8, RegisterIndex as R, UpValueIndex as U,
        VarCount,
    };

    fn operations() -> [Operation; 9] {
        [
            Operation::Move {
                dest: R(0),
                source: R(1),
            },
            Operation::LoadConstant {
                dest: R(0),
                constant: C16(1),
            },
            Operation::NewTable {
                dest: R(0),
                array_size: 1,
                map_size: 2,
            },
            Operation::GetTable {
                dest: R(0),
                table: R(1),
                key: RCIndex::Constant(C8(0)),
            },
            Operation::SetTable {
                table: R(0),
                key: RCIndex::Constant(C8(0)),
                value: RCIndex::Register(R(3)),
            },
            Operation::GetUpTable {
                dest: R(0),
                table: U(0),
                key: RCIndex::Constant(C8(0)),
            },
            Operation::SetUpTable {
                table: U(0),
                key: RCIndex::Constant(C8(0)),
                value: RCIndex::Register(R(3)),
            },
            Operation::GetUpValue {
                dest: R(0),
                source: U(0),
            },
            Operation::SetUpValue {
                dest: U(0),
                source: R(3),
            },
        ]
    }

    fn fixture(op: Operation, fault: Option<Fault>) -> Result<(), JitError> {
        let snapshot = Snapshot {
            operations: super::super::resources::owned(&[
                op,
                Operation::Return {
                    start: R(0),
                    count: VarCount::constant(0),
                },
            ]),
            constants: super::super::resources::owned(&[
                Slot {
                    tag: abi::INTEGER,
                    bits: 42,
                },
                Slot {
                    tag: abi::REFERENCE,
                    bits: 0,
                },
            ]),
            registers: 4,
            upvalues: 1,
            prototypes: 0,
        };
        snapshot.verify().unwrap();
        let graph = super::super::flow::FlowGraph::new(&snapshot).unwrap();
        let baseline = snapshot.operations.allocator().0.current();
        let mut stores = super::super::tags::Stores::new(&graph, &snapshot).unwrap();
        let mut function = cranelift_codegen::ir::Function::new();
        for ty in [types::I64, types::I64] {
            function.signature.params.push(AbiParam::new(ty));
        }
        let mut helper_signature = function.signature.clone();
        for ty in [types::I32; 4] {
            helper_signature.params.push(AbiParam::new(ty));
        }
        helper_signature.returns.push(AbiParam::new(types::I32));
        let signature = function.import_signature(helper_signature);
        let imports: [_; 9] = std::array::from_fn(|index| {
            let helper = function.import_function(cranelift_codegen::ir::ExtFuncData {
                name: cranelift_codegen::ir::ExternalName::testcase(format!("helper_{index}")),
                signature,
                colocated: false,
                patchable: false,
            });
            (helpers::SYMBOLS[index].0, helper)
        });
        let mut context = FunctionBuilderContext::new();
        let (slots, host, blocks, fallback, panicked);
        {
            let mut builder = FunctionBuilder::new(&mut function, &mut context);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            slots = builder.block_params(entry)[0];
            host = builder.block_params(entry)[1];
            blocks = [builder.create_block(), builder.create_block()];
            for block in blocks {
                builder.append_block_param(block, types::I32);
            }
            fallback = builder.create_block();
            panicked = builder.create_block();
            let guard = builder.create_block();
            for block in [fallback, panicked, guard] {
                builder.append_block_param(block, types::I64);
                builder.append_block_param(block, types::I32);
            }
            let zero = builder.ins().iconst(types::I32, 0);
            builder.ins().jump(blocks[0], &[zero.into()]);
            builder.switch_to_block(blocks[0]);
            let count = builder.block_params(blocks[0])[0];
            let mut emitter = Emitter {
                builder: &mut builder,
                snapshot: &snapshot,
                graph: &graph,
                blocks: &blocks,
                slots,
                fallback,
                guard,
                panicked,
                host,
                helpers: &imports,
                pc: 0,
                count,
                written: false,
                stores: &mut stores,
                omit_numeric_guards: false,
            };
            emitter.emit(op);
            for block in [blocks[1], fallback, panicked, guard] {
                builder.switch_to_block(block);
                builder.ins().return_(&[]);
            }
            builder.seal_all_blocks();
            let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
                .unwrap()
                .finish(cranelift_codegen::settings::Flags::new(
                    cranelift_codegen::settings::builder(),
                ))
                .unwrap();
            builder.finalize(isa.frontend_config());
        }
        stores
            .verify(&function, slots, None, snapshot.registers)
            .unwrap();
        if let Some(fault) = fault {
            stores.helper_calls.corrupt(&mut function, fault, &imports);
        }
        cranelift_codegen::verify_function(
            &function,
            &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
        )
        .unwrap();
        let result = stores.helper_calls.verify(
            &function,
            &snapshot,
            Boundary {
                slots,
                host,
                blocks: &blocks,
                imports: &imports,
                fallback,
                panicked,
            },
        );
        let ledger = snapshot.operations.allocator().0.clone();
        drop(stores);
        assert_eq!(ledger.current(), baseline);
        result
    }
    fn refused(result: Result<(), JitError>) {
        assert!(
            matches!(result, Err(JitError::Compilation(ref message)) if message == "invalid helper call data flow"),
            "{result:?}"
        );
    }

    #[test]
    fn all_nine_helpers_match_source_imports_arguments_and_status_flow() {
        for op in operations() {
            fixture(op, None).unwrap();
        }
    }
    #[test]
    fn symbol_pointer_operand_pc_and_signature_corruption_is_refused() {
        for fault in [
            Fault::Symbol,
            Fault::Pointer,
            Fault::Operand,
            Fault::SourcePc,
            Fault::Signature,
        ] {
            refused(fixture(operations()[2], Some(fault)));
        }
    }
    #[test]
    fn constant_operand_flags_cannot_change_to_register_operands() {
        for index in [4, 6] {
            refused(fixture(operations()[index], Some(Fault::ConstantFlag)));
        }
    }
    #[test]
    fn completed_status_target_and_fuel_corruption_is_refused() {
        for fault in [
            Fault::CompletedTest,
            Fault::SuccessTarget,
            Fault::SuccessCount,
        ] {
            refused(fixture(operations()[2], Some(fault)));
        }
    }
    #[test]
    fn panic_status_and_exit_target_pc_and_count_corruption_is_refused() {
        for fault in [
            Fault::PanicTest,
            Fault::ExitTarget,
            Fault::ExitPc,
            Fault::ExitCount,
        ] {
            refused(fixture(operations()[2], Some(fault)));
        }
    }
    #[test]
    fn extra_call_and_store_side_effects_are_refused() {
        for fault in [Fault::ExtraCall, Fault::ExtraStore] {
            refused(fixture(operations()[2], Some(fault)));
        }
    }
    #[test]
    fn exact_count_pc_and_capacity_cannot_remove_helper_obligations() {
        for fault in [Fault::Missing, Fault::ProgramCounter, Fault::Growth] {
            refused(fixture(operations()[2], Some(fault)));
        }
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;

    fn refuse_corrupted_exit_flow(fault: super::super::exit_flow::Fault) {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype =
                crate::FunctionPrototype::compile(ctx, "exit-flow-corruption", b"return 42")
                    .unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let total = Arc::new(AtomicUsize::new(0));
        let result = compile_in(
            &snapshot,
            total.clone(),
            8 * 1024 * 1024,
            BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            Failure::CorruptExitFlow(fault),
        );
        assert!(
            matches!(result, Err(JitError::Compilation(ref message))
            if message == "invalid shared exit data flow"),
            "{:?}",
            result.as_ref().err()
        );
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }

    macro_rules! exit_flow_corruption {
        ($name:ident, $fault:ident) => {
            #[test]
            fn $name() {
                refuse_corrupted_exit_flow(super::super::exit_flow::Fault::$fault);
            }
        };
    }

    exit_flow_corruption!(
        corrupted_exit_flow_output_branch_is_refused_before_codegen_and_mapping,
        OutputBranch
    );
    exit_flow_corruption!(
        corrupted_exit_flow_pc_width_is_refused_before_codegen_and_mapping,
        PcWidth
    );
    exit_flow_corruption!(
        corrupted_exit_flow_count_width_is_refused_before_codegen_and_mapping,
        CountWidth
    );

    exit_flow_corruption!(
        corrupted_exit_flow_pc_is_refused_before_codegen_and_mapping,
        Pc
    );
    exit_flow_corruption!(
        corrupted_exit_flow_count_is_refused_before_codegen_and_mapping,
        Count
    );
    exit_flow_corruption!(
        corrupted_exit_flow_reason_is_refused_before_codegen_and_mapping,
        Reason
    );
    exit_flow_corruption!(
        corrupted_exit_flow_base_is_refused_before_codegen_and_mapping,
        Base
    );
    exit_flow_corruption!(
        corrupted_exit_flow_offset_is_refused_before_codegen_and_mapping,
        Offset
    );
    exit_flow_corruption!(
        corrupted_exit_flow_flags_is_refused_before_codegen_and_mapping,
        Flags
    );
    exit_flow_corruption!(
        corrupted_exit_flow_extra_store_is_refused_before_codegen_and_mapping,
        ExtraStore
    );
    exit_flow_corruption!(
        corrupted_exit_flow_missing_store_is_refused_before_codegen_and_mapping,
        MissingStore
    );
    exit_flow_corruption!(
        corrupted_exit_flow_store_order_is_refused_before_codegen_and_mapping,
        StoreOrder
    );
    exit_flow_corruption!(
        corrupted_exit_flow_return_is_refused_before_codegen_and_mapping,
        Return
    );
    exit_flow_corruption!(
        corrupted_exit_flow_extra_return_is_refused_before_codegen_and_mapping,
        ExtraReturn
    );
    exit_flow_corruption!(
        corrupted_exit_flow_output_use_is_refused_before_codegen_and_mapping,
        OutputUse
    );
    exit_flow_corruption!(
        corrupted_exit_flow_duplicate_handler_is_refused_before_codegen_and_mapping,
        DuplicateHandler
    );
    exit_flow_corruption!(
        corrupted_exit_flow_signature_is_refused_before_codegen_and_mapping,
        Signature
    );

    #[test]
    fn corrupted_scalar_store_is_refused_before_codegen_and_mapping() {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype =
                crate::FunctionPrototype::compile(ctx, "corrupt-tag", b"return 42").unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let total = Arc::new(AtomicUsize::new(0));
        let result = compile_in(
            &snapshot,
            total.clone(),
            8 * 1024 * 1024,
            BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            Failure::CorruptTag,
        );
        assert!(
            matches!(result, Err(JitError::Compilation(ref message)) if message == "invalid scalar tag data flow")
        );
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn omitted_numeric_guards_are_refused_before_codegen_and_mapping() {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype =
                crate::FunctionPrototype::compile(ctx, "omit-guards", b"local x=40 return x+2")
                    .unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let total = Arc::new(AtomicUsize::new(0));
        let result = compile_in(
            &snapshot,
            total.clone(),
            8 * 1024 * 1024,
            BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            Failure::OmitNumericGuards,
        );
        assert!(
            matches!(result, Err(JitError::Compilation(ref message)) if message == "invalid scalar tag data flow")
        );
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn block_map_quota_refuses_before_host_setup_and_releases_storage() {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype =
                crate::FunctionPrototype::compile(ctx, "block-map-quota", b"return 1").unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let ledger = snapshot.operations.allocator().0.clone();
        let baseline = ledger.current();
        let graph = super::super::flow::FlowGraph::new(&snapshot).unwrap();
        let records = super::super::tags::Stores::new(&graph, &snapshot).unwrap();
        let limit = ledger.current();
        drop(records);
        drop(graph);
        ledger.set_limit(limit);
        let total = Arc::new(AtomicUsize::new(0));
        let result = compile_in(
            &snapshot,
            total.clone(),
            8 * 1024 * 1024,
            BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            Failure::None,
        );
        assert!(
            matches!(result, Err(JitError::ResourceLimit("frontend block map"))),
            "{:?}",
            result.as_ref().err()
        );
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(ledger.current(), baseline);
        assert_eq!(ledger.refusals(), 1);
    }

    fn refuse_corrupted_helper_flow(fault: super::super::helper_flow::Fault) {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype = crate::FunctionPrototype::compile(
                ctx,
                "helper-flow-corruption",
                b"local t={} t.x=42 return t.x",
            )
            .unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let total = Arc::new(AtomicUsize::new(0));
        let result = compile_in(
            &snapshot,
            total.clone(),
            8 * 1024 * 1024,
            BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            Failure::CorruptHelperFlow(fault),
        );
        assert!(
            matches!(result, Err(JitError::Compilation(ref message)) if message == "invalid helper call data flow")
        );
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }

    macro_rules! helper_flow_corruption {
        ($name:ident, $fault:ident) => {
            #[test]
            fn $name() {
                refuse_corrupted_helper_flow(super::super::helper_flow::Fault::$fault);
            }
        };
    }

    helper_flow_corruption!(
        corrupted_helper_flow_symbol_is_refused_before_codegen_and_mapping,
        Symbol
    );
    helper_flow_corruption!(
        corrupted_helper_flow_pointer_is_refused_before_codegen_and_mapping,
        Pointer
    );
    helper_flow_corruption!(
        corrupted_helper_flow_operand_is_refused_before_codegen_and_mapping,
        Operand
    );
    helper_flow_corruption!(
        corrupted_helper_flow_constant_flag_is_refused_before_codegen_and_mapping,
        ConstantFlag
    );
    helper_flow_corruption!(
        corrupted_helper_flow_source_pc_is_refused_before_codegen_and_mapping,
        SourcePc
    );
    helper_flow_corruption!(
        corrupted_helper_flow_signature_is_refused_before_codegen_and_mapping,
        Signature
    );
    helper_flow_corruption!(
        corrupted_helper_flow_completed_test_is_refused_before_codegen_and_mapping,
        CompletedTest
    );
    helper_flow_corruption!(
        corrupted_helper_flow_success_target_is_refused_before_codegen_and_mapping,
        SuccessTarget
    );
    helper_flow_corruption!(
        corrupted_helper_flow_success_count_is_refused_before_codegen_and_mapping,
        SuccessCount
    );
    helper_flow_corruption!(
        corrupted_helper_flow_panic_test_is_refused_before_codegen_and_mapping,
        PanicTest
    );
    helper_flow_corruption!(
        corrupted_helper_flow_exit_target_is_refused_before_codegen_and_mapping,
        ExitTarget
    );
    helper_flow_corruption!(
        corrupted_helper_flow_exit_pc_is_refused_before_codegen_and_mapping,
        ExitPc
    );
    helper_flow_corruption!(
        corrupted_helper_flow_exit_count_is_refused_before_codegen_and_mapping,
        ExitCount
    );
    helper_flow_corruption!(
        corrupted_helper_flow_extra_call_is_refused_before_codegen_and_mapping,
        ExtraCall
    );
    helper_flow_corruption!(
        corrupted_helper_flow_extra_store_is_refused_before_codegen_and_mapping,
        ExtraStore
    );

    fn refuse_corrupted_transfer(fault: super::super::tags::TransferCorruption) {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype = crate::FunctionPrototype::compile(
                ctx,
                "transfer-corruption",
                b"local x=42 local y=7 x=y return x",
            )
            .unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let total = Arc::new(AtomicUsize::new(0));
        let result = compile_in(
            &snapshot,
            total.clone(),
            8 * 1024 * 1024,
            BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            Failure::CorruptTransfer(fault),
        );
        assert!(
            matches!(result, Err(JitError::Compilation(ref message)) if message == "invalid scalar tag data flow")
        );
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }

    macro_rules! transfer_corruption {
        ($name:ident, $fault:ident) => {
            #[test]
            fn $name() {
                refuse_corrupted_transfer(super::super::tags::TransferCorruption::$fault);
            }
        };
    }

    transfer_corruption!(
        corrupted_transfer_payload_is_refused_before_codegen_and_mapping,
        Payload
    );
    transfer_corruption!(
        corrupted_transfer_source_is_refused_before_codegen_and_mapping,
        Source
    );
    transfer_corruption!(
        corrupted_transfer_destination_is_refused_before_codegen_and_mapping,
        Destination
    );
    transfer_corruption!(
        corrupted_transfer_split_is_refused_before_codegen_and_mapping,
        Split
    );
    transfer_corruption!(
        corrupted_transfer_targets_is_refused_before_codegen_and_mapping,
        Targets
    );
    transfer_corruption!(
        corrupted_transfer_count_is_refused_before_codegen_and_mapping,
        Count
    );
    transfer_corruption!(
        corrupted_transfer_pc_is_refused_before_codegen_and_mapping,
        ProgramCounter
    );
    transfer_corruption!(
        corrupted_transfer_ordinal_is_refused_before_codegen_and_mapping,
        Ordinal
    );
    transfer_corruption!(
        corrupted_transfer_extra_is_refused_before_codegen_and_mapping,
        ExtraStore
    );

    fn refuse_corrupted_loop(prep: bool, fault: super::super::tags::LoopCorruption) {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype = crate::FunctionPrototype::compile(
                ctx,
                "loop-corruption",
                b"local total=0 for i=1,3 do total=total+i end return total",
            )
            .unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let total = Arc::new(AtomicUsize::new(0));
        let result = compile_in(
            &snapshot,
            total.clone(),
            8 * 1024 * 1024,
            BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            Failure::CorruptLoop(prep, fault),
        );
        assert!(
            matches!(result, Err(JitError::Compilation(ref message)) if message == "invalid scalar tag data flow")
        );
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }

    macro_rules! loop_corruption {
        ($name:ident, $prep:literal, $fault:ident) => {
            #[test]
            fn $name() {
                refuse_corrupted_loop($prep, super::super::tags::LoopCorruption::$fault);
            }
        };
    }

    loop_corruption!(
        corrupted_loop_prep_index_is_refused_before_codegen_and_mapping,
        true,
        IndexOpcode
    );
    loop_corruption!(
        corrupted_loop_prep_float_is_refused_before_codegen_and_mapping,
        true,
        FloatOpcode
    );
    loop_corruption!(
        corrupted_loop_prep_guard_is_refused_before_codegen_and_mapping,
        true,
        Guard
    );
    loop_corruption!(
        corrupted_loop_prep_split_is_refused_before_codegen_and_mapping,
        true,
        Split
    );
    loop_corruption!(
        corrupted_loop_prep_source_is_refused_before_codegen_and_mapping,
        true,
        Source
    );
    loop_corruption!(
        corrupted_loop_prep_store_is_refused_before_codegen_and_mapping,
        true,
        Store
    );
    loop_corruption!(
        corrupted_loop_prep_target_is_refused_before_codegen_and_mapping,
        true,
        Targets
    );
    loop_corruption!(
        corrupted_loop_prep_count_is_refused_before_codegen_and_mapping,
        true,
        Count
    );
    loop_corruption!(
        corrupted_loop_step_index_is_refused_before_codegen_and_mapping,
        false,
        IndexOpcode
    );
    loop_corruption!(
        corrupted_loop_step_float_is_refused_before_codegen_and_mapping,
        false,
        FloatOpcode
    );
    loop_corruption!(
        corrupted_loop_step_split_is_refused_before_codegen_and_mapping,
        false,
        Split
    );
    loop_corruption!(
        corrupted_loop_step_overflow_is_refused_before_codegen_and_mapping,
        false,
        Overflow
    );
    loop_corruption!(
        corrupted_loop_step_direction_is_refused_before_codegen_and_mapping,
        false,
        Direction
    );
    loop_corruption!(
        corrupted_loop_step_limit_is_refused_before_codegen_and_mapping,
        false,
        Limit
    );
    loop_corruption!(
        corrupted_loop_step_phi_is_refused_before_codegen_and_mapping,
        false,
        Phi
    );
    loop_corruption!(
        corrupted_loop_step_source_is_refused_before_codegen_and_mapping,
        false,
        Source
    );
    loop_corruption!(
        corrupted_loop_step_store_is_refused_before_codegen_and_mapping,
        false,
        Store
    );
    loop_corruption!(
        corrupted_loop_step_visible_is_refused_before_codegen_and_mapping,
        false,
        VisibleStore
    );
    loop_corruption!(
        corrupted_loop_step_targets_is_refused_before_codegen_and_mapping,
        false,
        Targets
    );
    loop_corruption!(
        corrupted_loop_step_count_is_refused_before_codegen_and_mapping,
        false,
        Count
    );

    fn refuse_corrupted_comparison(failure: Failure) {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype = crate::FunctionPrototype::compile(
                ctx,
                "comparison-corruption",
                b"local x=41 local y=2.5 if x<y then return 1 else return 2 end",
            )
            .unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let total = Arc::new(AtomicUsize::new(0));
        let result = compile_in(
            &snapshot,
            total.clone(),
            8 * 1024 * 1024,
            BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            failure,
        );
        assert!(
            matches!(result, Err(JitError::Compilation(ref message)) if message == "invalid scalar tag data flow")
        );
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn corrupted_comparison_same_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_comparison(Failure::CorruptComparisonSame);
    }
    #[test]
    fn corrupted_comparison_guard_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_comparison(Failure::CorruptComparisonGuard);
    }
    #[test]
    fn corrupted_comparison_bound_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_comparison(Failure::CorruptComparisonBound);
    }
    #[test]
    fn corrupted_comparison_split_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_comparison(Failure::CorruptComparisonSplit);
    }
    #[test]
    fn corrupted_comparison_phi_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_comparison(Failure::CorruptComparisonPhi);
    }
    #[test]
    fn corrupted_comparison_targets_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_comparison(Failure::CorruptComparisonTargets);
    }
    #[test]
    fn corrupted_comparison_count_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_comparison(Failure::CorruptComparisonCount);
    }
    #[test]
    fn corrupted_comparison_source_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_comparison(Failure::CorruptComparisonSource);
    }

    #[test]
    fn corrupted_comparison_polarity_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_comparison(Failure::CorruptComparisonPolarity);
    }

    fn refuse_corrupted_truth(failure: Failure) {
        let mut lua = crate::Lua::empty();
        let source: &[u8] = if matches!(
            failure,
            Failure::CorruptTruthPayload | Failure::CorruptTruthSource
        ) {
            b"local x=42 local y=7 return not x"
        } else {
            b"local x=42 if x then return 1 else return 2 end"
        };
        let snapshot = lua.enter(|ctx| {
            let prototype =
                crate::FunctionPrototype::compile(ctx, "truth-corruption", source).unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let total = Arc::new(AtomicUsize::new(0));
        let result = compile_in(
            &snapshot,
            total.clone(),
            8 * 1024 * 1024,
            BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            failure,
        );
        assert!(
            matches!(result, Err(JitError::Compilation(ref message)) if message == "invalid scalar tag data flow")
        );
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn corrupted_truth_payload_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_truth(Failure::CorruptTruthPayload);
    }
    #[test]
    fn corrupted_truth_source_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_truth(Failure::CorruptTruthSource);
    }
    #[test]
    fn corrupted_truth_condition_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_truth(Failure::CorruptTruthCondition);
    }
    #[test]
    fn corrupted_truth_targets_are_refused_before_codegen_and_mapping() {
        refuse_corrupted_truth(Failure::CorruptTruthTargets);
    }
    #[test]
    fn corrupted_truth_count_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_truth(Failure::CorruptTruthCount);
    }

    fn refuse_corrupted_arithmetic(failure: Failure) {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype = crate::FunctionPrototype::compile(
                ctx,
                "arithmetic-corruption",
                b"local x=41 local y=2 return x-y",
            )
            .unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let total = Arc::new(AtomicUsize::new(0));
        let result = compile_in(
            &snapshot,
            total.clone(),
            8 * 1024 * 1024,
            BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            failure,
        );
        assert!(
            matches!(result, Err(JitError::Compilation(ref message)) if message == "invalid scalar tag data flow")
        );
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn corrupted_arithmetic_opcode_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_arithmetic(Failure::CorruptArithmeticOpcode);
    }

    #[test]
    fn corrupted_arithmetic_operands_are_refused_before_codegen_and_mapping() {
        refuse_corrupted_arithmetic(Failure::CorruptArithmeticOperands);
    }

    #[test]
    fn corrupted_arithmetic_source_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_arithmetic(Failure::CorruptArithmeticSource);
    }

    #[test]
    fn corrupted_arithmetic_destination_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_arithmetic(Failure::CorruptArithmeticDestination);
    }

    fn refuse_corrupted_float(failure: Failure) {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype = crate::FunctionPrototype::compile(
                ctx,
                "float-corruption",
                b"local x=41 return x/2",
            )
            .unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let total = Arc::new(AtomicUsize::new(0));
        let result = compile_in(
            &snapshot,
            total.clone(),
            8 * 1024 * 1024,
            BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            failure,
        );
        assert!(
            matches!(result, Err(JitError::Compilation(ref message)) if message == "invalid scalar tag data flow")
        );
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn corrupted_float_selector_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_float(Failure::CorruptFloatSelector);
    }

    #[test]
    fn corrupted_float_payload_is_refused_before_codegen_and_mapping() {
        refuse_corrupted_float(Failure::CorruptFloatPayload);
    }

    #[test]
    #[ignore = "writes generated-kernel artifacts through make jit-disassembly"]
    fn dump_finalized_native_kernels() {
        use std::{fmt::Write, fs, path::PathBuf};

        let directory = PathBuf::from(
            std::env::var_os("LUNA_JIT_DIAGNOSTIC_DIR")
                .expect("make jit-disassembly must supply an output directory"),
        );
        fs::create_dir_all(&directory).unwrap();
        let fixtures = [
            (
                "scalar",
                "local sum=0 for i=1,100 do sum=sum+i end return sum",
            ),
            ("table", "local t={10} local x=t[1] t[1]=x+1 return t[1]"),
        ];
        for (name, source) in fixtures {
            let mut lua = crate::Lua::empty();
            let snapshot = lua.enter(|ctx| {
                let prototype =
                    crate::FunctionPrototype::compile(ctx, name, source.as_bytes()).unwrap();
                Snapshot::new(&prototype, 4096, 65536).unwrap()
            });
            let total = Arc::new(AtomicUsize::new(0));
            let code = compile(&snapshot, total.clone(), 8 * 1024 * 1024).unwrap();
            assert!(code.byte_len > 0);
            assert!(code.byte_len <= total.load(Ordering::Relaxed));
            let address = code.entry as *const u8;
            // Read finalized code bytes while the owning executable module is live.
            let bytes = unsafe { std::slice::from_raw_parts(address, code.byte_len) };
            fs::write(directory.join(format!("{name}.bin")), bytes).unwrap();
            fs::write(directory.join(format!("{name}.lua")), source).unwrap();
            let mut metadata = format!(
                "abi=3\narch={}\nos={}\nentry_address={:#x}\ncode_bytes={}\nregisters={}\n",
                std::env::consts::ARCH,
                std::env::consts::OS,
                address as usize,
                code.byte_len,
                code.registers,
            );
            for (_, symbol, entry) in helpers::SYMBOLS {
                writeln!(metadata, "helper={symbol} address={:#x}", entry as usize).unwrap();
            }
            for (pc, operation) in snapshot.operations.iter().enumerate() {
                writeln!(
                    metadata,
                    "pc={pc} entry={} operation={operation:?}",
                    code.entries[pc]
                )
                .unwrap();
            }
            for (index, constant) in snapshot.constants.iter().enumerate() {
                writeln!(metadata, "constant={index} value={constant:?}").unwrap();
            }
            let mut slots = vec![
                Slot {
                    tag: abi::NIL,
                    bits: 0
                };
                code.registers
            ];
            let mut pc = 0;
            let mut instructions = 0;
            for _ in 0..100 {
                let exit = code.invoke(&mut slots, pc, 64);
                instructions += exit.instructions;
                pc = exit.pc as usize;
                if exit.instructions == 0 {
                    break;
                }
            }
            if name == "scalar" {
                assert!(instructions > 100);
                let Operation::Return { start, count } = snapshot.operations[pc] else {
                    panic!("scalar kernel did not return");
                };
                assert_eq!(count.to_constant(), Some(1));
                let result = slots[usize::from(start.0)];
                assert_eq!((result.tag, result.bits), (abi::INTEGER, 5050));
            } else {
                assert!(snapshot
                    .operations
                    .iter()
                    .any(|op| matches!(op, Operation::GetTable { .. })));
                assert!(matches!(
                    snapshot.operations[pc],
                    Operation::NewTable { .. }
                ));
            }
            writeln!(metadata, "native_instructions={instructions}\nexit_pc={pc}").unwrap();
            fs::write(directory.join(format!("{name}.metadata")), metadata).unwrap();
            drop(code);
            assert_eq!(total.load(Ordering::Relaxed), 0);
        }
    }

    fn memory(pages: usize) -> Memory {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        assert!(page > 0);
        Memory {
            allocations: BudgetVec::new_in(BudgetAllocator(super::super::resources::Ledger::new(
                2 * 1024 * 1024,
            ))),
            total: Arc::new(AtomicUsize::new(0)),
            quota_refused: Arc::new(AtomicBool::new(false)),
            metadata_refused: Arc::new(AtomicBool::new(false)),
            unavailable: Arc::new(AtomicBool::new(false)),
            failure: Failure::None,
            limit: pages * page as usize,
            page: page as usize,
        }
    }

    #[test]
    fn page_rounded_partial_allocation_is_reclaimed_after_refusal() {
        let mut memory = memory(1);
        let total = memory.total.clone();
        let metadata = memory.allocations.allocator().0.clone();
        assert!(!memory
            .allocate(1, 1, JITMemoryKind::Executable)
            .unwrap()
            .is_null());
        assert_eq!(total.load(Ordering::Relaxed), memory.page);
        assert!(memory.allocate(1, 1, JITMemoryKind::Executable).is_err());
        assert!(memory.quota_refused.load(Ordering::Relaxed));
        assert_eq!(total.load(Ordering::Relaxed), memory.page);
        drop(memory);
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(metadata.current(), 0);
    }

    #[test]
    fn overflowing_allocation_does_not_reserve_memory() {
        let mut memory = memory(1);
        assert!(memory
            .allocate(usize::MAX, 1, JITMemoryKind::Executable)
            .is_err());
        assert!(memory.quota_refused.load(Ordering::Relaxed));
        assert_eq!(memory.total.load(Ordering::Relaxed), 0);
        assert!(memory.allocations.is_empty());
    }

    #[test]
    fn explicit_reclamation_is_idempotent() {
        let mut memory = memory(2);
        let metadata = memory.allocations.allocator().0.clone();
        memory
            .allocate(memory.page + 1, 1, JITMemoryKind::Executable)
            .unwrap();
        assert_eq!(memory.total.load(Ordering::Relaxed), memory.page * 2);
        unsafe {
            memory.free_memory();
            memory.free_memory();
        }
        assert_eq!(memory.total.load(Ordering::Relaxed), 0);
        assert!(memory.allocations.is_empty());
        assert_eq!(metadata.current(), 0);
    }

    #[test]
    fn allocation_record_quota_refuses_before_mapping_and_preserves_old_segments() {
        let mut memory = memory(4);
        let metadata = memory.allocations.allocator().0.clone();
        metadata.set_limit(1);
        assert!(memory.allocate(1, 1, JITMemoryKind::Executable).is_err());
        assert!(memory.metadata_refused.load(Ordering::Relaxed));
        assert!(!memory.quota_refused.load(Ordering::Relaxed));
        assert_eq!(memory.total.load(Ordering::Relaxed), 0);
        assert_eq!(metadata.current(), 0);
        metadata.set_limit(4096);
        memory.metadata_refused.store(false, Ordering::Relaxed);
        memory.allocate(1, 1, JITMemoryKind::Executable).unwrap();
        let retained = metadata.current();
        metadata.set_limit(retained);
        assert!(memory.allocate(1, 1, JITMemoryKind::Executable).is_err());
        assert!(memory.metadata_refused.load(Ordering::Relaxed));
        assert_eq!(memory.allocations.len(), 1);
        assert_eq!(memory.total.load(Ordering::Relaxed), memory.page);
        assert_eq!(metadata.current(), retained);
        drop(memory);
        assert_eq!(metadata.current(), 0);
    }

    #[test]
    fn entry_metadata_refusal_precedes_compiler_and_mapping_allocation() {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype =
                crate::FunctionPrototype::compile(ctx, "entry-metadata", b"return 42").unwrap();
            Snapshot::new(&prototype, 4096, 4096).unwrap()
        });
        let total = Arc::new(AtomicUsize::new(0));
        let metadata = super::super::resources::Ledger::new(1);
        assert!(matches!(
            compile_in(
                &snapshot,
                total.clone(),
                4096,
                BudgetAllocator(metadata.clone()),
                super::super::work::Limits::from(&super::super::JitConfig::default()),
                Failure::None
            ),
            Err(JitError::ResourceLimit("JIT metadata"))
        ));
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(metadata.current(), 0);
        assert_eq!(metadata.refusals(), 1);
    }

    #[test]
    fn denied_native_memory_preserves_interpretation_other_code_and_recovery() {
        use crate::{Closure, Executor, JitConfig, JitMode, Lua};
        for failure in [Failure::Allocate, Failure::Protect] {
            let mut lua = Lua::empty();
            lua.set_jit_config(JitConfig {
                mode: JitMode::Auto,
                hot_threshold: 1,
                ..JitConfig::default()
            })
            .unwrap();
            let (first, first_executor) = lua
                .try_enter(|ctx| {
                    let closure = Closure::load(ctx, Some("existing-code"), &b"return 42"[..])?;
                    Ok((
                        ctx.stash(closure),
                        ctx.stash(Executor::start(ctx, closure.into(), ())),
                    ))
                })
                .unwrap();
            assert_eq!(lua.prepare_jit().unwrap(), 1);
            let mapped = lua.jit_stats().code_bytes;
            let metadata_before = lua.jit_stats().metadata_bytes;
            assert!(mapped > 0 && metadata_before > 0);
            let (second, second_executor) = lua
                .try_enter(|ctx| {
                    let closure = Closure::load(
                        ctx,
                        Some("denied-code"),
                        &b"local t={answer=84} return t.answer"[..],
                    )?;
                    ctx.jit().0.borrow_mut().memory_failure = failure;
                    Ok((
                        ctx.stash(closure),
                        ctx.stash(Executor::start(ctx, closure.into(), ())),
                    ))
                })
                .unwrap();
            let metadata_loaded = lua.jit_stats().metadata_bytes;
            assert!(matches!(
                lua.prepare_jit(),
                Err(JitError::Unavailable(
                    "native memory allocation or protection denied"
                ))
            ));
            let refused = lua.jit_stats();
            assert_eq!(refused.code_bytes, mapped);
            assert_eq!(refused.metadata_bytes, metadata_loaded);
            assert_eq!(refused.snapshot_bytes, 0);
            assert_eq!(refused.installed_regions, 1);
            assert_eq!(refused.native_entries, 0);
            assert_eq!(refused.cache_evictions, 0);
            assert_eq!(refused.cache_eviction_refusals, 0);
            assert_eq!(lua.execute::<i64>(&first_executor).unwrap(), 42);
            assert!(lua.jit_stats().native_instructions > 0);
            let native_before = lua.jit_stats().native_instructions;
            assert_eq!(lua.execute::<i64>(&second_executor).unwrap(), 84);
            assert_eq!(lua.jit_stats().native_instructions, native_before);
            lua.enter(|ctx| ctx.jit().0.borrow_mut().memory_failure = Failure::None);
            lua.clear_jit_cache();
            assert_eq!(lua.prepare_jit().unwrap(), 2);
            let recovered =
                lua.enter(|ctx| ctx.stash(Executor::start(ctx, ctx.fetch(&second).into(), ())));
            assert_eq!(lua.execute::<i64>(&recovered).unwrap(), 84);
            assert!(lua.jit_stats().native_instructions > native_before);
            drop((first, second, first_executor, second_executor, recovered));
            lua.gc_collect();
            lua.gc_collect();
            lua.service_jit().unwrap();
            let cleared = lua.jit_stats();
            assert_eq!(
                (
                    cleared.code_bytes,
                    cleared.metadata_bytes,
                    cleared.snapshot_bytes
                ),
                (0, 0, 0)
            );
        }
    }
}
