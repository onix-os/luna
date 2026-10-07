use std::{alloc::Layout, io, sync::atomic::Ordering};

use allocator_api2::vec::Vec as BudgetVec;
use cranelift_codegen::ir::{
    condcodes::{FloatCC, IntCC},
    types, AbiParam, Block, Inst, InstBuilder, MemFlagsData, Value as IrValue,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{BranchProtection, JITBuilder, JITMemoryKind, JITMemoryProvider, JITModule};
use cranelift_module::{default_libcall_names, Linkage, Module, ModuleReloc, ModuleResult};

use super::{
    abi::{self, Entry, Exit, Slot},
    atomic_owner::AtomicShared,
    exits::Kind as ExitKind,
    global_box,
    handoff::Handoff,
    helpers,
    ir::Snapshot,
    memory_status::MemoryStatus,
    resources::{BudgetAllocator, MappingCounter, Reservation},
    segments::{Request, Segment},
    JitError,
};
use crate::opcode::{Operation, RCIndex};

#[cfg(not(miri))]
pub(super) mod calls;
#[cfg(not(miri))]
pub(super) mod region;

struct Memory {
    allocations: BudgetVec<Segment, BudgetAllocator>,
    total: MappingCounter,
    status: AtomicShared<MemoryStatus>,
    #[cfg(test)]
    failure: Failure,
    limit: usize,
    page: usize,
}

impl Memory {
    fn release(&mut self) {
        let ledger = self.allocations.allocator().0.clone();
        for segment in self.allocations.drain(..) {
            let bytes = segment.bytes;
            let requested = segment.requested_bytes;
            drop(segment);
            self.total.release_requested(requested);
            self.total.fetch_sub(bytes, Ordering::Relaxed);
            ledger.release_external(bytes);
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
        let request = Request::new(size, align, self.page).inspect_err(|_| {
            self.status.quota_refused.store(true, Ordering::Relaxed);
        })?;
        let bytes = request.bytes;
        let image_bytes = self
            .allocations
            .iter()
            .try_fold(bytes, |total, segment| total.checked_add(segment.bytes));
        if image_bytes.is_none_or(|bytes| bytes > self.limit) {
            self.status.image_refused.store(true, Ordering::Relaxed);
            self.status.quota_refused.store(true, Ordering::Relaxed);
            return Err(io::Error::other("native image exceeds code cache limit"));
        }
        self.allocations.try_reserve_exact(1).map_err(|_| {
            self.status.metadata_refused.store(true, Ordering::Relaxed);
            io::Error::other("native allocation record quota exhausted")
        })?;
        self.total
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|next| *next <= self.limit)
            })
            .map_err(|_| {
                self.status.quota_refused.store(true, Ordering::Relaxed);
                io::Error::other("native memory quota exhausted")
            })?;
        if self
            .allocations
            .allocator()
            .0
            .reserve_external(bytes)
            .is_err()
        {
            self.total.fetch_sub(bytes, Ordering::Relaxed);
            self.status.quota_refused.store(true, Ordering::Relaxed);
            return Err(io::Error::other("host memory quota exhausted"));
        }
        #[cfg(test)]
        if matches!(
            self.failure,
            Failure::Allocate
                | Failure::RefuseRelocationCopy
                | Failure::RefuseSignatures
                | Failure::RefuseSymbols
        ) {
            self.total.fetch_sub(bytes, Ordering::Relaxed);
            self.allocations.allocator().0.release_external(bytes);
            self.status.unavailable.store(true, Ordering::Relaxed);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected native allocation denial",
            ));
        }
        match Segment::new(request, kind) {
            Ok((segment, pointer)) => {
                self.total.add_requested(segment.requested_bytes);
                self.allocations.push(segment);
                Ok(pointer)
            }
            Err(error) => {
                self.total.fetch_sub(bytes, Ordering::Relaxed);
                self.allocations.allocator().0.release_external(bytes);
                self.status.unavailable.store(true, Ordering::Relaxed);
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
            self.status.unavailable.store(true, Ordering::Relaxed);
            return Err(cranelift_module::ModuleError::Backend(anyhow::anyhow!(
                "injected native protection denial"
            )));
        }
        #[cfg(test)]
        let segments = self.allocations.iter_mut().enumerate();
        #[cfg(not(test))]
        let segments = self.allocations.iter_mut();
        for segment in segments {
            #[cfg(test)]
            let (index, segment) = segment;
            #[cfg(test)]
            if self.failure == Failure::ProtectAfterFirst && index == 1 {
                self.status.unavailable.store(true, Ordering::Relaxed);
                return Err(cranelift_module::ModuleError::Backend(anyhow::anyhow!(
                    "injected partial protection denial"
                )));
            }
            if let Err(error) = segment.finalize(protection) {
                self.status.unavailable.store(true, Ordering::Relaxed);
                return Err(cranelift_module::ModuleError::Backend(anyhow::Error::new(
                    error,
                )));
            }
        }
        wasmtime_jit_icache_coherence::pipeline_flush_mt().map_err(|error| {
            self.status.unavailable.store(true, Ordering::Relaxed);
            cranelift_module::ModuleError::Backend(anyhow::anyhow!(
                "native pipeline flush: {error}"
            ))
        })
    }
}

struct Provider(Handoff<Memory>);

impl JITMemoryProvider for Provider {
    fn allocate(&mut self, size: usize, align: u64, kind: JITMemoryKind) -> io::Result<*mut u8> {
        self.0
            .with_mut(|memory| memory.allocate(size, align, kind))
            .unwrap_or_else(|| Err(io::Error::other("native memory already detached")))
    }

    unsafe fn free_memory(&mut self) {
        self.0.with_mut(Memory::release);
    }

    fn finalize(&mut self, protection: BranchProtection) -> ModuleResult<()> {
        match self.0.borrow_mut().as_mut() {
            Some(memory) => memory.finalize(protection),
            None => Err(cranelift_module::ModuleError::Backend(anyhow::anyhow!(
                "native memory already detached"
            ))),
        }
    }
}

pub(super) struct Code {
    _memory: Memory,
    entry: Entry,
    #[cfg(test)]
    byte_len: usize,
    pub(super) relocations: usize,
    pub registers: usize,
    pub entries: BudgetVec<bool, BudgetAllocator>,
    pub projected_upvalues: bool,
    #[cfg(test)]
    pub continuations: Option<super::continuations::Continuations>,
    #[cfg(test)]
    pub scalar_leaf: Option<super::leaf::Pattern>,
    #[cfg(test)]
    cell_kernel: bool,
    #[cfg(test)]
    pub integer_activation: bool,
    #[cfg(all(test, not(miri)))]
    pub scalar_kernel: Option<super::owner::Shared<Code>>,
}

impl Code {
    #[cfg(not(miri))]
    pub(super) fn linked_entry(&self) -> Entry {
        self.entry
    }
    #[cfg(all(test, not(miri)))]
    pub fn discard_scalar_kernel(&mut self) -> bool {
        self.scalar_kernel.take().is_some()
    }

    #[cfg(all(test, not(miri)))]
    pub fn discard_optional_entries(&mut self) -> bool {
        let scalar = self.discard_scalar_kernel();
        let continuations = self.continuations.take().is_some();
        scalar || continuations
    }

    #[cfg(all(test, not(miri)))]
    pub fn into_shared(
        self,
        allocator: BudgetAllocator,
    ) -> Result<super::owner::Shared<Code>, allocator_api2::alloc::AllocError> {
        match super::owner::Shared::try_new_recover(self, allocator.clone()) {
            Ok(owner) => Ok(owner),
            Err((mut code, error)) => {
                if code.discard_optional_entries() {
                    super::owner::Shared::try_new(code, allocator)
                } else {
                    Err(error)
                }
            }
        }
    }

    #[cfg(test)]
    pub fn invoke(&self, slots: &mut [Slot], pc: usize, budget: u32) -> Exit {
        unsafe { self.invoke_host(slots, pc, budget, std::ptr::null_mut()) }
    }

    /// Invokes a pinned module using scalar scratch slots and an opaque helper host.
    ///
    /// # Safety
    /// `host` must be null or carry a live, exclusively borrowed helper frame for this call.
    #[cfg(test)]
    pub unsafe fn invoke_host(
        &self,
        slots: &mut [Slot],
        pc: usize,
        budget: u32,
        host: *mut abi::Host,
    ) -> Exit {
        assert!(slots.len() >= self.registers);
        unsafe { self.invoke_raw(slots.as_mut_ptr(), pc, budget, host) }
    }

    /// Invokes pinned code using the original pointer to its scalar register prefix.
    ///
    /// # Safety
    /// `slots` covers `self.registers` initialized slots. Host data and any projection
    /// refer to live, exclusively accessible frame buffers for the call.
    pub unsafe fn invoke_raw(
        &self,
        slots: *mut Slot,
        pc: usize,
        budget: u32,
        host: *mut abi::Host,
    ) -> Exit {
        #[cfg(test)]
        assert!(!self.cell_kernel, "scalar kernel requires a cell view");
        unsafe { abi::invoke(self.entry, slots, pc, budget, host) }
    }

    /// Invokes scalar code with initialized slots and an optional scoped cell view.
    ///
    /// # Safety
    /// Slots cover the register prefix. A non-null view and its non-null cell must
    /// be live and exclusive; the view cannot overlap slots or cell storage.
    #[cfg(all(test, not(miri)))]
    pub unsafe fn invoke_cell_raw(
        &self,
        slots: *mut Slot,
        pc: usize,
        budget: u32,
        view: *mut super::leaf::View,
    ) -> Exit {
        assert!(self.cell_kernel, "ordinary entry requires a helper host");
        let entry: super::leaf::CellEntry = unsafe { std::mem::transmute(self.entry) };
        let mut exit = Exit::default();
        unsafe { entry(slots, pc as u64, budget.min(64), &mut exit, view) };
        exit
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Failure {
    RequireReleasedWorkspace(usize),
    RequireReleasedSnapshot,
    DetectHostSetup,
    DetectProviderSetup,
    ProtectAfterFirst,
    RefuseOwnerStorage,
    RefuseOwnerAllocation,
    #[cfg(not(miri))]
    RefuseScalarOwnerStorage,
    #[cfg(not(miri))]
    RefuseScalarOwnerAllocation,
    #[cfg(not(miri))]
    RefuseScalarCacheStorage,
    #[cfg(not(miri))]
    RefuseContinuationStorage,
    #[cfg(not(miri))]
    RefuseContinuationAllocation,
    #[cfg(not(miri))]
    RefuseContinuationCacheStorage,
    RefusePredecessors,
    RefuseDominanceStorage,
    RefuseDominanceWork,
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
    CorruptEntryFlow(super::entry_flow::Fault),
    CorruptRegionFlow(super::entry_flow::RegionFault),
    RefuseRegionWorkspace,
    RefuseRelocationStorage(bool),
    RefuseRelocationCopy,
    RequireRelocationCopy(usize),
    RefuseSignatures,
    RefuseSymbols,
    NativeIsaUnavailable,
    RequireSignatures(usize),
    CorruptInlineName,
    CorruptInlineSignature,
    CorruptBinding(super::tags::BindingFault),
}

#[cfg(test)]
pub(super) fn compile(
    snapshot: &Snapshot,
    total: MappingCounter,
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

#[cfg(all(test, not(miri)))]
pub(super) fn compile_continuations_in(
    snapshot: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    work: super::work::Limits,
    failure: Failure,
) -> Result<Code, JitError> {
    let mut code = compile_in(snapshot, total, limit, metadata.clone(), work, failure)?;
    let Ok(_owner) = Reservation::new(
        metadata.0.clone(),
        super::owner::Shared::<Code>::allocation_bytes(),
    ) else {
        return Ok(code);
    };
    let previous_limit = metadata.0.limit();
    match failure {
        Failure::RefuseContinuationStorage => metadata.0.set_limit(metadata.0.current()),
        Failure::RefuseContinuationAllocation => metadata.0.fail_after(0),
        _ => {}
    }
    code.continuations = super::continuations::Continuations::new(snapshot, metadata.clone()).ok();
    match failure {
        Failure::RefuseContinuationStorage => metadata.0.set_limit(previous_limit),
        Failure::RefuseContinuationAllocation => metadata.0.fail_after(usize::MAX),
        _ => {}
    }
    Ok(code)
}

pub(super) fn compile_in(
    snapshot: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    work: super::work::Limits,
    #[cfg(test)] failure: Failure,
) -> Result<Code, JitError> {
    compile_selected(
        snapshot,
        total,
        limit,
        metadata,
        work,
        Selection {
            projected: false,
            #[cfg(not(miri))]
            scoped_helpers: false,
            #[cfg(test)]
            leaf: false,
            #[cfg(test)]
            cell_kernel: false,
            #[cfg(test)]
            integer_activation: false,
            #[cfg(test)]
            failure,
        },
    )
}

#[cfg(test)]
pub(super) fn compile_projected_in(
    snapshot: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    work: super::work::Limits,
    failure: Failure,
) -> Result<Code, JitError> {
    compile_selected(
        snapshot,
        total,
        limit,
        metadata,
        work,
        Selection {
            projected: true,
            #[cfg(not(miri))]
            scoped_helpers: false,
            leaf: false,
            cell_kernel: false,
            integer_activation: false,
            failure,
        },
    )
}

#[cfg(test)]
pub(super) fn compile_leaf_in(
    snapshot: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    work: super::work::Limits,
    failure: Failure,
) -> Result<Code, JitError> {
    compile_selected(
        snapshot,
        total,
        limit,
        metadata,
        work,
        Selection {
            projected: false,
            #[cfg(not(miri))]
            scoped_helpers: false,
            leaf: true,
            cell_kernel: false,
            integer_activation: false,
            failure,
        },
    )
}

#[cfg(all(test, not(miri)))]
pub(super) fn compile_leaf_kernel_in(
    snapshot: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    work: super::work::Limits,
    failure: Failure,
) -> Result<Code, JitError> {
    compile_selected(
        snapshot,
        total,
        limit,
        metadata,
        work,
        Selection {
            projected: false,
            scoped_helpers: false,
            leaf: true,
            cell_kernel: true,
            integer_activation: false,
            failure,
        },
    )
}

#[cfg(all(test, not(miri)))]
pub(super) fn compile_leaf_pair_in(
    snapshot: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    work: super::work::Limits,
    failure: Failure,
) -> Result<Code, JitError> {
    compile_leaf_pair_selected(snapshot, total, limit, metadata, work, failure, false)
}

#[cfg(all(test, not(miri)))]
pub(super) fn compile_integer_leaf_pair_in(
    snapshot: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    work: super::work::Limits,
    failure: Failure,
) -> Result<Code, JitError> {
    compile_leaf_pair_selected(snapshot, total, limit, metadata, work, failure, true)
}

#[cfg(all(test, not(miri)))]
fn compile_leaf_pair_selected(
    snapshot: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    work: super::work::Limits,
    failure: Failure,
    integer_activation: bool,
) -> Result<Code, JitError> {
    let expansion = super::work::Expansion::admit(snapshot, work)?;
    let mut ordinary = compile_in(
        snapshot,
        total.clone(),
        limit,
        metadata.clone(),
        work,
        failure,
    )?;
    let Ok(_ordinary_owner) = Reservation::new(
        metadata.0.clone(),
        super::owner::Shared::<Code>::allocation_bytes(),
    ) else {
        return Ok(ordinary);
    };
    let remaining = super::work::Limits {
        instructions: work.instructions.saturating_sub(expansion.instructions),
        blocks: work.blocks.saturating_sub(expansion.blocks),
        relocations: work.relocations.saturating_sub(ordinary.relocations),
    };
    let kernel = if integer_activation {
        compile_selected(
            snapshot,
            total,
            limit,
            metadata.clone(),
            remaining,
            Selection {
                projected: false,
                scoped_helpers: false,
                leaf: true,
                cell_kernel: true,
                integer_activation: true,
                failure,
            },
        )
    } else {
        compile_leaf_kernel_in(snapshot, total, limit, metadata.clone(), remaining, failure)
    };
    let Ok(kernel) = kernel else {
        return Ok(ordinary);
    };
    let previous_limit = metadata.0.limit();
    match failure {
        Failure::RefuseScalarOwnerStorage => metadata.0.set_limit(metadata.0.current()),
        Failure::RefuseScalarOwnerAllocation => metadata.0.fail_after(0),
        _ => {}
    }
    ordinary.scalar_kernel = super::owner::Shared::try_new(kernel, metadata.clone()).ok();
    match failure {
        Failure::RefuseScalarOwnerStorage => metadata.0.set_limit(previous_limit),
        Failure::RefuseScalarOwnerAllocation => metadata.0.fail_after(usize::MAX),
        _ => {}
    }
    Ok(ordinary)
}

struct Selection {
    projected: bool,
    #[cfg(not(miri))]
    scoped_helpers: bool,
    #[cfg(test)]
    leaf: bool,
    #[cfg(test)]
    cell_kernel: bool,
    #[cfg(test)]
    integer_activation: bool,
    #[cfg(test)]
    failure: Failure,
}

#[cfg(not(miri))]
pub(super) fn compile_scoped_in(
    snapshot: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    work: super::work::Limits,
) -> Result<Code, JitError> {
    compile_selected(
        snapshot,
        total,
        limit,
        metadata,
        work,
        Selection {
            projected: false,
            scoped_helpers: true,
            #[cfg(test)]
            leaf: false,
            #[cfg(test)]
            cell_kernel: false,
            #[cfg(test)]
            integer_activation: false,
            #[cfg(test)]
            failure: Failure::None,
        },
    )
}

fn compile_selected(
    snapshot: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    work: super::work::Limits,
    selection: Selection,
) -> Result<Code, JitError> {
    #[cfg(test)]
    let failure = selection.failure;
    #[cfg(test)]
    let leaf_pattern = if selection.leaf {
        Some(
            super::leaf::Pattern::recognize(snapshot)
                .ok_or_else(|| JitError::Compilation("invalid scalar-cell leaf".into()))?,
        )
    } else {
        None
    };
    let expansion = super::work::Expansion::admit(snapshot, work)?;
    let graph = super::flow::FlowGraph::new(snapshot)?;
    let mut stores = super::tags::Stores::new(&graph, snapshot)?;
    let mut blocks = BudgetVec::new_in(snapshot.operations.allocator().clone());
    blocks
        .try_reserve_exact(snapshot.operations.len())
        .map_err(|_| JitError::ResourceLimit("frontend block map"))?;
    let mut paths = super::entry_flow::Paths::new(snapshot)?;
    let mut entries = BudgetVec::new_in(metadata.clone());
    entries
        .try_reserve_exact(snapshot.operations.len())
        .map_err(|_| JitError::ResourceLimit("JIT metadata"))?;
    entries.extend(graph.nodes.iter().map(|node| node.lowering.native()));
    let status = MemoryStatus::try_new(metadata.clone())?;
    #[cfg(test)]
    if failure == Failure::DetectHostSetup {
        return Err(JitError::Compilation("injected host setup probe".into()));
    }
    let fail = |error: cranelift_module::ModuleError| {
        status
            .error()
            .unwrap_or_else(|| JitError::Compilation(error.to_string()))
    };
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return Err(JitError::Unavailable(
            "cannot determine native memory page size",
        ));
    }
    let memory = Handoff::try_new(
        Memory {
            allocations: BudgetVec::new_in(metadata.clone()),
            total,
            status: status.clone(),
            #[cfg(test)]
            failure,
            limit,
            page: page as usize,
        },
        metadata.clone(),
    )
    .map_err(|_| JitError::ResourceLimit("JIT metadata"))?;
    let provider_charge;
    let provider;
    (provider, provider_charge) = global_box::try_new(Provider(memory.clone()), metadata)
        .map_err(|_| JitError::ResourceLimit("JIT metadata"))?;
    #[cfg(test)]
    if failure == Failure::DetectProviderSetup {
        return Err(JitError::Compilation(
            "injected provider setup probe".into(),
        ));
    }
    let helper_symbols = helpers::SYMBOLS;
    #[cfg(not(miri))]
    let helper_symbols = if selection.scoped_helpers {
        super::scoped_helpers::SYMBOLS
    } else {
        helper_symbols
    };
    let mut selected_symbols = helper_symbols;
    let mut helper_count = 0;
    for symbol in helper_symbols {
        if helper_needed(symbol.0, snapshot) {
            selected_symbols[helper_count] = symbol;
            helper_count += 1;
        }
    }
    let helper_symbols = &selected_symbols[..helper_count];
    let symbol_bytes = symbol_storage_bytes(helper_symbols.iter().map(|(_, name, _)| name.len()))?;
    #[cfg(test)]
    if failure == Failure::RefuseSymbols {
        let ledger = &snapshot.operations.allocator().0;
        ledger.set_limit(ledger.current());
    }
    let symbol_charge = Reservation::new(snapshot.operations.allocator().0.clone(), symbol_bytes)
        .map_err(|_| JitError::ResourceLimit("native symbols"))?;
    #[cfg(test)]
    let native_isa = if failure == Failure::NativeIsaUnavailable {
        Err("injected unsupported host instruction set")
    } else {
        cranelift_native::builder()
    };
    #[cfg(not(test))]
    let native_isa = cranelift_native::builder();
    let mut jit = native_builder(native_isa)?;
    for &(_, name, entry) in helper_symbols {
        jit.symbol(owned_symbol(name)?, entry as *const u8);
    }
    jit.memory_provider(provider);
    let relocation_copy_charge;
    let mut projection_copy_charges = [None, None];
    #[cfg(test)]
    let mut projection_copy_bytes = 0;
    let signature_charge;
    let mut module = JITModule::new(jit);
    let ptr = module.target_config().pointer_type();
    let entry_types = [ptr, types::I64, types::I32, ptr, ptr];
    let helper_types = [ptr, ptr, types::I32, types::I32, types::I32, types::I32];
    let helper_returns = [types::I32];
    let projected_kinds = [abi::HELPER_GET_UPVALUE, abi::HELPER_SET_UPVALUE].map(|kind| {
        #[cfg(test)]
        let selected = selection.projected || leaf_pattern.is_some();
        #[cfg(not(test))]
        let selected = selection.projected;
        selected
            && snapshot.operations.iter().any(|operation| {
                matches!(
                    (kind, operation),
                    (abi::HELPER_GET_UPVALUE, Operation::GetUpValue { .. })
                        | (abi::HELPER_SET_UPVALUE, Operation::SetUpValue { .. })
                )
            })
    });
    let projection_count = projected_kinds.iter().filter(|&&needed| needed).count();
    let signature_bytes = signature_storage_bytes(
        entry_types.len(),
        helper_types.len(),
        helper_returns.len(),
        helper_count + 2 * projection_count,
    )?;
    #[cfg(test)]
    let signature_bytes = signature_bytes
        .checked_add(if leaf_pattern.is_some() {
            Layout::array::<AbiParam>(
                (helper_types.len() + helper_returns.len()) * projection_count,
            )
            .map_err(|_| JitError::ResourceLimit("inline signature size"))?
            .size()
        } else {
            0
        })
        .ok_or(JitError::ResourceLimit("native signature size"))?;
    #[cfg(test)]
    if failure == Failure::RefuseSignatures {
        let ledger = &snapshot.operations.allocator().0;
        ledger.set_limit(ledger.current());
    }
    signature_charge = Reservation::new(snapshot.operations.allocator().0.clone(), signature_bytes)
        .map_err(|_| JitError::ResourceLimit("native signatures"))?;
    let mut signature = module.make_signature();
    fill_signature(&mut signature.params, entry_types)?;
    let function = module
        .declare_anonymous_function(&signature)
        .map_err(fail)?;
    #[cfg(test)]
    if matches!(failure, Failure::RequireSignatures(_)) {
        let declaration = module.declarations().get_function_decl(function);
        assert!(declaration.name.is_none());
        assert_eq!(declaration.linkage, Linkage::Local);
        assert_eq!(declaration.signature, signature);
    }
    let mut helper_signature = module.make_signature();
    fill_signature(&mut helper_signature.params, helper_types)?;
    fill_signature(&mut helper_signature.returns, helper_returns)?;
    let helper_ids = super::arrays::try_array::<_, _, { helpers::SYMBOLS.len() }>(|index| {
        let Some(&(kind, name, _)) = helper_symbols.get(index) else {
            return Ok(None);
        };
        module
            .declare_function(name, Linkage::Import, &helper_signature)
            .map(|id| Some((kind, id)))
    })
    .map_err(fail)?;
    let projection_ids = super::arrays::try_array::<_, _, 2>(|index| {
        if projected_kinds[index] {
            module
                .declare_anonymous_function(&helper_signature)
                .map(Some)
                .map_err(fail)
        } else {
            Ok(None)
        }
    })?;
    drop(helper_signature);
    let mut context = module.make_context();
    context.func.signature = signature;
    let mut fb_context = FunctionBuilderContext::new();
    let (fallback, guard, exhausted, panicked, root);
    let mut helper_refs =
        [(0, cranelift_codegen::ir::FuncRef::from_u32(0)); helpers::SYMBOLS.len()];
    {
        let mut builder = FunctionBuilder::new(&mut context.func, &mut fb_context);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let arguments: [IrValue; 5] = builder
            .block_params(entry)
            .try_into()
            .map_err(|_| JitError::Compilation("invalid native entry parameters".into()))?;
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
        for _ in &blocks {
            paths.record(super::entry_flow::Point {
                trampoline: builder.create_block(),
                body: builder.create_block(),
                region: [0, 0],
            });
        }
        root = paths.emit_entry(&mut builder, &blocks, fallback);
        {
            for (destination, (kind, id)) in
                helper_refs.iter_mut().zip(helper_ids.into_iter().flatten())
            {
                let id = match kind {
                    abi::HELPER_GET_UPVALUE => projection_ids[0].unwrap_or(id),
                    abi::HELPER_SET_UPVALUE => projection_ids[1].unwrap_or(id),
                    _ => id,
                };
                *destination = (kind, module.declare_func_in_func(id, builder.func));
            }
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
                helpers: &helper_refs[..helper_count],
                count: arguments[2],
                pc: 0,
                written: false,
                stores: &mut stores,
                #[cfg(test)]
                omit_numeric_guards: failure == Failure::OmitNumericGuards,
            };
            for (pc, op) in snapshot.operations.iter().copied().enumerate() {
                let start = emitter.builder.func.dfg.num_blocks() as u32;
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
                let body = paths.body(pc);
                emitter.builder.ins().brif(
                    limit,
                    exhausted,
                    &[pc_value.into(), emitter.count.into()],
                    body,
                    &[],
                );
                emitter.builder.switch_to_block(body);
                emitter.emit(op);
                paths.region(pc, start, emitter.builder.func.dfg.num_blocks() as u32);
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
    drop(fb_context);
    let helper_refs = &helper_refs[..helper_count];
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
    #[cfg(test)]
    if failure == Failure::RefusePredecessors {
        let ledger = &snapshot.operations.allocator().0;
        ledger.set_limit(ledger.current());
    }
    #[cfg(test)]
    if failure == Failure::RefuseDominanceStorage {
        let allocator = snapshot.operations.allocator();
        let baseline = allocator.0.current();
        let graph = super::preds::Predecessors::new(&context.func, allocator.clone())?;
        let limit = allocator.0.current();
        drop(graph);
        assert_eq!(allocator.0.current(), baseline);
        allocator.0.set_limit(limit);
    }
    #[cfg(test)]
    if failure == Failure::RefuseDominanceWork {
        stores.refuse_dominance_work();
    }
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
    #[cfg(test)]
    let mut root = root;
    #[cfg(test)]
    if let Failure::CorruptEntryFlow(fault) = failure {
        paths.corrupt(&mut context.func, &blocks, &mut root, fallback, fault);
    }
    paths.verify(&context.func, snapshot, &blocks, root, fallback, exhausted)?;
    #[cfg(test)]
    if let Failure::CorruptRegionFlow(fault) = failure {
        paths.corrupt_region(&mut context.func, &blocks, root, &exit_handlers, fault);
    }
    #[cfg(test)]
    if failure == Failure::RefuseRegionWorkspace {
        let ledger = &snapshot.operations.allocator().0;
        ledger.set_limit(ledger.current());
    }
    paths.verify_regions(&context.func, &graph, &blocks, root, &exit_handlers)?;
    #[cfg(test)]
    if let Failure::CorruptBinding(fault) = failure {
        stores.corrupt_binding(&mut context.func, parameters[0], fault);
    }
    stores.verify_bindings(&context.func, parameters[0], &paths, &graph)?;
    drop((stores, paths, graph, blocks));
    let mut projection_contexts = [None, None];
    let mut projection_instructions = 0;
    let mut projection_blocks = 0;
    for (index, id) in projection_ids.iter().copied().enumerate() {
        let Some(id) = id else { continue };
        let kind = [abi::HELPER_GET_UPVALUE, abi::HELPER_SET_UPVALUE][index];
        let fallback_id = helper_ids
            .iter()
            .flatten()
            .find(|&&(candidate, _)| candidate == kind)
            .unwrap()
            .1;
        let mut projection = module.make_context();
        #[cfg(test)]
        {
            projection.func.name = cranelift_codegen::ir::UserFuncName::user(0, id.as_u32());
        }
        projection.func.signature = module
            .declarations()
            .get_function_decl(id)
            .signature
            .clone();
        let fallback_ref = module.declare_func_in_func(fallback_id, &mut projection.func);
        let mut frontend = FunctionBuilderContext::new();
        {
            let mut builder = FunctionBuilder::new(&mut projection.func, &mut frontend);
            #[cfg(test)]
            if let Some(pattern) = leaf_pattern {
                if selection.cell_kernel {
                    super::projection::lowering::emit_leaf_cell_helper(
                        &mut builder,
                        kind,
                        pattern,
                    )?;
                } else {
                    super::projection::lowering::emit_leaf_helper(
                        &mut builder,
                        kind,
                        fallback_ref,
                        pattern,
                    )?;
                }
            } else {
                super::projection::lowering::emit_helper(&mut builder, kind, fallback_ref)?;
            }
            #[cfg(not(test))]
            super::projection::lowering::emit_helper(&mut builder, kind, fallback_ref)?;
            builder.seal_all_blocks();
            builder.finalize(module.isa().frontend_config());
        }
        if projection.func.signature != module.declarations().get_function_decl(id).signature {
            return Err(JitError::Compilation(
                "invalid projection helper signature".into(),
            ));
        }
        #[cfg(test)]
        if let Some(pattern) = leaf_pattern {
            if selection.cell_kernel {
                super::projection::lowering::verify_leaf_cell_helper(
                    &projection.func,
                    kind,
                    pattern,
                )?;
            } else {
                super::projection::lowering::verify_leaf_helper(
                    &projection.func,
                    kind,
                    fallback_ref,
                    pattern,
                )?;
            }
        } else {
            super::projection::lowering::verify_helper(&projection.func, kind, fallback_ref)?;
        }
        #[cfg(not(test))]
        super::projection::lowering::verify_helper(&projection.func, kind, fallback_ref)?;
        #[cfg(test)]
        cranelift_codegen::verify_function(&projection.func, module.isa())
            .map_err(|error| JitError::Compilation(error.to_string()))?;
        projection_instructions += projection
            .func
            .layout
            .blocks()
            .map(|block| projection.func.layout.block_insts(block).count())
            .sum::<usize>();
        projection_blocks += projection.func.layout.blocks().count();
        projection_contexts[index] = Some(projection);
    }
    expansion.verify_actual(
        instructions + projection_instructions,
        block_count + projection_blocks,
    )?;
    #[cfg(test)]
    if leaf_pattern.is_some() {
        expansion.verify_actual(
            instructions + projection_instructions + 32,
            block_count + projection_blocks + 16,
        )?;
        if failure == Failure::CorruptInlineName {
            let name = projection_contexts[1].as_ref().unwrap().func.name.clone();
            projection_contexts[0].as_mut().unwrap().func.name = name;
        }
        if failure == Failure::CorruptInlineSignature {
            projection_contexts[0]
                .as_mut()
                .unwrap()
                .func
                .signature
                .params[2]
                .extension = cranelift_codegen::ir::ArgumentExtension::Sext;
        }
        let mut inliner = LeafInliner {
            functions: std::array::from_fn(|index| {
                let kind = [abi::HELPER_GET_UPVALUE, abi::HELPER_SET_UPVALUE][index];
                let reference = helper_refs.iter().find(|(k, _)| *k == kind).unwrap().1;
                let function = &projection_contexts[index].as_ref().unwrap().func;
                (reference, function)
            }),
            counts: [0, 0],
        };
        context
            .inline(&mut inliner)
            .map_err(|error| JitError::Compilation(error.to_string()))?;
        if inliner.counts != [1, 1] {
            return Err(JitError::Compilation(
                "invalid scalar-cell inline sites".into(),
            ));
        }
        cranelift_codegen::verify_function(&context.func, module.isa())
            .map_err(|error| JitError::Compilation(error.to_string()))?;
        if selection.cell_kernel
            && context.func.layout.blocks().any(|block| {
                context.func.layout.block_insts(block).any(|inst| {
                    matches!(
                        context.func.dfg.insts[inst].opcode(),
                        cranelift_codegen::ir::Opcode::Call
                            | cranelift_codegen::ir::Opcode::CallIndirect
                            | cranelift_codegen::ir::Opcode::ReturnCall
                            | cranelift_codegen::ir::Opcode::ReturnCallIndirect
                    )
                })
            })
        {
            return Err(JitError::Compilation(
                "scalar kernel contains a call".into(),
            ));
        }
        let instructions = context
            .func
            .layout
            .blocks()
            .map(|block| context.func.layout.block_insts(block).count())
            .sum::<usize>();
        expansion.verify_actual(
            instructions + projection_instructions,
            context.func.layout.blocks().count() + projection_blocks,
        )?;
    }
    #[cfg(test)]
    if selection.integer_activation {
        let plan = super::integer::Plan::new(snapshot)?;
        let _workspace = Reservation::new(snapshot.operations.allocator().0.clone(), 16 * 1024)
            .map_err(|_| JitError::ResourceLimit("integer template workspace"))?;
        let previous_instructions = context
            .func
            .layout
            .blocks()
            .map(|block| context.func.layout.block_insts(block).count())
            .sum::<usize>();
        let previous_blocks = context.func.layout.blocks().count();
        let signature = context.func.signature.clone();
        context.func = plan.function(
            context.func.name.clone(),
            signature.clone(),
            module.target_config(),
        )?;
        plan.verify(&context.func, signature, module.target_config())?;
        cranelift_codegen::verify_function(&context.func, module.isa())
            .map_err(|error| JitError::Compilation(error.to_string()))?;
        let instructions = context
            .func
            .layout
            .blocks()
            .map(|block| context.func.layout.block_insts(block).count())
            .sum::<usize>();
        expansion.verify_actual(
            previous_instructions
                .checked_add(
                    instructions
                        .checked_mul(2)
                        .ok_or(JitError::ResourceLimit("integer template instructions"))?,
                )
                .and_then(|count| count.checked_add(projection_instructions))
                .ok_or(JitError::ResourceLimit("integer template instructions"))?,
            previous_blocks
                .checked_add(
                    context
                        .func
                        .layout
                        .blocks()
                        .count()
                        .checked_mul(2)
                        .ok_or(JitError::ResourceLimit("integer template blocks"))?,
                )
                .and_then(|count| count.checked_add(projection_blocks))
                .ok_or(JitError::ResourceLimit("integer template blocks"))?,
        )?;
        for (pc, entry) in entries.iter_mut().enumerate() {
            *entry = pc == 0;
        }
    }
    #[cfg(test)]
    if let Failure::RequireReleasedWorkspace(baseline) | Failure::RequireSignatures(baseline) =
        failure
    {
        assert_eq!(
            snapshot.operations.allocator().0.current(),
            baseline + signature_bytes + symbol_bytes
        );
    }
    context
        .compile(module.isa(), &mut Default::default())
        .map_err(|error| fail(error.into()))?;
    let compiled = context.compiled_code().unwrap();
    let mut relocations = compiled.buffer.relocs().len();
    for projection in projection_contexts.iter_mut().flatten() {
        projection
            .compile(module.isa(), &mut Default::default())
            .map_err(|error| fail(error.into()))?;
        relocations = relocations
            .checked_add(projection.compiled_code().unwrap().buffer.relocs().len())
            .ok_or(JitError::ResourceLimit("native relocations"))?;
    }
    if relocations > work.relocations {
        return Err(JitError::ResourceLimit("native relocations"));
    }
    #[cfg(test)]
    if let Failure::RefuseRelocationStorage(allocation) = failure {
        let ledger = &snapshot.operations.allocator().0;
        if allocation {
            ledger.fail_after(0);
        } else {
            ledger.set_limit(ledger.current());
        }
    }
    let mut module_relocations = BudgetVec::new_in(snapshot.operations.allocator().clone());
    module_relocations
        .try_reserve_exact(compiled.buffer.relocs().len())
        .map_err(|_| JitError::ResourceLimit("native relocation staging"))?;
    module_relocations.extend(
        compiled
            .buffer
            .relocs()
            .iter()
            .map(|relocation| ModuleReloc::from_mach_reloc(relocation, &context.func, function)),
    );
    let relocation_copy_bytes = Layout::array::<ModuleReloc>(compiled.buffer.relocs().len())
        .map_err(|_| JitError::ResourceLimit("native relocation copy size"))?
        .size();
    #[cfg(test)]
    if failure == Failure::RefuseRelocationCopy {
        let ledger = &snapshot.operations.allocator().0;
        ledger.set_limit(ledger.current());
    }
    relocation_copy_charge = Reservation::new(
        snapshot.operations.allocator().0.clone(),
        relocation_copy_bytes,
    )
    .map_err(|_| JitError::ResourceLimit("native relocation copy"))?;
    #[cfg(test)]
    if let Failure::RequireRelocationCopy(baseline) = failure {
        assert_eq!(
            snapshot.operations.allocator().0.current(),
            baseline
                + signature_bytes
                + symbol_bytes
                + module_relocations.capacity() * std::mem::size_of::<ModuleReloc>()
                + relocation_copy_bytes
        );
    }
    module
        .define_function_bytes(
            function,
            u64::from(compiled.buffer.alignment),
            compiled.code_buffer(),
            &module_relocations,
        )
        .map_err(fail)?;
    drop(module_relocations);
    #[cfg(test)]
    if let Failure::RequireRelocationCopy(baseline) = failure {
        assert_eq!(
            snapshot.operations.allocator().0.current(),
            baseline + signature_bytes + symbol_bytes + relocation_copy_bytes
        );
    }
    #[cfg(test)]
    let mut byte_len = compiled.code_buffer().len();
    drop(context);
    for (index, projection) in projection_contexts.into_iter().enumerate() {
        let Some(projection) = projection else {
            continue;
        };
        let id = projection_ids[index].unwrap();
        let compiled = projection.compiled_code().unwrap();
        #[cfg(test)]
        {
            byte_len += compiled.code_buffer().len();
        }
        let count = compiled.buffer.relocs().len();
        let mut relocations = BudgetVec::new_in(snapshot.operations.allocator().clone());
        relocations
            .try_reserve_exact(count)
            .map_err(|_| JitError::ResourceLimit("projection relocation staging"))?;
        relocations.extend(
            compiled
                .buffer
                .relocs()
                .iter()
                .map(|relocation| ModuleReloc::from_mach_reloc(relocation, &projection.func, id)),
        );
        let bytes = Layout::array::<ModuleReloc>(count)
            .map_err(|_| JitError::ResourceLimit("projection relocation copy size"))?
            .size();
        let charge = Reservation::new(snapshot.operations.allocator().0.clone(), bytes)
            .map_err(|_| JitError::ResourceLimit("projection relocation copy"))?;
        #[cfg(test)]
        {
            projection_copy_bytes += bytes;
        }
        module
            .define_function_bytes(
                id,
                u64::from(compiled.buffer.alignment),
                compiled.code_buffer(),
                &relocations,
            )
            .map_err(fail)?;
        drop((relocations, projection));
        projection_copy_charges[index] = Some(charge);
    }
    #[cfg(test)]
    if let Failure::RequireSignatures(baseline) = failure {
        assert_eq!(
            snapshot.operations.allocator().0.current(),
            baseline
                + signature_bytes
                + symbol_bytes
                + relocation_copy_bytes
                + projection_copy_bytes
        );
    }
    module.finalize_definitions().map_err(fail)?;
    let entry =
        unsafe { std::mem::transmute::<*const u8, Entry>(module.get_finalized_function(function)) };
    let image = memory
        .take()
        .ok_or_else(|| JitError::Compilation("missing finalized native memory".into()))?;
    drop(module);
    drop(signature_charge);
    drop(symbol_charge);
    drop(relocation_copy_charge);
    drop(projection_copy_charges);
    #[cfg(test)]
    if let Failure::RequireRelocationCopy(baseline) | Failure::RequireSignatures(baseline) = failure
    {
        assert_eq!(snapshot.operations.allocator().0.current(), baseline);
    }
    drop(provider_charge);
    drop(memory);
    Ok(Code {
        _memory: image,
        entry,
        #[cfg(test)]
        byte_len,
        relocations,
        registers: snapshot.registers,
        entries,
        projected_upvalues: selection.projected && projection_count != 0,
        #[cfg(test)]
        continuations: None,
        #[cfg(test)]
        scalar_leaf: leaf_pattern,
        #[cfg(test)]
        cell_kernel: selection.cell_kernel,
        #[cfg(test)]
        integer_activation: selection.integer_activation,
        #[cfg(all(test, not(miri)))]
        scalar_kernel: None,
    })
}

#[cfg(test)]
struct LeafInliner<'a> {
    functions: [(
        cranelift_codegen::ir::FuncRef,
        &'a cranelift_codegen::ir::Function,
    ); 2],
    counts: [u8; 2],
}

#[cfg(test)]
impl cranelift_codegen::inline::Inline for LeafInliner<'_> {
    fn inline(
        &mut self,
        caller: &cranelift_codegen::ir::Function,
        _call: Inst,
        opcode: cranelift_codegen::ir::Opcode,
        callee: cranelift_codegen::ir::FuncRef,
        _args: &[IrValue],
    ) -> cranelift_codegen::inline::InlineCommand<'_> {
        for (index, &(reference, function)) in self.functions.iter().enumerate() {
            let name_matches = match caller.dfg.ext_funcs[callee].name {
                cranelift_codegen::ir::ExternalName::User(name) => {
                    function.name.get_user() == Some(&caller.params.user_named_funcs()[name])
                }
                _ => false,
            };
            if callee == reference
                && name_matches
                && opcode == cranelift_codegen::ir::Opcode::Call
                && caller.dfg.signatures[caller.dfg.ext_funcs[callee].signature]
                    == function.signature
            {
                self.counts[index] = self.counts[index].saturating_add(1);
                return cranelift_codegen::inline::InlineCommand::Inline {
                    callee: std::borrow::Cow::Borrowed(function),
                    visit_callee: false,
                };
            }
        }
        cranelift_codegen::inline::InlineCommand::KeepCall
    }
}

fn native_builder(
    isa: Result<cranelift_codegen::isa::Builder, &'static str>,
) -> Result<JITBuilder, JitError> {
    let isa = isa.map_err(JitError::Unavailable)?;
    let mut flags = settings::builder();
    for (name, value) in [
        ("opt_level", "speed"),
        ("enable_verifier", "true"),
        ("use_colocated_libcalls", "false"),
        (
            "is_pic",
            if cfg!(target_arch = "x86_64") {
                "true"
            } else {
                "false"
            },
        ),
    ] {
        flags
            .set(name, value)
            .map_err(|error| JitError::Compilation(error.to_string()))?;
    }
    let isa = isa
        .finish(settings::Flags::new(flags))
        .map_err(|error| JitError::Compilation(error.to_string()))?;
    Ok(JITBuilder::with_isa(isa, default_libcall_names()))
}

fn helper_needed(kind: u32, snapshot: &Snapshot) -> bool {
    snapshot
        .operations
        .iter()
        .any(|operation| match (kind, *operation) {
            (abi::HELPER_MOVE, Operation::Move { .. })
            | (abi::HELPER_NEW_TABLE, Operation::NewTable { .. })
            | (abi::HELPER_GET_TABLE, Operation::GetTable { .. })
            | (abi::HELPER_SET_TABLE, Operation::SetTable { .. })
            | (abi::HELPER_GET_UP_TABLE, Operation::GetUpTable { .. })
            | (abi::HELPER_SET_UP_TABLE, Operation::SetUpTable { .. })
            | (abi::HELPER_GET_UPVALUE, Operation::GetUpValue { .. })
            | (abi::HELPER_SET_UPVALUE, Operation::SetUpValue { .. }) => true,
            (abi::HELPER_CONSTANT, Operation::LoadConstant { constant, .. }) => snapshot
                .constants
                .get(usize::from(constant.0))
                .is_some_and(|slot| slot.tag == abi::REFERENCE),
            (abi::HELPER_SET_LIST, Operation::SetList { count, .. }) => !count.is_variable(),
            _ => false,
        })
}

fn symbol_storage_bytes(lengths: impl IntoIterator<Item = usize>) -> Result<usize, JitError> {
    let total = lengths
        .into_iter()
        .try_fold((0usize, 0usize), |(total, largest), length| {
            Layout::array::<u8>(length).ok()?;
            Some((total.checked_add(length)?, largest.max(length)))
        })
        .and_then(|(total, largest)| total.checked_mul(3)?.checked_add(largest));
    total.ok_or(JitError::ResourceLimit("native symbol size"))
}

fn owned_symbol(name: &str) -> Result<String, JitError> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(name.len())
        .map_err(|_| JitError::ResourceLimit("native symbol allocation"))?;
    if owned.capacity() != name.len() {
        return Err(JitError::ResourceLimit("native symbol capacity"));
    }
    owned.push_str(name);
    Ok(owned)
}

fn signature_storage_bytes(
    entry: usize,
    helper_parameters: usize,
    helper_returns: usize,
    helpers: usize,
) -> Result<usize, JitError> {
    let bytes = |count| {
        Layout::array::<AbiParam>(count)
            .ok()
            .map(|layout| layout.size())
    };
    let total = bytes(entry).and_then(|entry| {
        let helper = bytes(helper_parameters)?.checked_add(bytes(helper_returns)?)?;
        let copies = helpers.checked_mul(2)?.checked_add(1)?;
        entry
            .checked_mul(2)?
            .checked_add(helper.checked_mul(copies)?)
    });
    total.ok_or(JitError::ResourceLimit("native signature size"))
}

fn fill_signature<const N: usize>(
    parameters: &mut Vec<AbiParam>,
    types: [cranelift_codegen::ir::Type; N],
) -> Result<(), JitError> {
    if !parameters.is_empty() {
        return Err(JitError::ResourceLimit("native signature capacity"));
    }
    parameters
        .try_reserve_exact(N)
        .map_err(|_| JitError::ResourceLimit("native signature allocation"))?;
    if parameters.capacity() != N {
        return Err(JitError::ResourceLimit("native signature capacity"));
    }
    parameters.extend(types.map(AbiParam::new));
    Ok(())
}

#[cfg(test)]
mod relocation_tests;

#[cfg(test)]
mod lifetime_tests;

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
        let args = [a, b, c, self.pc as u32]
            .map(|arg| self.builder.ins().iconst(types::I32, i64::from(arg)));
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
        self.stores.record_at(
            self.pc,
            inst,
            self.graph.nodes[self.pc].access.scalar_tags(),
        );
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
        self.stores.float_input_at(
            self.pc,
            self.builder.func.dfg.value_def(result).unwrap_inst(),
            tag,
            bits,
        );
        result
    }

    fn numeric_input(&mut self, point: IrValue, tag: IrValue, bits: IrValue) {
        self.stores.numeric_input_at(
            self.pc,
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
            SetList { base, count } if !count.is_variable() => self.helper(
                abi::HELPER_SET_LIST,
                u32::from(base.0),
                u32::from(count.to_constant().unwrap()),
                0,
            ),
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

    pub(super) fn operations() -> [Operation; 10] {
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
            Operation::SetList {
                base: R(0),
                count: VarCount::constant(2),
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
        let imports: [_; helpers::SYMBOLS.len()] = std::array::from_fn(|index| {
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
mod ownership_tests {
    use super::super::{
        entry_flow::{Paths, Point},
        tags::BindingFault,
    };
    use super::*;
    use crate::types::{
        ConstantIndex16 as C16, ConstantIndex8 as C8, Opt254, RegisterIndex as R,
        UpValueIndex as U, VarCount,
    };

    fn arithmetic() -> Operation {
        Operation::Add {
            dest: R(0),
            left: R(1).into(),
            right: RCIndex::Constant(C8(0)),
        }
    }
    fn operation(fault: BindingFault) -> Operation {
        use BindingFault::*;
        match fault {
            InputPc | InputValue | ArithmeticPc => arithmetic(),
            TruthPc => Operation::Not {
                dest: R(0),
                source: R(1),
            },
            ComparisonPc => Operation::Less {
                left: R(1).into(),
                right: RCIndex::Constant(C8(0)),
                skip_if: false,
            },
            PrepPc => Operation::NumericForPrep {
                base: R(0),
                jump: 0,
            },
            LoopPc => Operation::NumericForLoop {
                base: R(0),
                jump: 0,
            },
            HelperPc => Operation::NewTable {
                dest: R(0),
                array_size: 0,
                map_size: 0,
            },
            _ => Operation::LoadBool {
                dest: R(0),
                value: true,
                skip_next: false,
            },
        }
    }

    fn fixture(op: Operation, fault: Option<BindingFault>) -> Result<(), JitError> {
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
                    bits: 42,
                },
                Slot {
                    tag: abi::REFERENCE,
                    bits: 0,
                },
            ]),
            registers: 8,
            upvalues: 1,
            prototypes: 0,
        };
        snapshot.verify().unwrap();
        let graph = super::super::flow::FlowGraph::new(&snapshot).unwrap();
        let ledger = snapshot.operations.allocator().0.clone();
        let baseline = ledger.current();
        let mut stores = super::super::tags::Stores::new(&graph, &snapshot).unwrap();
        let mut paths = Paths::new(&snapshot).unwrap();
        let mut function = cranelift_codegen::ir::Function::new();
        function
            .signature
            .params
            .extend([types::I64, types::I64].map(AbiParam::new));
        let mut signature = function.signature.clone();
        signature.params.extend([types::I32; 4].map(AbiParam::new));
        signature.returns.push(AbiParam::new(types::I32));
        let signature = function.import_signature(signature);
        let imports: [_; helpers::SYMBOLS.len()] = std::array::from_fn(|index| {
            let func = function.import_function(cranelift_codegen::ir::ExtFuncData {
                name: cranelift_codegen::ir::ExternalName::testcase(format!(
                    "owned_helper_{index}"
                )),
                signature,
                colocated: false,
                patchable: false,
            });
            (helpers::SYMBOLS[index].0, func)
        });
        let mut context = FunctionBuilderContext::new();
        let slots;
        {
            let mut builder = FunctionBuilder::new(&mut function, &mut context);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            slots = builder.block_params(entry)[0];
            let host = builder.block_params(entry)[1];
            let blocks = std::array::from_fn::<_, 3, _>(|_| builder.create_block());
            for block in blocks {
                builder.append_block_param(block, types::I32);
            }
            let fallback = builder.create_block();
            let guard = builder.create_block();
            let panicked = builder.create_block();
            for block in [fallback, guard, panicked] {
                builder.append_block_param(block, types::I64);
                builder.append_block_param(block, types::I32);
            }
            for body in blocks {
                paths.record(Point {
                    body,
                    trampoline: entry,
                    region: [0, 0],
                });
            }
            let zero = builder.ins().iconst(types::I32, 0);
            builder.ins().jump(blocks[0], &[zero.into()]);
            builder.switch_to_block(blocks[0]);
            let count = builder.block_params(blocks[0])[0];
            let start = builder.func.dfg.num_blocks() as u32;
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
                count,
                pc: 0,
                written: false,
                stores: &mut stores,
                omit_numeric_guards: false,
            };
            emitter.emit(op);
            let end = emitter.builder.func.dfg.num_blocks() as u32;
            paths.region(0, start, end);
            for pc in [1, 2] {
                paths.region(pc, end, end);
            }
            for block in blocks
                .into_iter()
                .skip(1)
                .chain([fallback, guard, panicked])
            {
                builder.switch_to_block(block);
                builder.ins().return_(&[]);
            }
            builder.seal_all_blocks();
            builder.finalize(
                cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
                    .unwrap()
                    .finish(cranelift_codegen::settings::Flags::new(
                        cranelift_codegen::settings::builder(),
                    ))
                    .unwrap()
                    .frontend_config(),
            );
        }
        stores
            .verify(&function, slots, None, snapshot.registers)
            .unwrap();
        if let Some(fault) = fault {
            stores.corrupt_binding(&mut function, slots, fault);
        }
        if fault == Some(BindingFault::ExtraPayload) {
            stores
                .verify(&function, slots, None, snapshot.registers)
                .unwrap();
        }
        cranelift_codegen::verify_function(
            &function,
            &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
        )
        .unwrap();
        let result = stores.verify_bindings(&function, slots, &paths, &graph);
        drop(stores);
        drop(paths);
        assert_eq!(ledger.current(), baseline);
        result
    }

    #[test]
    fn all_emitted_record_families_belong_to_their_source_region() {
        let binary = |kind| match kind {
            0 => arithmetic(),
            1 => Operation::Sub {
                dest: R(0),
                left: R(1).into(),
                right: RCIndex::Constant(C8(0)),
            },
            2 => Operation::Mul {
                dest: R(0),
                left: R(1).into(),
                right: RCIndex::Constant(C8(0)),
            },
            _ => Operation::Div {
                dest: R(0),
                left: R(1).into(),
                right: RCIndex::Constant(C8(0)),
            },
        };
        let operations = [
            Operation::Move {
                dest: R(0),
                source: R(1),
            },
            Operation::LoadConstant {
                dest: R(0),
                constant: C16(0),
            },
            Operation::LoadConstant {
                dest: R(0),
                constant: C16(1),
            },
            Operation::LoadBool {
                dest: R(0),
                value: true,
                skip_next: true,
            },
            Operation::LoadNil {
                dest: R(0),
                count: 2,
            },
            Operation::Jump {
                offset: 0,
                close_upvalues: Opt254::none(),
            },
            Operation::Not {
                dest: R(0),
                source: R(1),
            },
            Operation::Test {
                value: R(1),
                is_true: false,
            },
            binary(0),
            binary(1),
            binary(2),
            binary(3),
            Operation::Eq {
                left: R(1).into(),
                right: RCIndex::Constant(C8(0)),
                skip_if: false,
            },
            operation(BindingFault::ComparisonPc),
            Operation::LessEq {
                left: R(1).into(),
                right: RCIndex::Constant(C8(0)),
                skip_if: false,
            },
            operation(BindingFault::PrepPc),
            operation(BindingFault::LoopPc),
            operation(BindingFault::HelperPc),
            Operation::GetTable {
                dest: R(0),
                table: R(1),
                key: RCIndex::Constant(C8(0)),
            },
            Operation::SetTable {
                table: R(0),
                key: RCIndex::Constant(C8(0)),
                value: R(1).into(),
            },
            Operation::GetUpTable {
                dest: R(0),
                table: U(0),
                key: RCIndex::Constant(C8(0)),
            },
            Operation::SetUpTable {
                table: U(0),
                key: RCIndex::Constant(C8(0)),
                value: R(1).into(),
            },
            Operation::GetUpValue {
                dest: R(0),
                source: U(0),
            },
            Operation::SetUpValue {
                dest: U(0),
                source: R(1),
            },
        ];
        for op in operations {
            fixture(op, None).unwrap();
        }
    }
    macro_rules! rejected {
        ($name:ident, $fault:ident) => {
            #[test]
            fn $name() {
                let result = fixture(operation(BindingFault::$fault), Some(BindingFault::$fault));
                assert!(matches!(result, Err(JitError::Compilation(ref message))
                    if message == "invalid semantic source ownership"), "{result:?}");
            }
        };
    }
    rejected!(store_pc_is_refused, StorePc);
    rejected!(store_mask_is_refused, StoreMask);
    rejected!(destination_is_refused, Destination);
    rejected!(input_pc_is_refused, InputPc);
    rejected!(input_value_is_refused, InputValue);
    rejected!(arithmetic_pc_is_refused, ArithmeticPc);
    rejected!(truth_pc_is_refused, TruthPc);
    rejected!(comparison_pc_is_refused, ComparisonPc);
    rejected!(prep_pc_is_refused, PrepPc);
    rejected!(loop_pc_is_refused, LoopPc);
    rejected!(transfer_write_pc_is_refused, TransferWritePc);
    rejected!(transfer_edge_pc_is_refused, TransferEdgePc);
    rejected!(helper_pc_is_refused, HelperPc);
    rejected!(extra_payload_is_refused, ExtraPayload);
}

#[cfg(test)]
mod memory_tests {
    use super::*;
    fn refuse_corrupted_binding(fault: super::super::tags::BindingFault) {
        use super::super::tags::BindingFault::*;
        let source: &[u8] = match fault {
            TruthPc => b"local x=1 local a=not x if x then a=false end return a",
            ComparisonPc => b"local x=1 if x<2 then return 1 end return 2",
            PrepPc | LoopPc => b"local s=0 for i=1,3 do s=s+i end return s",
            HelperPc => b"local t={} t.x=42 return t.x",
            _ => b"local x=1 local a=x+1 local b=x+2 return a+b",
        };
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype =
                crate::FunctionPrototype::compile(ctx, "binding-corruption", source).unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let ledger = snapshot.operations.allocator().0.clone();
        let baseline = ledger.current();
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
        let result = compile_in(
            &snapshot,
            total.clone(),
            8 * 1024 * 1024,
            BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            Failure::CorruptBinding(fault),
        );
        assert!(
            matches!(result, Err(JitError::Compilation(ref message))
            if message == "invalid semantic source ownership"),
            "{:?}",
            result.as_ref().err()
        );
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(ledger.current(), baseline);
    }
    macro_rules! binding_corruption {
        ($name:ident, $fault:ident) => {
            #[test]
            fn $name() {
                refuse_corrupted_binding(super::super::tags::BindingFault::$fault);
            }
        };
    }
    binding_corruption!(
        corrupted_binding_store_pc_is_refused_before_codegen_and_mapping,
        StorePc
    );
    binding_corruption!(
        corrupted_binding_store_mask_is_refused_before_codegen_and_mapping,
        StoreMask
    );
    binding_corruption!(
        corrupted_binding_destination_is_refused_before_codegen_and_mapping,
        Destination
    );
    binding_corruption!(
        corrupted_binding_input_pc_is_refused_before_codegen_and_mapping,
        InputPc
    );
    binding_corruption!(
        corrupted_binding_input_value_is_refused_before_codegen_and_mapping,
        InputValue
    );
    binding_corruption!(
        corrupted_binding_arithmetic_pc_is_refused_before_codegen_and_mapping,
        ArithmeticPc
    );
    binding_corruption!(
        corrupted_binding_truth_pc_is_refused_before_codegen_and_mapping,
        TruthPc
    );
    binding_corruption!(
        corrupted_binding_comparison_pc_is_refused_before_codegen_and_mapping,
        ComparisonPc
    );
    binding_corruption!(
        corrupted_binding_prep_pc_is_refused_before_codegen_and_mapping,
        PrepPc
    );
    binding_corruption!(
        corrupted_binding_loop_pc_is_refused_before_codegen_and_mapping,
        LoopPc
    );
    binding_corruption!(
        corrupted_binding_transfer_write_pc_is_refused_before_codegen_and_mapping,
        TransferWritePc
    );
    binding_corruption!(
        corrupted_binding_transfer_edge_pc_is_refused_before_codegen_and_mapping,
        TransferEdgePc
    );
    binding_corruption!(
        corrupted_binding_helper_pc_is_refused_before_codegen_and_mapping,
        HelperPc
    );
    binding_corruption!(
        corrupted_binding_extra_payload_is_refused_before_codegen_and_mapping,
        ExtraPayload
    );

    fn refuse_region_failure(failure: Failure, quota: bool) {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype = crate::FunctionPrototype::compile(
                ctx,
                "region-flow-corruption",
                b"local a=1 local b=a return b",
            )
            .unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let ledger = snapshot.operations.allocator().0.clone();
        let baseline = ledger.current();
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
        let result = compile_in(
            &snapshot,
            total.clone(),
            8 * 1024 * 1024,
            BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            failure,
        );
        if quota {
            assert!(
                matches!(
                    result,
                    Err(JitError::ResourceLimit("source path verification"))
                ),
                "{:?}",
                result.as_ref().err()
            );
            assert_eq!(ledger.refusals(), 1);
        } else {
            assert!(
                matches!(result, Err(JitError::Compilation(ref message))
                if message == "invalid source region data flow"),
                "{:?}",
                result.as_ref().err()
            );
        }
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(ledger.current(), baseline);
    }
    #[test]
    fn corrupted_region_flow_workspace_quota_is_refused_before_mapping() {
        refuse_region_failure(Failure::RefuseRegionWorkspace, true);
    }
    macro_rules! region_flow_corruption {
        ($name:ident, $fault:ident) => {
            #[test]
            fn $name() {
                refuse_region_failure(
                    Failure::CorruptRegionFlow(super::super::entry_flow::RegionFault::$fault),
                    false,
                );
            }
        };
    }
    region_flow_corruption!(
        corrupted_region_flow_gap_is_refused_before_codegen_and_mapping,
        Gap
    );
    region_flow_corruption!(
        corrupted_region_flow_end_is_refused_before_codegen_and_mapping,
        End
    );
    region_flow_corruption!(
        corrupted_region_flow_missing_is_refused_before_codegen_and_mapping,
        Missing
    );
    region_flow_corruption!(
        corrupted_region_flow_fuel_is_refused_before_codegen_and_mapping,
        Fuel
    );
    region_flow_corruption!(
        corrupted_region_flow_successor_is_refused_before_codegen_and_mapping,
        Successor
    );
    region_flow_corruption!(
        corrupted_region_flow_retry_is_refused_before_codegen_and_mapping,
        Retry
    );
    region_flow_corruption!(
        corrupted_region_flow_pc_is_refused_before_codegen_and_mapping,
        Pc
    );
    region_flow_corruption!(
        corrupted_region_flow_count_is_refused_before_codegen_and_mapping,
        Count
    );
    region_flow_corruption!(
        corrupted_region_flow_kind_is_refused_before_codegen_and_mapping,
        Kind
    );
    region_flow_corruption!(
        corrupted_region_flow_root_is_refused_before_codegen_and_mapping,
        Root
    );
    region_flow_corruption!(
        corrupted_region_flow_cycle_is_refused_before_codegen_and_mapping,
        Cycle
    );
    region_flow_corruption!(
        corrupted_region_flow_cross_is_refused_before_codegen_and_mapping,
        Cross
    );
    region_flow_corruption!(
        corrupted_region_flow_unreachable_is_refused_before_codegen_and_mapping,
        Unreachable
    );

    #[test]
    fn high_and_invalid_entry_pcs_preserve_pc_and_slots_without_work() {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype =
                crate::FunctionPrototype::compile(ctx, "high-pc", b"return 42").unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
        let code = compile(&snapshot, total.clone(), 8 * 1024 * 1024).unwrap();
        for pc in [
            snapshot.operations.len(),
            u32::MAX as usize,
            u32::MAX as usize + 1,
            usize::MAX,
        ] {
            for budget in [0, 1, 64, u32::MAX] {
                let mut slots = vec![
                    Slot {
                        tag: abi::INTEGER,
                        bits: 123
                    };
                    snapshot.registers
                ];
                let exit = code.invoke(&mut slots, pc, budget);
                assert_eq!(exit.pc, pc as u64);
                assert_eq!(exit.instructions, 0);
                assert_eq!(exit.reason, ExitKind::Interpreter as u32);
                assert!(slots
                    .iter()
                    .all(|slot| slot.tag == abi::INTEGER && slot.bits == 123));
            }
        }
        drop(code);
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn predecessor_quota_refuses_before_codegen_and_releases_storage() {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype = crate::FunctionPrototype::compile(
                ctx,
                "predecessor-quota",
                b"local x=1 local y=x+2 return y",
            )
            .unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let ledger = snapshot.operations.allocator().0.clone();
        let baseline = ledger.current();
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
        let result = compile_in(
            &snapshot,
            total.clone(),
            8 * 1024 * 1024,
            BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            Failure::RefusePredecessors,
        );
        assert!(
            matches!(
                result,
                Err(JitError::ResourceLimit("frontend predecessor graph"))
            ),
            "{:?}",
            result.as_ref().err()
        );
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(ledger.current(), baseline);
        assert_eq!(ledger.refusals(), 1);
    }

    fn dominance_refusal(failure: Failure, expected: &'static str, refusals: usize) {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype = crate::FunctionPrototype::compile(
                ctx,
                "dominance-quota",
                b"local x=1 local y=x+2 return y",
            )
            .unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let ledger = snapshot.operations.allocator().0.clone();
        let baseline = ledger.current();
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
        let result = compile_in(
            &snapshot,
            total.clone(),
            8 * 1024 * 1024,
            BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            failure,
        );
        assert!(
            matches!(result, Err(JitError::ResourceLimit(reason)) if reason == expected),
            "{:?}",
            result.as_ref().err()
        );
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(ledger.current(), baseline);
        assert_eq!(ledger.refusals(), refusals);
    }

    #[test]
    fn dominance_storage_refuses_before_codegen_and_releases_storage() {
        dominance_refusal(
            Failure::RefuseDominanceStorage,
            "frontend dominance storage",
            1,
        );
    }

    #[test]
    fn dominance_work_refuses_before_codegen_and_releases_storage() {
        dominance_refusal(Failure::RefuseDominanceWork, "frontend dominance work", 0);
    }

    #[test]
    fn entry_path_quota_refuses_before_host_setup_and_releases_storage() {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype =
                crate::FunctionPrototype::compile(ctx, "entry-path-quota", b"return 1").unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let ledger = snapshot.operations.allocator().0.clone();
        let baseline = ledger.current();
        let graph = super::super::flow::FlowGraph::new(&snapshot).unwrap();
        let stores = super::super::tags::Stores::new(&graph, &snapshot).unwrap();
        let limit = ledger.current() + snapshot.operations.len() * std::mem::size_of::<Block>();
        drop(stores);
        drop(graph);
        ledger.set_limit(limit);
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
        let result = compile_in(
            &snapshot,
            total.clone(),
            8 * 1024 * 1024,
            BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            Failure::None,
        );
        assert!(
            matches!(
                result,
                Err(JitError::ResourceLimit("entry path verification"))
            ),
            "{:?}",
            result.as_ref().err()
        );
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(ledger.current(), baseline);
        assert_eq!(ledger.refusals(), 1);
    }
    fn refuse_corrupted_entry_flow(fault: super::super::entry_flow::Fault) {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype =
                crate::FunctionPrototype::compile(ctx, "entry-flow-corruption", b"return 42")
                    .unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
        let result = compile_in(
            &snapshot,
            total.clone(),
            8 * 1024 * 1024,
            BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            Failure::CorruptEntryFlow(fault),
        );
        assert!(
            matches!(result, Err(JitError::Compilation(ref message))
            if message == "invalid entry or budget data flow"),
            "{:?}",
            result.as_ref().err()
        );
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }
    macro_rules! entry_flow_corruption {
        ($name:ident, $fault:ident) => {
            #[test]
            fn $name() {
                refuse_corrupted_entry_flow(super::super::entry_flow::Fault::$fault);
            }
        };
    }
    entry_flow_corruption!(
        corrupted_entry_flow_entry_length_is_refused_before_codegen_and_mapping,
        EntryLength
    );
    entry_flow_corruption!(
        corrupted_entry_flow_entry_polarity_is_refused_before_codegen_and_mapping,
        EntryPolarity
    );
    entry_flow_corruption!(
        corrupted_entry_flow_entry_source_is_refused_before_codegen_and_mapping,
        EntrySource
    );
    entry_flow_corruption!(
        corrupted_entry_flow_dispatch_source_is_refused_before_codegen_and_mapping,
        DispatchSource
    );
    entry_flow_corruption!(
        corrupted_entry_flow_dispatch_index_is_refused_before_codegen_and_mapping,
        DispatchIndex
    );
    entry_flow_corruption!(
        corrupted_entry_flow_table_target_is_refused_before_codegen_and_mapping,
        TableTarget
    );
    entry_flow_corruption!(
        corrupted_entry_flow_table_default_is_refused_before_codegen_and_mapping,
        TableDefault
    );
    entry_flow_corruption!(
        corrupted_entry_flow_initial_count_is_refused_before_codegen_and_mapping,
        InitialCount
    );
    entry_flow_corruption!(
        corrupted_entry_flow_trampoline_target_is_refused_before_codegen_and_mapping,
        TrampolineTarget
    );
    entry_flow_corruption!(
        corrupted_entry_flow_unknown_pc_is_refused_before_codegen_and_mapping,
        UnknownPc
    );
    entry_flow_corruption!(
        corrupted_entry_flow_unknown_count_is_refused_before_codegen_and_mapping,
        UnknownCount
    );
    entry_flow_corruption!(
        corrupted_entry_flow_budget_pc_is_refused_before_codegen_and_mapping,
        BudgetPc
    );
    entry_flow_corruption!(
        corrupted_entry_flow_budget_condition_is_refused_before_codegen_and_mapping,
        BudgetCondition
    );
    entry_flow_corruption!(
        corrupted_entry_flow_budget_operands_is_refused_before_codegen_and_mapping,
        BudgetOperands
    );
    entry_flow_corruption!(
        corrupted_entry_flow_budget_target_is_refused_before_codegen_and_mapping,
        BudgetTarget
    );
    entry_flow_corruption!(
        corrupted_entry_flow_budget_count_is_refused_before_codegen_and_mapping,
        BudgetCount
    );
    entry_flow_corruption!(
        corrupted_entry_flow_budget_body_is_refused_before_codegen_and_mapping,
        BudgetBody
    );
    entry_flow_corruption!(
        corrupted_entry_flow_extra_body_entry_is_refused_before_codegen_and_mapping,
        ExtraBodyEntry
    );
    entry_flow_corruption!(
        corrupted_entry_flow_header_store_is_refused_before_codegen_and_mapping,
        HeaderStore
    );
    entry_flow_corruption!(
        corrupted_entry_flow_missing_is_refused_before_codegen_and_mapping,
        Missing
    );
    entry_flow_corruption!(
        corrupted_entry_flow_growth_is_refused_before_codegen_and_mapping,
        Growth
    );
    entry_flow_corruption!(
        corrupted_entry_flow_root_is_refused_before_codegen_and_mapping,
        Root
    );

    fn refuse_corrupted_exit_flow(fault: super::super::exit_flow::Fault) {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype =
                crate::FunctionPrototype::compile(ctx, "exit-flow-corruption", b"return 42")
                    .unwrap();
            Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
        });
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
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
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
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
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
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
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
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
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
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
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
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
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
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
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
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
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
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
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
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
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
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
            let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
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

    #[test]
    fn finalized_image_retains_only_charged_runtime_storage() {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype =
                crate::FunctionPrototype::compile(ctx, "detached", b"return 42").unwrap();
            Snapshot::new(&prototype, 4096, 4096).unwrap()
        });
        let host = super::super::resources::Ledger::new(2 * 1024 * 1024);
        let metadata = super::super::resources::Ledger::child(65536, host.clone());
        let total = MappingCounter::new(host.clone());
        let code = compile_in(
            &snapshot,
            total.clone(),
            1024 * 1024,
            BudgetAllocator(metadata.clone()),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            Failure::None,
        )
        .unwrap();
        let retained = code.entries.capacity() * std::mem::size_of::<bool>()
            + AtomicShared::<MemoryStatus>::allocation_bytes()
            + code._memory.allocations.capacity() * std::mem::size_of::<Segment>();
        assert_eq!(metadata.current(), retained);
        let mapped = code
            ._memory
            .allocations
            .iter()
            .map(|segment| segment.bytes)
            .sum::<usize>();
        assert!(mapped > 0);
        assert_eq!(
            (host.current(), total.load(Ordering::Relaxed)),
            (retained + mapped, mapped)
        );
        assert!(segment_permissions(code.entry as *const u8)
            .unwrap()
            .starts_with("r-x"));
        let mut slots = vec![
            Slot {
                tag: abi::NIL,
                bits: 0
            };
            code.registers
        ];
        let exit = code.invoke(&mut slots, 0, 64);
        assert!(exit.instructions > 0);
        let Operation::Return { start, .. } = snapshot.operations[exit.pc as usize] else {
            panic!("detached image did not reach return");
        };
        assert_eq!(
            (
                slots[usize::from(start.0)].tag,
                slots[usize::from(start.0)].bits
            ),
            (abi::INTEGER, 42)
        );
        drop(code);
        assert_eq!(
            (
                metadata.current(),
                host.current(),
                total.load(Ordering::Relaxed)
            ),
            (0, 0, 0)
        );
    }

    #[test]
    fn detached_provider_cannot_allocate_finalize_or_release_image() {
        let memory = memory(1);
        let total = memory.total.clone();
        let metadata = memory.allocations.allocator().0.clone();
        let slot = Handoff::try_new(memory, BudgetAllocator(metadata.clone())).unwrap();
        let mut provider = Provider(slot.clone());
        provider.allocate(1, 1, JITMemoryKind::Executable).unwrap();
        provider.finalize(BranchProtection::None).unwrap();
        let image = slot.take().unwrap();
        let mapped = total.load(Ordering::Relaxed);
        assert!(mapped > 0);
        assert!(provider.allocate(1, 1, JITMemoryKind::Executable).is_err());
        assert!(provider.finalize(BranchProtection::None).is_err());
        unsafe {
            provider.free_memory();
        }
        assert_eq!(total.load(Ordering::Relaxed), mapped);
        drop((provider, slot));
        assert_eq!(total.load(Ordering::Relaxed), mapped);
        assert_eq!(
            metadata.current(),
            image.allocations.capacity() * std::mem::size_of::<Segment>()
        );
        assert!(segment_permissions(image.allocations[0].base())
            .unwrap()
            .starts_with("r-x"));
        drop(image);
        assert_eq!((metadata.current(), total.load(Ordering::Relaxed)), (0, 0));
    }

    fn memory(pages: usize) -> Memory {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        assert!(page > 0);
        Memory {
            allocations: BudgetVec::new_in(BudgetAllocator(super::super::resources::Ledger::new(
                2 * 1024 * 1024,
            ))),
            total: MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX)),
            status: MemoryStatus::try_new(BudgetAllocator(super::super::resources::Ledger::new(
                4096,
            )))
            .unwrap(),
            failure: Failure::None,
            limit: pages * page as usize,
            page: page as usize,
        }
    }

    fn segment_permissions(pointer: *const u8) -> Option<String> {
        let address = pointer as usize;
        std::fs::read_to_string("/proc/self/maps")
            .unwrap()
            .lines()
            .find_map(|line| {
                let mut fields = line.split_whitespace();
                let (start, end) = fields.next()?.split_once('-')?;
                let start = usize::from_str_radix(start, 16).ok()?;
                let end = usize::from_str_radix(end, 16).ok()?;
                (start <= address && address < end).then(|| fields.next().unwrap().to_owned())
            })
    }

    #[test]
    fn requested_bytes_exclude_padding_and_survive_finalization_until_reclamation() {
        let mut memory = memory(16);
        let total = memory.total.clone();
        let mut requested = 0;
        for (size, align, kind) in [
            (0, 1, JITMemoryKind::Executable),
            (17, 4 * memory.page as u64, JITMemoryKind::ReadOnly),
            (memory.page + 1, 8, JITMemoryKind::Writable),
        ] {
            memory.allocate(size, align, kind).unwrap();
            requested += size;
            assert_eq!(total.requested(), requested);
            assert!(total.load(Ordering::Relaxed) > requested);
        }
        let mapped = total.load(Ordering::Relaxed);
        memory.finalize(BranchProtection::None).unwrap();
        memory.finalize(BranchProtection::None).unwrap();
        assert_eq!(total.requested(), requested);
        assert_eq!(total.load(Ordering::Relaxed), mapped);
        memory.release();
        assert_eq!((total.requested(), total.load(Ordering::Relaxed)), (0, 0));
        memory.release();
        assert_eq!(total.requested(), 0);
    }

    #[test]
    fn failed_mapping_and_protection_reclaim_requested_bytes_without_losing_peer() {
        for failure in [Failure::Allocate, Failure::Protect] {
            let mut peer = memory(4);
            peer.allocate(31, 1, JITMemoryKind::Writable).unwrap();
            let total = peer.total.clone();
            let baseline = total.load(Ordering::Relaxed);
            let mut candidate = memory(4);
            candidate.total = total.clone();
            candidate.failure = failure;
            let result = candidate.allocate(97, 1, JITMemoryKind::Executable);
            if failure == Failure::Allocate {
                assert!(result.is_err());
                assert_eq!(total.requested(), 31);
            } else {
                result.unwrap();
                assert_eq!(total.requested(), 128);
                assert!(candidate.finalize(BranchProtection::None).is_err());
            }
            drop(candidate);
            assert_eq!(
                (total.requested(), total.load(Ordering::Relaxed)),
                (31, baseline)
            );
            drop(peer);
            assert_eq!((total.requested(), total.load(Ordering::Relaxed)), (0, 0));
        }
    }

    #[test]
    fn segment_permissions_and_repeated_finalization_keep_record_storage_fixed() {
        let mut memory = memory(16);
        let metadata = memory.allocations.allocator().0.clone();
        let exec = memory.allocate(64, 8, JITMemoryKind::Executable).unwrap();
        let readonly = memory.allocate(64, 8, JITMemoryKind::ReadOnly).unwrap();
        let writable = memory.allocate(64, 8, JITMemoryKind::Writable).unwrap();
        for pointer in [exec, readonly, writable] {
            unsafe {
                pointer.write(0x5a);
            }
            assert!(segment_permissions(pointer).unwrap().starts_with("rw-"));
        }
        let baseline = (
            memory.allocations.len(),
            memory.allocations.capacity(),
            metadata.current(),
            memory.total.load(Ordering::Relaxed),
        );
        memory.finalize(BranchProtection::None).unwrap();
        assert!(segment_permissions(exec).unwrap().starts_with("r-x"));
        assert!(segment_permissions(readonly).unwrap().starts_with("r--"));
        assert!(segment_permissions(writable).unwrap().starts_with("rw-"));
        for pointer in [exec, readonly, writable] {
            assert_eq!(unsafe { pointer.read() }, 0x5a);
        }
        for _ in 0..16 {
            memory.finalize(BranchProtection::None).unwrap();
        }
        assert_eq!(
            (
                memory.allocations.len(),
                memory.allocations.capacity(),
                metadata.current(),
                memory.total.load(Ordering::Relaxed)
            ),
            baseline
        );
        memory.release();
        assert_eq!(
            (metadata.current(), memory.total.load(Ordering::Relaxed)),
            (0, 0)
        );
    }

    #[test]
    fn segment_over_alignment_charges_padding_and_preserves_peer_on_refusal() {
        let mut memory = memory(8);
        let align = memory.page * 4;
        let size = memory.page + 17;
        let pointer = memory
            .allocate(size, align as u64, JITMemoryKind::Writable)
            .unwrap();
        assert_eq!(pointer as usize % align, 0);
        assert_eq!(memory.total.load(Ordering::Relaxed), memory.page * 5);
        unsafe {
            pointer.write(42);
            pointer.add(size - 1).write(84);
        }
        assert!(memory
            .allocate(1, align as u64, JITMemoryKind::Writable)
            .is_err());
        assert_eq!(memory.allocations.len(), 1);
        assert_eq!(memory.total.load(Ordering::Relaxed), memory.page * 5);
        assert_eq!(
            unsafe { (pointer.read(), pointer.add(size - 1).read()) },
            (42, 84)
        );
        memory.release();
        assert_eq!(memory.total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn segment_partial_protection_failure_retains_every_mapping_until_reclamation() {
        let mut memory = memory(8);
        let metadata = memory.allocations.allocator().0.clone();
        let exec = memory.allocate(64, 8, JITMemoryKind::Executable).unwrap();
        let readonly = memory.allocate(64, 8, JITMemoryKind::ReadOnly).unwrap();
        let writable = memory.allocate(64, 8, JITMemoryKind::Writable).unwrap();
        let baseline = (metadata.current(), memory.total.load(Ordering::Relaxed));
        memory.failure = Failure::ProtectAfterFirst;
        assert!(memory.finalize(BranchProtection::None).is_err());
        assert!(memory.status.unavailable.load(Ordering::Relaxed));
        assert!(segment_permissions(exec).unwrap().starts_with("r-x"));
        assert!(segment_permissions(readonly).unwrap().starts_with("rw-"));
        assert!(segment_permissions(writable).unwrap().starts_with("rw-"));
        assert_eq!(
            (metadata.current(), memory.total.load(Ordering::Relaxed)),
            baseline
        );
        memory.failure = Failure::None;
        memory.finalize(BranchProtection::None).unwrap();
        assert!(segment_permissions(readonly).unwrap().starts_with("r--"));
        memory.release();
        assert_eq!(
            (metadata.current(), memory.total.load(Ordering::Relaxed)),
            (0, 0)
        );
    }

    #[test]
    fn segment_zero_request_maps_a_live_page_and_invalid_alignment_never_reserves() {
        let mut memory = memory(1);
        let metadata = memory.allocations.allocator().0.clone();
        for align in [0, 3] {
            assert!(memory.allocate(1, align, JITMemoryKind::Writable).is_err());
            assert_eq!(
                (metadata.current(), memory.total.load(Ordering::Relaxed)),
                (0, 0)
            );
        }
        let pointer = memory.allocate(0, 8, JITMemoryKind::Writable).unwrap();
        assert!(!pointer.is_null());
        assert_eq!(memory.total.load(Ordering::Relaxed), memory.page);
        unsafe {
            pointer.write(42);
        }
        memory.release();
        assert_eq!(
            (metadata.current(), memory.total.load(Ordering::Relaxed)),
            (0, 0)
        );
    }

    #[test]
    fn segment_record_underlying_refusal_precedes_mapping() {
        let mut memory = memory(4);
        let metadata = memory.allocations.allocator().0.clone();
        metadata.fail_after(0);
        assert!(memory.allocate(1, 8, JITMemoryKind::Executable).is_err());
        assert!(memory.status.metadata_refused.load(Ordering::Relaxed));
        assert_eq!(
            (
                memory.allocations.len(),
                metadata.current(),
                memory.total.load(Ordering::Relaxed)
            ),
            (0, 0, 0)
        );
        assert_eq!(metadata.refusals(), 1);
    }

    #[test]
    fn segment_reclamation_os_worker() {
        const FILTER: &str = "jit::backend::memory_tests::segment_reclamation_os_worker";
        if std::env::var_os("LUNA_SEGMENT_RECLAIM_WORKER").is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", FILTER, "--nocapture"])
                .env("LUNA_SEGMENT_RECLAIM_WORKER", "1")
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success());
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("segment worker timeout");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            return;
        }
        let mut memory = memory(16);
        let metadata = memory.allocations.allocator().0.clone();
        let pointer = memory
            .allocate(
                memory.page + 17,
                (memory.page * 4) as u64,
                JITMemoryKind::Executable,
            )
            .unwrap();
        assert!(segment_permissions(pointer).unwrap().starts_with("rw-"));
        memory.finalize(BranchProtection::None).unwrap();
        assert!(segment_permissions(pointer).unwrap().starts_with("r-x"));
        let base = memory.allocations[0].base() as usize;
        let bytes = memory.allocations[0].bytes;
        let mut resident = 0u8;
        assert_eq!(base % memory.page, 0);
        for offset in (0..bytes).step_by(memory.page) {
            assert_eq!(
                unsafe {
                    libc::mincore(
                        (base + offset) as *mut libc::c_void,
                        memory.page,
                        &mut resident,
                    )
                },
                0
            );
        }
        memory.release();
        for offset in (0..bytes).step_by(memory.page) {
            let result = unsafe {
                libc::mincore(
                    (base + offset) as *mut libc::c_void,
                    memory.page,
                    &mut resident,
                )
            };
            assert_eq!(result, -1);
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ENOMEM)
            );
        }
        assert_eq!(
            (metadata.current(), memory.total.load(Ordering::Relaxed)),
            (0, 0)
        );
        memory.release();
    }

    #[test]
    fn host_page_quota_preserves_prior_segment_and_releases_parent_charges() {
        let host = super::super::resources::Ledger::new(usize::MAX);
        let metadata = super::super::resources::Ledger::child(65536, host.clone());
        let mut memory = memory(4);
        memory.allocations = BudgetVec::new_in(BudgetAllocator(metadata.clone()));
        memory.allocations.try_reserve_exact(2).unwrap();
        let baseline = host.current();
        host.set_limit(baseline + memory.page);
        memory.allocate(1, 1, JITMemoryKind::Executable).unwrap();
        assert_eq!(host.current(), baseline + memory.page);
        assert_eq!(metadata.current(), baseline);
        assert!(memory.allocate(1, 1, JITMemoryKind::Executable).is_err());
        assert_eq!(memory.allocations.len(), 1);
        assert_eq!(memory.total.load(Ordering::Relaxed), memory.page);
        assert_eq!(host.current(), baseline + memory.page);
        assert_eq!(host.refusals(), 1);
        assert!(memory.status.quota_refused.load(Ordering::Relaxed));
        assert!(!memory.status.metadata_refused.load(Ordering::Relaxed));
        unsafe {
            memory.free_memory();
            memory.free_memory();
        }
        assert_eq!((host.current(), metadata.current()), (0, 0));
        assert_eq!(memory.total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn host_page_reservation_rolls_back_on_provider_denial() {
        let host = super::super::resources::Ledger::new(usize::MAX);
        let metadata = super::super::resources::Ledger::child(65536, host.clone());
        let mut memory = memory(4);
        memory.allocations = BudgetVec::new_in(BudgetAllocator(metadata.clone()));
        memory.failure = Failure::Allocate;
        assert!(memory.allocate(1, 1, JITMemoryKind::Executable).is_err());
        assert_eq!(memory.total.load(Ordering::Relaxed), 0);
        assert_eq!(host.current(), metadata.current());
        assert!(host.peak() >= metadata.current() + memory.page);
        assert_eq!(host.refusals(), 0);
        drop(memory);
        assert_eq!((host.current(), metadata.current()), (0, 0));
    }

    #[test]
    fn oversized_image_refuses_before_record_allocation_and_preserves_its_segments() {
        for case in 0..3 {
            let mut memory = memory(1);
            let metadata = memory.allocations.allocator().0.clone();
            let total = memory.total.clone();
            if case == 1 {
                memory.allocate(1, 1, JITMemoryKind::Executable).unwrap();
            }
            let baseline = (metadata.current(), total.load(Ordering::Relaxed));
            metadata.fail_after(0);
            memory.failure = Failure::Allocate;
            let (size, align) = match case {
                0 => (memory.page + 1, 1),
                1 => (1, 1),
                _ => (1, 4 * memory.page as u64),
            };
            assert!(memory
                .allocate(size, align, JITMemoryKind::ReadOnly)
                .is_err());
            assert!(matches!(
                memory.status.error(),
                Some(JitError::ResourceLimit("native image size"))
            ));
            assert!(!memory.status.metadata_refused.load(Ordering::Relaxed));
            assert!(!memory.status.unavailable.load(Ordering::Relaxed));
            assert_eq!(memory.allocations.len(), usize::from(case == 1));
            assert_eq!(total.requested(), usize::from(case == 1));
            assert_eq!(
                (metadata.current(), total.load(Ordering::Relaxed)),
                baseline
            );
            drop(memory);
            assert_eq!((metadata.current(), total.load(Ordering::Relaxed)), (0, 0));
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
        assert!(memory.status.quota_refused.load(Ordering::Relaxed));
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
        assert!(memory.status.quota_refused.load(Ordering::Relaxed));
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
        assert!(memory.status.metadata_refused.load(Ordering::Relaxed));
        assert!(!memory.status.quota_refused.load(Ordering::Relaxed));
        assert_eq!(memory.total.load(Ordering::Relaxed), 0);
        assert_eq!(metadata.current(), 0);
        metadata.set_limit(4096);
        memory
            .status
            .metadata_refused
            .store(false, Ordering::Relaxed);
        memory.allocate(1, 1, JITMemoryKind::Executable).unwrap();
        let retained = metadata.current();
        metadata.set_limit(retained);
        assert!(memory.allocate(1, 1, JITMemoryKind::Executable).is_err());
        assert!(memory.status.metadata_refused.load(Ordering::Relaxed));
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
        let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
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
    fn status_refusal_precedes_host_setup_and_releases_entry_storage() {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype = crate::FunctionPrototype::compile(ctx, "status", b"return 42").unwrap();
            Snapshot::new(&prototype, 4096, 4096).unwrap()
        });
        let snapshot_ledger = snapshot.operations.allocator().0.clone();
        let snapshot_baseline = snapshot_ledger.current();
        let entry_bytes = snapshot.operations.len();
        let owner_bytes = AtomicShared::<MemoryStatus>::allocation_bytes();
        for cause in 0..3 {
            let host = super::super::resources::Ledger::new(if cause == 1 {
                entry_bytes + owner_bytes - 1
            } else {
                65536
            });
            let metadata = super::super::resources::Ledger::child(
                if cause == 0 {
                    entry_bytes + owner_bytes - 1
                } else {
                    65536
                },
                host.clone(),
            );
            if cause == 2 {
                metadata.fail_after(1);
            }
            let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
            assert!(matches!(
                compile_in(
                    &snapshot,
                    total.clone(),
                    65536,
                    BudgetAllocator(metadata.clone()),
                    super::super::work::Limits::from(&super::super::JitConfig::default()),
                    Failure::DetectHostSetup
                ),
                Err(JitError::ResourceLimit("JIT metadata"))
            ));
            assert_eq!(
                (
                    metadata.current(),
                    host.current(),
                    total.load(Ordering::Relaxed)
                ),
                (0, 0, 0)
            );
            assert_eq!(metadata.refusals(), 1);
            assert_eq!(snapshot_ledger.current(), snapshot_baseline);
        }
    }

    #[test]
    fn status_refusal_preserves_live_module_and_same_snapshot_recovers() {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype =
                crate::FunctionPrototype::compile(ctx, "status-peer", b"return 42").unwrap();
            Snapshot::new(&prototype, 4096, 4096).unwrap()
        });
        for cause in 0..3 {
            let host = super::super::resources::Ledger::new(2 * 1024 * 1024);
            let metadata = super::super::resources::Ledger::child(65536, host.clone());
            let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
            let compile_module = |failure| {
                compile_in(
                    &snapshot,
                    total.clone(),
                    1024 * 1024,
                    BudgetAllocator(metadata.clone()),
                    super::super::work::Limits::from(&super::super::JitConfig::default()),
                    failure,
                )
            };
            let peer = compile_module(Failure::None).unwrap();
            let baseline = (
                metadata.current(),
                host.current(),
                total.load(Ordering::Relaxed),
            );
            let admission =
                snapshot.operations.len() + AtomicShared::<MemoryStatus>::allocation_bytes() - 1;
            match cause {
                0 => metadata.set_limit(baseline.0 + admission),
                1 => host.set_limit(baseline.1 + admission),
                _ => metadata.fail_after(1),
            }
            assert!(matches!(
                compile_module(Failure::DetectHostSetup),
                Err(JitError::ResourceLimit("JIT metadata"))
            ));
            assert_eq!(
                (
                    metadata.current(),
                    host.current(),
                    total.load(Ordering::Relaxed)
                ),
                baseline
            );
            let mut slots = vec![
                Slot {
                    tag: abi::NIL,
                    bits: 0
                };
                peer.registers
            ];
            let exit = peer.invoke(&mut slots, 0, 64);
            assert!(exit.instructions > 0);
            let Operation::Return { start, .. } = snapshot.operations[exit.pc as usize] else {
                panic!("peer did not reach its interpreted return");
            };
            assert_eq!(
                (
                    slots[usize::from(start.0)].tag,
                    slots[usize::from(start.0)].bits
                ),
                (abi::INTEGER, 42)
            );
            metadata.set_limit(65536);
            host.set_limit(2 * 1024 * 1024);
            metadata.fail_after(usize::MAX);
            let recovered = compile_module(Failure::None).unwrap();
            assert!(recovered.invoke(&mut slots, 0, 64).instructions > 0);
            drop((peer, recovered));
            assert_eq!(
                (
                    metadata.current(),
                    host.current(),
                    total.load(Ordering::Relaxed)
                ),
                (0, 0, 0)
            );
        }
    }

    #[test]
    fn provider_box_refusal_precedes_host_setup_and_releases_entry_storage() {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype = crate::FunctionPrototype::compile(ctx, "status", b"return 42").unwrap();
            Snapshot::new(&prototype, 4096, 4096).unwrap()
        });
        let snapshot_ledger = snapshot.operations.allocator().0.clone();
        let snapshot_baseline = snapshot_ledger.current();
        let entry_bytes = snapshot.operations.len();
        let owner_bytes = AtomicShared::<MemoryStatus>::allocation_bytes()
            + Handoff::<Memory>::allocation_bytes()
            + std::alloc::Layout::new::<Provider>().size();
        for cause in 0..3 {
            let host = super::super::resources::Ledger::new(if cause == 1 {
                entry_bytes + owner_bytes - 1
            } else {
                65536
            });
            let metadata = super::super::resources::Ledger::child(
                if cause == 0 {
                    entry_bytes + owner_bytes - 1
                } else {
                    65536
                },
                host.clone(),
            );
            if cause == 2 {
                metadata.fail_after(3);
            }
            let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
            assert!(matches!(
                compile_in(
                    &snapshot,
                    total.clone(),
                    65536,
                    BudgetAllocator(metadata.clone()),
                    super::super::work::Limits::from(&super::super::JitConfig::default()),
                    Failure::DetectProviderSetup
                ),
                Err(JitError::ResourceLimit("JIT metadata"))
            ));
            assert_eq!(
                (
                    metadata.current(),
                    host.current(),
                    total.load(Ordering::Relaxed)
                ),
                (0, 0, 0)
            );
            assert_eq!(
                metadata.peak(),
                if cause == 2 {
                    entry_bytes + owner_bytes
                } else {
                    entry_bytes
                        + AtomicShared::<MemoryStatus>::allocation_bytes()
                        + Handoff::<Memory>::allocation_bytes()
                }
            );
            assert_eq!(metadata.refusals(), 1);
            assert_eq!(snapshot_ledger.current(), snapshot_baseline);
        }
    }

    #[test]
    fn provider_box_refusal_preserves_live_module_and_same_snapshot_recovers() {
        let mut lua = crate::Lua::empty();
        let snapshot = lua.enter(|ctx| {
            let prototype =
                crate::FunctionPrototype::compile(ctx, "status-peer", b"return 42").unwrap();
            Snapshot::new(&prototype, 4096, 4096).unwrap()
        });
        for cause in 0..3 {
            let host = super::super::resources::Ledger::new(2 * 1024 * 1024);
            let metadata = super::super::resources::Ledger::child(65536, host.clone());
            let total = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
            let compile_module = |failure| {
                compile_in(
                    &snapshot,
                    total.clone(),
                    1024 * 1024,
                    BudgetAllocator(metadata.clone()),
                    super::super::work::Limits::from(&super::super::JitConfig::default()),
                    failure,
                )
            };
            let peer = compile_module(Failure::None).unwrap();
            let baseline = (
                metadata.current(),
                host.current(),
                total.load(Ordering::Relaxed),
            );
            let admission = snapshot.operations.len()
                + AtomicShared::<MemoryStatus>::allocation_bytes()
                + Handoff::<Memory>::allocation_bytes()
                + std::alloc::Layout::new::<Provider>().size()
                - 1;
            match cause {
                0 => metadata.set_limit(baseline.0 + admission),
                1 => host.set_limit(baseline.1 + admission),
                _ => metadata.fail_after(3),
            }
            assert!(matches!(
                compile_module(Failure::DetectProviderSetup),
                Err(JitError::ResourceLimit("JIT metadata"))
            ));
            assert_eq!(
                (
                    metadata.current(),
                    host.current(),
                    total.load(Ordering::Relaxed)
                ),
                baseline
            );
            let mut slots = vec![
                Slot {
                    tag: abi::NIL,
                    bits: 0
                };
                peer.registers
            ];
            let exit = peer.invoke(&mut slots, 0, 64);
            assert!(exit.instructions > 0);
            let Operation::Return { start, .. } = snapshot.operations[exit.pc as usize] else {
                panic!("peer did not reach its interpreted return");
            };
            assert_eq!(
                (
                    slots[usize::from(start.0)].tag,
                    slots[usize::from(start.0)].bits
                ),
                (abi::INTEGER, 42)
            );
            metadata.set_limit(65536);
            host.set_limit(2 * 1024 * 1024);
            metadata.fail_after(usize::MAX);
            let recovered = compile_module(Failure::None).unwrap();
            assert!(recovered.invoke(&mut slots, 0, 64).instructions > 0);
            drop((peer, recovered));
            assert_eq!(
                (
                    metadata.current(),
                    host.current(),
                    total.load(Ordering::Relaxed)
                ),
                (0, 0, 0)
            );
        }
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

#[cfg(all(test, not(miri)))]
pub(super) mod projection_probe;
