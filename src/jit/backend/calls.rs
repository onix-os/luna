use std::ffi::c_void;

use cranelift_codegen::ir::{ExternalName, Function, UserExternalName};

use super::*;
use crate::jit::calls::{NativeFrame, Plan, Program};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy)]
pub(crate) struct Hooks {
    pub enter: unsafe extern "C" fn(*mut c_void, u64, u32, u32) -> *mut NativeFrame,
    pub leave: unsafe extern "C" fn(*mut c_void, *mut NativeFrame, u64, u32) -> u32,
}

pub(crate) struct CallCode {
    _memory: Memory,
    entry: unsafe extern "C" fn(*mut c_void, u32) -> u32,
    leaf: crate::jit::leaf::CellEntry,
    compact: super::compact::Entry,
    operands: (u64, u8, u8, u8),
    #[cfg(test)]
    pub relocations: usize,
    #[cfg(test)]
    pub bytes: usize,
}

impl CallCode {
    pub(in crate::jit) fn compact(&self) -> &super::compact::Entry {
        &self.compact
    }

    pub(crate) fn operands(&self) -> (u64, u8, u8, u8) {
        self.operands
    }

    /// Invokes the verified callee without the aggregate callbacks.
    ///
    /// # Safety
    /// The frame, slots, view and capture must remain valid and exclusive.
    pub unsafe fn invoke_leaf(&self, frame: *mut NativeFrame, budget: u32) {
        unsafe {
            (*frame).exit = (self.leaf)((*frame).slots, 0, budget, (*frame).view).into_exit();
        }
    }

    /// Invokes the linked aggregate entry.
    ///
    /// # Safety
    /// Hook data, frames, slots, views and cells must remain valid and exclusive
    /// for the call. The hooks must not unwind.
    pub unsafe fn invoke(&self, data: *mut c_void, budget: u32) -> u32 {
        unsafe { (self.entry)(data, budget) }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Default)]
pub(crate) enum LinkFault {
    #[default]
    None,
    CalleeTarget,
    EnterTarget,
    LeaveTarget,
    Signature,
}

fn imports(function: &Function) -> Result<[cranelift_codegen::ir::FuncRef; 3], JitError> {
    let mut keys = function.dfg.ext_funcs.keys();
    let references = [keys.next(), keys.next(), keys.next()];
    if references.iter().any(Option::is_none) || keys.next().is_some() {
        return Err(JitError::Compilation("aggregate import cardinality".into()));
    }
    Ok(references.map(Option::unwrap))
}

fn bind(function: &mut Function, ids: [cranelift_module::FuncId; 3]) -> Result<(), JitError> {
    for reference in imports(function)? {
        let declaration = &function.dfg.ext_funcs[reference];
        let index = if declaration.name == ExternalName::testcase("activation_enter") {
            0
        } else if declaration.name == ExternalName::testcase("activation_leaf") {
            1
        } else if declaration.name == ExternalName::testcase("activation_leave") {
            2
        } else {
            return Err(JitError::Compilation("unknown aggregate import".into()));
        };
        let name = function.declare_imported_user_function(UserExternalName {
            namespace: 0,
            index: ids[index].as_u32(),
        });
        function.dfg.ext_funcs[reference].name = ExternalName::User(name);
    }
    Ok(())
}

fn verify_bound(
    plan: &Plan<'_>,
    program: &Program,
    module: &JITModule,
    ids: [cranelift_module::FuncId; 3],
) -> Result<(), JitError> {
    let mut expected = plan.program(module.target_config(), module.isa().default_call_conv())?;
    bind(&mut expected.entry, ids)?;
    if program.entry != expected.entry || program.callee != expected.callee {
        return Err(JitError::Compilation(
            "aggregate linked source mismatch".into(),
        ));
    }
    Ok(())
}

pub(crate) fn compile(
    plan: &Plan<'_>,
    hooks: Hooks,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    limits: super::super::work::Limits,
    #[cfg(test)] failure: Failure,
    #[cfg(test)] fault: LinkFault,
) -> Result<CallCode, JitError> {
    if plan.expansion.instructions > limits.instructions || plan.expansion.blocks > limits.blocks {
        return Err(JitError::ResourceLimit("aggregate IR"));
    }
    let workspace = plan.allocator();
    #[cfg(test)]
    if failure == Failure::RefuseSignatures {
        workspace.0.set_limit(workspace.0.current());
    }
    let signature_slots = [2usize, 7, 4, 4, 4]
        .into_iter()
        .map(|parameters| parameters.max(4) + 4)
        .sum::<usize>();
    let signature_bytes = Layout::array::<AbiParam>((signature_slots + 7) * 3)
        .map_err(|_| JitError::ResourceLimit("aggregate signature size"))?
        .size();
    let _signatures = Reservation::new(workspace.0.clone(), signature_bytes)
        .map_err(|_| JitError::ResourceLimit("aggregate signatures"))?;
    let symbol_bytes = symbol_storage_bytes(["activation_enter".len(), "activation_leave".len()])?;
    #[cfg(test)]
    if failure == Failure::RefuseSymbols {
        workspace.0.set_limit(workspace.0.current());
    }
    let _symbols = Reservation::new(workspace.0.clone(), symbol_bytes)
        .map_err(|_| JitError::ResourceLimit("aggregate symbols"))?;
    let _templates = Reservation::new(workspace.0.clone(), 32 * 1024)
        .map_err(|_| JitError::ResourceLimit("aggregate template workspace"))?;
    let status = MemoryStatus::try_new(metadata.clone())?;
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
    let (provider, _provider_charge) = global_box::try_new(Provider(memory.clone()), metadata)
        .map_err(|_| JitError::ResourceLimit("JIT metadata"))?;
    let mut jit = native_builder(cranelift_native::builder())?;
    jit.symbol(owned_symbol("activation_enter")?, hooks.enter as *const u8);
    jit.symbol(owned_symbol("activation_leave")?, hooks.leave as *const u8);
    jit.memory_provider(provider);
    let mut module = JITModule::new(jit);
    let mut program = plan.program(module.target_config(), module.isa().default_call_conv())?;
    let compact_binding = super::compact::Binding::new(plan.callee())?;
    let compact_program = compact_binding.program(module.isa());
    compact_binding.verify(&compact_program, module.isa())?;
    let functions = [&program.entry, &program.callee, &compact_program.function];
    plan.expansion.verify_actual(
        functions
            .iter()
            .flat_map(|f| f.layout.blocks().map(|b| f.layout.block_insts(b).count()))
            .sum(),
        functions.iter().map(|f| f.layout.blocks().count()).sum(),
    )?;
    plan.verify_program(
        &program,
        module.target_config(),
        module.isa().default_call_conv(),
    )?;
    let entry = module
        .declare_anonymous_function(&program.entry.signature)
        .map_err(fail)?;
    let leaf = module
        .declare_anonymous_function(&program.callee.signature)
        .map_err(fail)?;
    let compact = module
        .declare_anonymous_function(&compact_program.function.signature)
        .map_err(fail)?;
    if entry.as_u32() != 0 || leaf.as_u32() != 1 || compact.as_u32() != 2 {
        return Err(JitError::Compilation(
            "aggregate function identities".into(),
        ));
    }
    let imports = imports(&program.entry)?;
    let enter_signature =
        &program.entry.dfg.signatures[program.entry.dfg.ext_funcs[imports[0]].signature];
    let enter = module
        .declare_function("activation_enter", Linkage::Import, enter_signature)
        .map_err(fail)?;
    let leave_signature =
        &program.entry.dfg.signatures[program.entry.dfg.ext_funcs[imports[2]].signature];
    let leave = module
        .declare_function("activation_leave", Linkage::Import, leave_signature)
        .map_err(fail)?;
    let ids = [enter, leaf, leave];
    bind(&mut program.entry, ids)?;
    #[cfg(test)]
    match fault {
        LinkFault::None => {}
        LinkFault::CalleeTarget | LinkFault::EnterTarget | LinkFault::LeaveTarget => {
            let index = match fault {
                LinkFault::CalleeTarget => 1,
                LinkFault::EnterTarget => 0,
                _ => 2,
            };
            let wrong = program
                .entry
                .declare_imported_user_function(UserExternalName {
                    namespace: 0,
                    index: entry.as_u32(),
                });
            program.entry.dfg.ext_funcs[imports[index]].name = ExternalName::User(wrong);
        }
        LinkFault::Signature => program.entry.signature.params[1].value_type = types::I64,
    }
    verify_bound(plan, &program, &module, ids)?;
    cranelift_codegen::verify_function(&program.entry, module.isa())
        .map_err(|e| JitError::Compilation(e.to_string()))?;
    cranelift_codegen::verify_function(&program.callee, module.isa())
        .map_err(|e| JitError::Compilation(e.to_string()))?;
    let mut entry_context = module.make_context();
    entry_context.func = program.entry;
    let mut leaf_context = module.make_context();
    leaf_context.func = program.callee;
    let mut compact_context = module.make_context();
    compact_context.func = compact_program.function;
    entry_context
        .compile(module.isa(), &mut Default::default())
        .map_err(|error| fail(error.into()))?;
    leaf_context
        .compile(module.isa(), &mut Default::default())
        .map_err(|error| fail(error.into()))?;
    compact_context
        .compile(module.isa(), &mut Default::default())
        .map_err(|error| fail(error.into()))?;
    if !compact_context
        .compiled_code()
        .unwrap()
        .buffer
        .relocs()
        .is_empty()
    {
        return Err(JitError::Compilation(
            "compact unexpected relocation".into(),
        ));
    }
    let contexts = [
        (entry, &entry_context),
        (leaf, &leaf_context),
        (compact, &compact_context),
    ];
    let relocations = contexts
        .iter()
        .map(|(_, ctx)| ctx.compiled_code().unwrap().buffer.relocs().len())
        .sum::<usize>();
    if relocations > limits.relocations {
        return Err(JitError::ResourceLimit("aggregate relocations"));
    }
    let copy_bytes = Layout::array::<ModuleReloc>(relocations)
        .map_err(|_| JitError::ResourceLimit("aggregate relocation copy size"))?
        .size();
    #[cfg(test)]
    if failure == Failure::RefuseRelocationCopy {
        workspace.0.set_limit(workspace.0.current());
    }
    let _copies = Reservation::new(workspace.0.clone(), copy_bytes)
        .map_err(|_| JitError::ResourceLimit("aggregate relocation copy"))?;
    #[cfg(test)]
    let mut bytes = 0;
    for (id, context) in contexts {
        let compiled = context.compiled_code().unwrap();
        let mut staging = BudgetVec::new_in(workspace.clone());
        staging
            .try_reserve_exact(compiled.buffer.relocs().len())
            .map_err(|_| JitError::ResourceLimit("aggregate relocation staging"))?;
        staging.extend(
            compiled
                .buffer
                .relocs()
                .iter()
                .map(|r| ModuleReloc::from_mach_reloc(r, &context.func, id)),
        );
        module
            .define_function_bytes(
                id,
                u64::from(compiled.buffer.alignment),
                compiled.code_buffer(),
                &staging,
            )
            .map_err(fail)?;
        #[cfg(test)]
        {
            bytes += compiled.code_buffer().len();
        }
    }
    module.finalize_definitions().map_err(fail)?;
    let pointer = module.get_finalized_function(entry);
    let leaf_pointer = module.get_finalized_function(leaf);
    let compact_pointer = module.get_finalized_function(compact);
    let image = memory
        .take()
        .ok_or_else(|| JitError::Compilation("missing aggregate mappings".into()))?;
    drop(module);
    let entry = unsafe {
        std::mem::transmute::<*const u8, unsafe extern "C" fn(*mut c_void, u32) -> u32>(pointer)
    };
    Ok(CallCode {
        _memory: image,
        entry,
        leaf: unsafe {
            std::mem::transmute::<*const u8, crate::jit::leaf::CellEntry>(leaf_pointer)
        },
        operands: plan.operands(),
        compact: unsafe { super::compact::Entry::new(compact_binding, compact_pointer) },
        #[cfg(test)]
        relocations,
        #[cfg(test)]
        bytes,
    })
}
