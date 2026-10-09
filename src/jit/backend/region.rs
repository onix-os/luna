use cranelift_codegen::ir::{ExternalName, Function, Signature, UserExternalName, UserFuncName};

use super::*;

#[cfg(test)]
mod tests;

pub(crate) use super::super::scoped_helpers::RegionView as View;

pub(crate) type Boundary = unsafe extern "C" fn(*mut View) -> u32;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    None,
    Bound,
    Caller,
    Slots,
    Status,
}

pub(crate) struct Driver {
    _memory: Memory,
    entry: unsafe extern "C" fn(*mut View, u32),
}

impl Driver {
    /// Executes at most `limit` caller fragments through the linked boundary.
    ///
    /// # Safety
    /// The view, caller entry, slot prefix and helper host must remain valid;
    /// the boundary must not unwind and must republish any reborrowed pointers.
    pub(crate) unsafe fn invoke(&self, view: *mut View, limit: u32) {
        unsafe { (self.entry)(view, limit) };
    }
}

fn template(module: &JITModule, ids: [cranelift_module::FuncId; 2], fault: Fault) -> Function {
    let pointer = module.target_config().pointer_type();
    let signature = |params: &[cranelift_codegen::ir::Type],
                     returns: Option<cranelift_codegen::ir::Type>| {
        let mut signature = Signature::new(module.isa().default_call_conv());
        signature
            .params
            .extend(params.iter().copied().map(AbiParam::new));
        signature.returns.extend(returns.map(AbiParam::new));
        signature
    };
    let mut function = Function::with_name_signature(
        UserFuncName::user(0, 0),
        signature(&[pointer, types::I32], None),
    );
    let mut caller = signature(&[pointer, types::I64, types::I32, pointer], None);
    caller.returns.extend([types::I64; 2].map(AbiParam::new));
    let imports = [caller, signature(&[pointer], Some(types::I32))];
    let references: [_; 2] = std::array::from_fn(|index| {
        let signature = function.import_signature(imports[index].clone());
        let name = function.declare_imported_user_function(UserExternalName {
            namespace: 0,
            index: ids[if fault == Fault::Caller && index == 0 {
                1
            } else {
                index
            }]
            .as_u32(),
        });
        function.import_function(cranelift_codegen::ir::ExtFuncData {
            name: ExternalName::User(name),
            signature,
            colocated: false,
            patchable: false,
        })
    });
    let mut context = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut function, &mut context);
    let entry = builder.create_block();
    let head = builder.create_block();
    let body = builder.create_block();
    let done = builder.create_block();
    builder.append_block_params_for_function_params(entry);
    builder.append_block_param(head, types::I32);
    builder.switch_to_block(entry);
    let view = builder.block_params(entry)[0];
    let limit = builder.block_params(entry)[1];
    builder.ins().jump(head, &[limit.into()]);
    builder.switch_to_block(head);
    let remaining = builder.block_params(head)[0];
    let active = if fault == Fault::Bound {
        builder.ins().iconst(types::I8, 1)
    } else {
        builder.ins().icmp_imm_s(IntCC::NotEqual, remaining, 0)
    };
    builder.ins().brif(active, body, &[], done, &[]);
    builder.switch_to_block(body);
    let slots = builder.ins().load(
        pointer,
        MemFlagsData::new(),
        view,
        if fault == Fault::Slots {
            std::mem::offset_of!(View, pc)
        } else {
            std::mem::offset_of!(View, slots)
        } as i32,
    );
    let pc = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        view,
        std::mem::offset_of!(View, pc) as i32,
    );
    let budget = builder.ins().load(
        types::I32,
        MemFlagsData::new(),
        view,
        std::mem::offset_of!(View, budget) as i32,
    );
    let host = builder.ins().load(
        pointer,
        MemFlagsData::new(),
        view,
        std::mem::offset_of!(View, host) as i32,
    );
    let exit = builder
        .ins()
        .iadd_imm_s(view, std::mem::offset_of!(View, exit) as i64);
    let call = builder
        .ins()
        .call(references[0], &[slots, pc, budget, host]);
    exit_transport::store_return(&mut builder, call, exit);
    let boundary = builder.ins().call(references[1], &[view]);
    let result = builder.inst_results(boundary)[0];
    let more = builder.ins().icmp_imm_s(
        IntCC::Equal,
        result,
        if fault == Fault::Status { 2 } else { 1 },
    );
    let next = builder.ins().iadd_imm_s(remaining, -1);
    builder.ins().brif(more, head, &[next.into()], done, &[]);
    builder.switch_to_block(done);
    builder.ins().return_(&[]);
    builder.seal_all_blocks();
    builder.finalize(module.target_config());
    function
}

pub(crate) fn compile(
    caller: Entry,
    boundary: Boundary,
    memory: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    workspace: BudgetAllocator,
    limits: super::super::work::Limits,
    fault: Fault,
) -> Result<Driver, JitError> {
    if limits.instructions < 64 || limits.blocks < 4 {
        return Err(JitError::ResourceLimit("native region IR"));
    }
    let _templates = Reservation::new(workspace.0.clone(), 32 * 1024)
        .map_err(|_| JitError::ResourceLimit("native region workspace"))?;
    let _signatures = Reservation::new(
        workspace.0.clone(),
        Layout::array::<AbiParam>(96).unwrap().size(),
    )
    .map_err(|_| JitError::ResourceLimit("native region signatures"))?;
    let _symbols = Reservation::new(
        workspace.0.clone(),
        symbol_storage_bytes(["region_caller".len(), "region_boundary".len()])?,
    )
    .map_err(|_| JitError::ResourceLimit("native region symbols"))?;
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
    let image = Handoff::try_new(
        Memory {
            allocations: BudgetVec::new_in(metadata.clone()),
            total: memory,
            status: status.clone(),
            #[cfg(test)]
            failure: Failure::None,
            limit,
            page: page as usize,
        },
        metadata.clone(),
    )
    .map_err(|_| JitError::ResourceLimit("JIT metadata"))?;
    let (provider, _charge) = global_box::try_new(Provider(image.clone()), metadata)
        .map_err(|_| JitError::ResourceLimit("JIT metadata"))?;
    let mut builder = native_builder(cranelift_native::builder())?;
    builder.symbol(owned_symbol("region_caller")?, caller as *const u8);
    builder.symbol(owned_symbol("region_boundary")?, boundary as *const u8);
    builder.memory_provider(provider);
    let mut module = JITModule::new(builder);
    let pointer = module.target_config().pointer_type();
    let mut signature = module.make_signature();
    signature
        .params
        .extend([pointer, types::I32].map(AbiParam::new));
    let entry = module
        .declare_anonymous_function(&signature)
        .map_err(fail)?;
    let mut caller_signature = module.make_signature();
    caller_signature
        .params
        .extend([pointer, types::I64, types::I32, pointer].map(AbiParam::new));
    caller_signature
        .returns
        .extend([types::I64; 2].map(AbiParam::new));
    let caller = module
        .declare_function("region_caller", Linkage::Import, &caller_signature)
        .map_err(fail)?;
    let mut boundary_signature = module.make_signature();
    boundary_signature.params.push(AbiParam::new(pointer));
    boundary_signature.returns.push(AbiParam::new(types::I32));
    let boundary = module
        .declare_function("region_boundary", Linkage::Import, &boundary_signature)
        .map_err(fail)?;
    let mut context = module.make_context();
    context.func = template(&module, [caller, boundary], fault);
    if context.func != template(&module, [caller, boundary], Fault::None) {
        return Err(JitError::Compilation(
            "native region source mismatch".into(),
        ));
    }
    let blocks = context.func.layout.blocks().count();
    let instructions: usize = context
        .func
        .layout
        .blocks()
        .map(|block| context.func.layout.block_insts(block).count())
        .sum();
    if blocks > limits.blocks || instructions > limits.instructions {
        return Err(JitError::ResourceLimit("native region IR"));
    }
    cranelift_codegen::verify_function(&context.func, module.isa())
        .map_err(|e| JitError::Compilation(e.to_string()))?;
    context
        .compile(module.isa(), &mut Default::default())
        .map_err(|e| fail(e.into()))?;
    let compiled = context.compiled_code().unwrap();
    if compiled.buffer.relocs().len() > limits.relocations {
        return Err(JitError::ResourceLimit("native region relocations"));
    }
    let _copies = Reservation::new(
        workspace.0.clone(),
        Layout::array::<ModuleReloc>(compiled.buffer.relocs().len())
            .map_err(|_| JitError::ResourceLimit("native region relocation size"))?
            .size(),
    )
    .map_err(|_| JitError::ResourceLimit("native region relocation copy"))?;
    let mut relocations = BudgetVec::new_in(workspace);
    relocations
        .try_reserve_exact(compiled.buffer.relocs().len())
        .map_err(|_| JitError::ResourceLimit("native region relocation staging"))?;
    relocations.extend(
        compiled
            .buffer
            .relocs()
            .iter()
            .map(|r| ModuleReloc::from_mach_reloc(r, &context.func, entry)),
    );
    module
        .define_function_bytes(
            entry,
            u64::from(compiled.buffer.alignment),
            compiled.code_buffer(),
            &relocations,
        )
        .map_err(fail)?;
    module.finalize_definitions().map_err(fail)?;
    let pointer = module.get_finalized_function(entry);
    let memory = image
        .take()
        .ok_or_else(|| JitError::Compilation("missing native region mappings".into()))?;
    drop(module);
    Ok(Driver {
        _memory: memory,
        entry: unsafe {
            std::mem::transmute::<*const u8, unsafe extern "C" fn(*mut View, u32)>(pointer)
        },
    })
}
