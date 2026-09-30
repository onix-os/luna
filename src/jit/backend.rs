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
    types, AbiParam, Block, InstBuilder, MemFlagsData, Value as IrValue,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Switch};
use cranelift_jit::{
    BranchProtection, JITBuilder, JITMemoryKind, JITMemoryProvider, JITModule, SystemMemoryProvider,
};
use cranelift_module::{default_libcall_names, Linkage, Module, ModuleResult};

use super::{
    abi::{self, Entry, Exit, Slot},
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
        Failure::None,
    )
}

pub(super) fn compile_in(
    snapshot: &Snapshot,
    total: Arc<AtomicUsize>,
    limit: usize,
    metadata: BudgetAllocator,
    #[cfg(test)] failure: Failure,
) -> Result<Code, JitError> {
    snapshot.verify()?;
    let mut entries = BudgetVec::new_in(metadata.clone());
    entries
        .try_reserve_exact(snapshot.operations.len())
        .map_err(|_| JitError::ResourceLimit("JIT metadata"))?;
    entries.extend(snapshot.operations.iter().copied().map(native_entry));
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
    {
        let mut builder = FunctionBuilder::new(&mut context.func, &mut fb_context);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let arguments = builder.block_params(entry).to_vec();
        let blocks: Vec<_> = (0..snapshot.operations.len())
            .map(|_| builder.create_block())
            .collect();
        for block in &blocks {
            builder.append_block_param(*block, types::I32);
        }
        let fallback = builder.create_block();
        let guard = builder.create_block();
        let exhausted = builder.create_block();
        let panicked = builder.create_block();
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
            let helper_refs: Vec<_> = helper_ids
                .iter()
                .map(|(kind, id)| (*kind, module.declare_func_in_func(*id, builder.func)))
                .collect();
            let mut emitter = Emitter {
                builder: &mut builder,
                snapshot,
                blocks: &blocks,
                slots: arguments[0],
                fallback,
                guard,
                panicked,
                host: arguments[4],
                helpers: &helper_refs,
                count: zero,
                pc: 0,
            };
            for (pc, op) in snapshot.operations.iter().copied().enumerate() {
                emitter.pc = pc;
                emitter.builder.switch_to_block(blocks[pc]);
                emitter.count = emitter.builder.block_params(blocks[pc])[0];
                let pc_value = emitter.builder.ins().iconst(types::I64, pc as i64);
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
        for (block, reason) in [(fallback, 0), (guard, 1), (exhausted, 2), (panicked, 3)] {
            builder.switch_to_block(block);
            let pc = builder.block_params(block)[0];
            let count = builder.block_params(block)[1];
            let reason = builder.ins().iconst(types::I32, reason);
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

fn native_entry(op: Operation) -> bool {
    use Operation::*;
    match op {
        Move { .. }
        | LoadConstant { .. }
        | LoadBool { .. }
        | LoadNil { .. }
        | Test { .. }
        | Not { .. }
        | Add { .. }
        | Sub { .. }
        | Mul { .. }
        | Div { .. }
        | NumericForPrep { .. }
        | NumericForLoop { .. }
        | Eq { .. }
        | Less { .. }
        | LessEq { .. }
        | NewTable { .. }
        | GetTable { .. }
        | SetTable { .. }
        | GetUpTable { .. }
        | SetUpTable { .. }
        | GetUpValue { .. }
        | SetUpValue { .. } => true,
        Jump { close_upvalues, .. } => close_upvalues.is_none(),
        SetList { .. }
        | Call { .. }
        | TailCall { .. }
        | Return { .. }
        | VarArgs { .. }
        | MarkToBeClosed { .. }
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
        | BitNot { .. } => false,
    }
}

struct Emitter<'a, 'b> {
    builder: &'a mut FunctionBuilder<'b>,
    snapshot: &'a Snapshot,
    blocks: &'a [Block],
    slots: IrValue,
    fallback: Block,
    guard: Block,
    panicked: Block,
    host: IrValue,
    helpers: &'a [(u32, cranelift_codegen::ir::FuncRef)],
    pc: usize,
    count: IrValue,
}

impl Emitter<'_, '_> {
    fn helper(&mut self, kind: u32, a: u32, b: u32, c: u32) {
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
        self.builder
            .ins()
            .brif(completed, success, &[], declined, &[]);
        self.builder.switch_to_block(success);
        self.advance(self.pc + 1);
        self.builder.switch_to_block(declined);
        let panic =
            self.builder
                .ins()
                .icmp_imm_u(IntCC::Equal, status, i64::from(abi::HELPER_PANICKED));
        let pc = self.constant(self.pc as u64);
        self.builder.ins().brif(
            panic,
            self.panicked,
            &[pc.into(), self.count.into()],
            self.fallback,
            &[pc.into(), self.count.into()],
        );
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

    fn store(&mut self, register: u8, tag: IrValue, bits: IrValue) {
        self.builder.ins().store(
            MemFlagsData::new(),
            tag,
            self.slots,
            i32::from(register) * 16,
        );
        self.builder.ins().store(
            MemFlagsData::new(),
            bits,
            self.slots,
            i32::from(register) * 16 + 8,
        );
    }

    fn store_typed(&mut self, register: u8, tag: u64, bits: IrValue) {
        let tag = self.constant(tag);
        self.store(register, tag, bits);
    }

    fn require(&mut self, condition: IrValue) {
        let next = self.builder.create_block();
        let pc = self.constant(self.pc as u64);
        self.builder.ins().brif(
            condition,
            next,
            &[],
            self.guard,
            &[pc.into(), self.count.into()],
        );
        self.builder.switch_to_block(next);
    }

    fn tag_is(&mut self, tag: IrValue, value: u64) -> IrValue {
        self.builder
            .ins()
            .icmp_imm_s(IntCC::Equal, tag, value as i64)
    }

    fn require_numeric(&mut self, tag: IrValue) {
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
        self.builder.ins().select(integer, converted, float)
    }

    fn advance(&mut self, next: usize) {
        let count = self.builder.ins().iadd_imm_s(self.count, 1);
        if let Some(block) = self.blocks.get(next) {
            self.builder.ins().jump(*block, &[count.into()]);
        } else {
            let pc = self.constant(next as u64);
            self.builder
                .ins()
                .jump(self.fallback, &[pc.into(), count.into()]);
        }
    }

    fn branch(&mut self, condition: IrValue, yes: usize, no: usize) {
        let count = self.builder.ins().iadd_imm_s(self.count, 1);
        self.builder.ins().brif(
            condition,
            self.blocks[yes],
            &[count.into()],
            self.blocks[no],
            &[count.into()],
        );
    }

    fn bail(&mut self) {
        let pc = self.constant(self.pc as u64);
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
                self.builder.ins().brif(scalar, direct, &[], reference, &[]);
                self.builder.switch_to_block(reference);
                let available = self.builder.ins().icmp_imm_s(IntCC::NotEqual, self.host, 0);
                self.require(available);
                self.helper(abi::HELPER_MOVE, u32::from(dest.0), u32::from(source.0), 0);
                self.builder.switch_to_block(direct);
                self.store(dest.0, tag, bits);
                self.advance(self.pc + 1);
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
                self.store_typed(dest.0, slot.tag, bits);
                self.advance(self.pc + 1);
            }
            LoadBool {
                dest,
                value,
                skip_next,
            } => {
                let bits = self.constant(u64::from(value));
                self.store_typed(dest.0, abi::BOOLEAN, bits);
                self.advance(self.pc + 1 + usize::from(skip_next));
            }
            LoadNil { dest, count } => {
                let zero = self.constant(0);
                for index in 0..count {
                    self.store_typed(dest.0 + index, abi::NIL, zero);
                }
                self.advance(self.pc + 1);
            }
            Jump {
                offset,
                close_upvalues,
            } if close_upvalues.is_none() => {
                self.advance(
                    (self.pc + 1)
                        .checked_add_signed(isize::from(offset))
                        .unwrap(),
                );
            }
            Test { value, is_true } => {
                let (tag, bits) = self.load(value.0);
                let truth = self.truth(tag, bits);
                let condition = if is_true {
                    truth
                } else {
                    self.builder.ins().bxor_imm_u(truth, 1)
                };
                self.branch(condition, self.pc + 2, self.pc + 1);
            }
            Not { dest, source } => {
                let (tag, bits) = self.load(source.0);
                let truth = self.truth(tag, bits);
                let opposite = self.builder.ins().bxor_imm_u(truth, 1);
                let bits = self.builder.ins().uextend(types::I64, opposite);
                self.store_typed(dest.0, abi::BOOLEAN, bits);
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
        let integer = self.builder.create_block();
        let float = self.builder.create_block();
        let li = self.tag_is(lt, abi::INTEGER);
        let ri = self.tag_is(rt, abi::INTEGER);
        let both = self.builder.ins().band(li, ri);
        if matches!(op, Operation::Div { .. }) {
            self.builder.ins().jump(float, &[]);
        } else {
            self.builder.ins().brif(both, integer, &[], float, &[]);
        }
        self.builder.switch_to_block(integer);
        let bits = match op {
            Operation::Add { .. } => self.builder.ins().iadd(lb, rb),
            Operation::Sub { .. } => self.builder.ins().isub(lb, rb),
            _ => self.builder.ins().imul(lb, rb),
        };
        self.store_typed(dest, abi::INTEGER, bits);
        self.advance(self.pc + 1);
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
        self.store_typed(dest, abi::NUMBER, bits);
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
        self.builder.ins().brif(same, same_type, &[], mixed, &[]);
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
        self.builder.ins().jump(join, &[result.into()]);
        self.builder.switch_to_block(mixed);
        let result = self.mixed_compare(op, lt, lb, rb);
        self.builder.ins().jump(join, &[result.into()]);
        self.builder.switch_to_block(join);
        let result = self.builder.block_params(join)[0];
        let skip = if skip_if {
            result
        } else {
            self.builder.ins().bxor_imm_u(result, 1)
        };
        self.branch(skip, self.pc + 2, self.pc + 1);
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
        self.require(nonzero);
        let integer = self.builder.create_block();
        let float = self.builder.create_block();
        let ii = self.tag_is(it, abi::INTEGER);
        let si = self.tag_is(st, abi::INTEGER);
        let both = self.builder.ins().band(ii, si);
        self.builder.ins().brif(both, integer, &[], float, &[]);
        let target = (self.pc + 1).checked_add_signed(isize::from(jump)).unwrap();
        self.builder.switch_to_block(integer);
        let index = self.builder.ins().isub(ib, sb);
        self.store_typed(base, abi::INTEGER, index);
        self.advance(target);
        self.builder.switch_to_block(float);
        let index = self.as_float(it, ib);
        let index = self.builder.ins().fsub(index, step);
        let bits = self
            .builder
            .ins()
            .bitcast(types::I64, MemFlagsData::new(), index);
        self.store_typed(base, abi::NUMBER, bits);
        self.advance(target);
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
        self.builder.ins().brif(both, integer, &[], float, &[]);
        let join = self.builder.create_block();
        self.builder.append_block_param(join, types::I64);
        self.builder.append_block_param(join, types::I64);
        self.builder.append_block_param(join, types::I8);
        self.builder.switch_to_block(integer);
        let (index, overflow) = self.builder.ins().sadd_overflow(ib, sb);
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
        let not_overflow = self.builder.ins().bxor_imm_u(overflow, 1);
        let condition = self.builder.ins().band(not_overflow, in_range);
        let tag = self.constant(abi::INTEGER);
        self.builder
            .ins()
            .jump(join, &[tag.into(), index.into(), condition.into()]);
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
        self.builder
            .ins()
            .jump(join, &[tag.into(), bits.into(), condition.into()]);
        self.builder.switch_to_block(join);
        let tag = self.builder.block_params(join)[0];
        let bits = self.builder.block_params(join)[1];
        let condition = self.builder.block_params(join)[2];
        self.store(base, tag, bits);
        let taken = self.builder.create_block();
        let done = self.builder.create_block();
        self.builder.ins().brif(condition, taken, &[], done, &[]);
        self.builder.switch_to_block(taken);
        self.store(base + 3, tag, bits);
        self.advance((self.pc + 1).checked_add_signed(isize::from(jump)).unwrap());
        self.builder.switch_to_block(done);
        self.advance(self.pc + 1);
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;

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
