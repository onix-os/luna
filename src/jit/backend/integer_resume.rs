use super::*;
use crate::jit::readonly_lookup::binding::integer_resume::{self, Entry, Plan};

pub(in crate::jit) struct Code {
    _memory: Memory,
    pub entry: Entry,
}

#[cfg(test)]
pub(super) fn from_artifact(memory: Memory, entry: *const u8) -> Code {
    Code {
        _memory: memory,
        entry: unsafe { std::mem::transmute::<*const u8, Entry>(entry) },
    }
}

pub(in crate::jit) fn compile(
    source: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    limits: crate::jit::work::Limits,
) -> Result<Code, CompileError> {
    compile_to(source, total, limit, metadata, limits, Destination::Install)?.installed()
}

pub(super) fn compile_to(
    source: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    limits: crate::jit::work::Limits,
    destination: Destination,
) -> Result<Compilation<Code>, CompileError> {
    let plan = Plan::recognize(source)
        .ok_or_else(|| CompileError::Compilation("canonical resume source".into()))?;
    let expansion = crate::jit::work::Expansion {
        instructions: 512,
        blocks: 32,
    };
    if limits.instructions < expansion.instructions || limits.blocks < expansion.blocks {
        return Err(CompileError::ResourceLimit("canonical resume expansion"));
    }
    let _templates = Reservation::new(source.operations.allocator().0.clone(), 192 * 1024)
        .map_err(|_| CompileError::ResourceLimit("canonical resume templates"))?;
    let _signatures = Reservation::new(
        source.operations.allocator().0.clone(),
        9 * Layout::new::<AbiParam>().size(),
    )
    .map_err(|_| CompileError::ResourceLimit("canonical resume signatures"))?;
    let status = MemoryStatus::try_new(metadata.clone())?;
    let fail = |error: cranelift_module::ModuleError| {
        status.error().unwrap_or_else(|| {
            CompileError::compilation(
                format_args!("{error}"),
                source.operations.allocator().clone(),
            )
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
            #[cfg(test)]
            failure: Failure::None,
            limit,
            page: page as usize,
        },
        metadata.clone(),
    )
    .map_err(|_| CompileError::ResourceLimit("canonical resume handoff"))?;
    let (provider, _charge) = global_box::try_new(Provider(memory.clone()), metadata)
        .map_err(|_| CompileError::ResourceLimit("canonical resume provider"))?;
    let mut jit = native_builder(
        cranelift_native::builder(),
        source.operations.allocator().clone(),
    )?;
    jit.memory_provider(provider);
    let mut module = JITModule::new(jit);
    #[cfg(any(test, feature = "jit-worker-bootstrap"))]
    let artifact_capture = artifact::begin(artifact::Kind::Resume);
    let mut context = module.make_context();
    context.func = integer_resume::function(
        plan,
        module.target_config(),
        module.isa().default_call_conv(),
    );
    integer_resume::verify(
        plan,
        &context.func,
        module.target_config(),
        module.isa().default_call_conv(),
    )?;
    cranelift_codegen::verify_function(&context.func, module.isa()).map_err(|error| {
        CompileError::compilation(
            format_args!("{error}"),
            source.operations.allocator().clone(),
        )
    })?;
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
    context
        .compile(module.isa(), &mut Default::default())
        .map_err(|error| fail(error.into()))?;
    let compiled = context.compiled_code().unwrap();
    if !compiled.buffer.relocs().is_empty() {
        return Err(CompileError::Compilation(
            "canonical resume relocations".into(),
        ));
    }
    #[cfg(any(test, feature = "jit-worker-bootstrap"))]
    artifact_capture.record(
        id,
        compiled.code_buffer(),
        u64::from(compiled.buffer.alignment),
        &[],
        &module,
    );
    #[cfg(any(test, feature = "jit-worker-bootstrap"))]
    if !destination.installs() {
        artifact_capture.commit();
        return Ok(Compilation::Emitted { relocations: 0 });
    }
    debug_assert!(destination.installs());
    module
        .define_function_bytes(
            id,
            u64::from(compiled.buffer.alignment),
            compiled.code_buffer(),
            &[],
        )
        .map_err(fail)?;
    module.finalize_definitions().map_err(fail)?;
    #[cfg(any(test, feature = "jit-worker-bootstrap"))]
    artifact_capture.commit();
    let pointer = module.get_finalized_function(id);
    let image = memory
        .take()
        .ok_or_else(|| CompileError::Compilation("canonical resume mappings".into()))?;
    drop(module);
    Ok(Compilation::Installed(Code {
        _memory: image,
        entry: unsafe { std::mem::transmute::<*const u8, Entry>(pointer) },
    }))
}
