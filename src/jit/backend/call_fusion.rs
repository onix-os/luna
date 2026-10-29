use super::*;
use crate::jit::call_fusion::site::kernel::{Entry as FusedEntry, Plan, View};

pub(in crate::jit) struct Code {
    _memory: Memory,
    entry: FusedEntry,
    registers: usize,
    pub(in crate::jit) bytes: usize,
}

impl Code {
    pub(in crate::jit) fn invoke_scratch(
        &self,
        slots: &mut [Slot],
        pc: u64,
        budget: u32,
        view: Option<&mut View>,
    ) -> Result<Exit, CompileError> {
        if slots.len() != self.registers {
            return Err(CompileError::Compilation("fused scratch width".into()));
        }
        let mut exit = Exit::default();
        let view = view.map_or(std::ptr::null_mut(), |view| view as *mut View);
        unsafe {
            (self.entry)(
                slots.as_mut_ptr(),
                pc,
                budget.min(64),
                &mut exit,
                std::ptr::null_mut(),
                view,
            );
        }
        Ok(exit)
    }
}

pub(in crate::jit) fn compile(
    plan: &Plan<'_>,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    limits: crate::jit::work::Limits,
) -> Result<Code, CompileError> {
    let expansion = plan.expansion(limits)?;
    let workspace = plan.allocator();
    let _templates = Reservation::new(
        workspace.0.clone(),
        64 * 1024 + expansion.instructions * 256,
    )
    .map_err(|_| CompileError::ResourceLimit("fused template workspace"))?;
    let _signatures = Reservation::new(
        workspace.0.clone(),
        24 * 3 * Layout::new::<AbiParam>().size(),
    )
    .map_err(|_| CompileError::ResourceLimit("fused signatures"))?;
    let status = MemoryStatus::try_new(metadata.clone())?;
    let fail = |error: cranelift_module::ModuleError| {
        status.error().unwrap_or_else(|| {
            CompileError::compilation(format_args!("{error}"), workspace.clone())
        })
    };
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return Err(CompileError::Unavailable(
            "cannot determine native memory page size",
        ));
    }
    let memory = Handoff::try_new(
        Memory {
            allocations: BudgetVec::new_in(metadata.clone()),
            total,
            status: status.clone(),
            failure: Failure::None,
            limit,
            page: page as usize,
        },
        metadata.clone(),
    )
    .map_err(|_| CompileError::ResourceLimit("fused memory handoff"))?;
    let (provider, _provider_charge) = global_box::try_new(Provider(memory.clone()), metadata)
        .map_err(|_| CompileError::ResourceLimit("fused memory provider"))?;
    let mut jit = native_builder(cranelift_native::builder(), workspace.clone())?;
    jit.memory_provider(provider);
    let mut module = JITModule::new(jit);
    let mut context = module.make_context();
    context.func = plan.program(module.target_config(), module.isa().default_call_conv());
    plan.verify(
        &context.func,
        module.target_config(),
        module.isa().default_call_conv(),
    )?;
    cranelift_codegen::verify_function(&context.func, module.isa())
        .map_err(|error| CompileError::compilation(format_args!("{error}"), workspace.clone()))?;
    expansion.verify_actual(
        context
            .func
            .layout
            .blocks()
            .map(|b| context.func.layout.block_insts(b).count())
            .sum(),
        context.func.layout.blocks().count(),
    )?;
    let id = module
        .declare_anonymous_function(&context.func.signature)
        .map_err(fail)?;
    if id.as_u32() != 0 {
        return Err(CompileError::Compilation("fused function identity".into()));
    }
    context
        .compile(module.isa(), &mut Default::default())
        .map_err(|error| fail(error.into()))?;
    let compiled = context.compiled_code().unwrap();
    if !compiled.buffer.relocs().is_empty() {
        return Err(CompileError::Compilation(
            "fused unexpected relocation".into(),
        ));
    }
    let bytes = compiled.code_buffer().len();
    module
        .define_function_bytes(
            id,
            u64::from(compiled.buffer.alignment),
            compiled.code_buffer(),
            &[],
        )
        .map_err(fail)?;
    module.finalize_definitions().map_err(fail)?;
    let pointer = module.get_finalized_function(id);
    let image = memory
        .take()
        .ok_or_else(|| CompileError::Compilation("missing fused mappings".into()))?;
    drop(module);
    Ok(Code {
        _memory: image,
        entry: unsafe { std::mem::transmute::<*const u8, FusedEntry>(pointer) },
        registers: plan.registers(),
        bytes,
    })
}
