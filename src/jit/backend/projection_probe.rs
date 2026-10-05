use super::*;

pub(crate) fn with_projection_probe(
    function: cranelift_codegen::ir::Function,
    call: impl FnOnce(*const u8),
) -> Result<(), JitError> {
    let ledger = super::super::resources::Ledger::new(2 * 1024 * 1024);
    let allocator = BudgetAllocator(ledger.clone());
    let total = MappingCounter::new(super::super::resources::Ledger::new(2 * 1024 * 1024));
    let status = MemoryStatus::try_new(allocator.clone())?;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return Err(JitError::Unavailable(
            "cannot determine native memory page size",
        ));
    }
    let memory = Handoff::try_new(
        Memory {
            allocations: BudgetVec::new_in(allocator.clone()),
            total: total.clone(),
            status,
            failure: Failure::None,
            limit: 128 * 1024,
            page: page as usize,
        },
        allocator.clone(),
    )
    .map_err(|_| JitError::ResourceLimit("projection probe metadata"))?;
    let (provider, charge) = global_box::try_new(Provider(memory.clone()), allocator)
        .map_err(|_| JitError::ResourceLimit("projection probe provider"))?;
    let mut builder = native_builder(cranelift_native::builder())?;
    builder.memory_provider(provider);
    let mut module = JITModule::new(builder);
    let fail = |error: cranelift_module::ModuleError| JitError::Compilation(error.to_string());
    let declaration = module
        .declare_anonymous_function(&function.signature)
        .map_err(fail)?;
    let mut context = module.make_context();
    context.func = function;
    context
        .compile(module.isa(), &mut Default::default())
        .map_err(|error| fail(error.into()))?;
    let compiled = context.compiled_code().unwrap();
    assert!(compiled.buffer.relocs().is_empty());
    module
        .define_function_bytes(
            declaration,
            u64::from(compiled.buffer.alignment),
            compiled.code_buffer(),
            &[],
        )
        .map_err(fail)?;
    module.finalize_definitions().map_err(fail)?;
    call(module.get_finalized_function(declaration));
    drop((context, module, memory, charge));
    assert_eq!(total.load(Ordering::Relaxed), 0);
    assert_eq!(ledger.current(), 0);
    Ok(())
}
