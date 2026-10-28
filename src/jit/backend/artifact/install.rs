use super::*;

#[derive(Debug)]
pub(in crate::jit) enum InstallError {
    Packet(Error),
    Native(JitError),
}

pub(super) struct Loaded {
    pub(super) _memory: Memory,
    pub(super) kind: Kind,
    pub(super) entries: BudgetVec<*const u8, BudgetAllocator>,
}

pub(in crate::jit) struct Installed {
    modules: BudgetVec<Loaded, BudgetAllocator>,
}

fn symbol(index: u8) -> (&'static str, *const u8) {
    match index {
        1..=10 => {
            let (_, name, entry) = helpers::SYMBOLS[usize::from(index - 1)];
            (name, entry as *const u8)
        }
        11 => ("luna_get_up_table_c3", helpers::call_hashless as *const u8),
        12 => (
            "luna_get_up_table_h1",
            helpers::PREHASHED_ENTRY as *const u8,
        ),
        _ => unreachable!(),
    }
}

pub(in crate::jit) fn install(
    packet: &[u8],
    binding: Binding<'_>,
    limits: Limits,
    total: MappingCounter,
    mapping_limit: usize,
    metadata: BudgetAllocator,
    failure: Failure,
) -> Result<Installed, InstallError> {
    let bundle = decode(packet, binding, limits, metadata.clone()).map_err(InstallError::Packet)?;
    let expected = environment().map_err(InstallError::Packet)?;
    if *expected != *bundle.environment {
        return Err(InstallError::Packet(Error::Identity));
    }
    let mut modules = BudgetVec::new_in(metadata.clone());
    modules
        .try_reserve_exact(bundle.modules.len())
        .map_err(|_| InstallError::Native(JitError::ResourceLimit("artifact module owners")))?;
    for image in &bundle.modules {
        modules.push(
            load(
                image,
                total.clone(),
                mapping_limit,
                metadata.clone(),
                failure,
            )
            .map_err(InstallError::Native)?,
        );
    }
    Ok(Installed { modules })
}

pub(super) fn load(
    image: &ModuleImage,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    failure: Failure,
) -> Result<Loaded, JitError> {
    load_in(image, total, limit, metadata.clone(), metadata, failure)
}

pub(super) fn load_in(
    image: &ModuleImage,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    workspace: BudgetAllocator,
    failure: Failure,
) -> Result<Loaded, JitError> {
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
            failure,
            limit,
            page: page as usize,
        },
        metadata.clone(),
    )
    .map_err(|_| JitError::ResourceLimit("artifact memory handoff"))?;
    let (provider, _provider_charge) =
        global_box::try_new(Provider(memory.clone()), metadata.clone())
            .map_err(|_| JitError::ResourceLimit("artifact provider"))?;
    let mut entries = BudgetVec::new_in(workspace.clone());
    entries
        .try_reserve_exact(image.functions.len())
        .map_err(|_| JitError::ResourceLimit("artifact entries"))?;
    let symbol_bytes = symbol_storage_bytes(
        image
            .declarations
            .iter()
            .filter(|decl| decl.0[0] != 0)
            .map(|decl| symbol(decl.0[0]).0.len()),
    )?;
    let signature_bytes = image
        .declarations
        .iter()
        .try_fold(0usize, |sum, declaration| {
            let count = declaration.params().len() + usize::from(declaration.0[2] != 0);
            sum.checked_add(
                Layout::array::<AbiParam>(count)
                    .ok()?
                    .size()
                    .checked_mul(2)?,
            )
        })
        .ok_or(JitError::ResourceLimit("artifact signatures"))?;
    let _symbols = Reservation::new(workspace.0.clone(), symbol_bytes)
        .map_err(|_| JitError::ResourceLimit("artifact symbols"))?;
    let _signatures = Reservation::new(workspace.0.clone(), signature_bytes)
        .map_err(|_| JitError::ResourceLimit("artifact signatures"))?;
    let relocation_bytes = image
        .functions
        .iter()
        .try_fold(0usize, |sum, function| {
            sum.checked_add(
                Layout::array::<ModuleReloc>(function.relocations.len())
                    .ok()?
                    .size(),
            )
        })
        .ok_or(JitError::ResourceLimit("artifact relocation copy"))?;
    let _relocations = Reservation::new(workspace.0.clone(), relocation_bytes)
        .map_err(|_| JitError::ResourceLimit("artifact relocation copy"))?;
    let mut builder = native_builder(cranelift_native::builder())?;
    for declaration in &image.declarations {
        if declaration.0[0] != 0 {
            let (name, address) = symbol(declaration.0[0]);
            builder.symbol(owned_symbol(name)?, address);
        }
    }
    builder.memory_provider(provider);
    let mut module = JITModule::new(builder);
    for (index, declaration) in image.declarations.iter().enumerate() {
        let signature = declaration.signature(&module);
        let id = if declaration.0[0] == 0 {
            module.declare_anonymous_function(&signature)
        } else {
            module.declare_function(symbol(declaration.0[0]).0, Linkage::Import, &signature)
        }
        .map_err(fail)?;
        if id.as_u32() as usize != index {
            return Err(JitError::Compilation("artifact function identity".into()));
        }
    }
    for function in &image.functions {
        let mut relocations = BudgetVec::new_in(workspace.clone());
        relocations
            .try_reserve_exact(function.relocations.len())
            .map_err(|_| JitError::ResourceLimit("artifact relocation staging"))?;
        relocations.extend(
            function
                .relocations
                .iter()
                .map(|relocation| relocation.module()),
        );
        module
            .define_function_bytes(
                FuncId::from_u32(function.id),
                u64::from(function.alignment),
                &function.bytes,
                &relocations,
            )
            .map_err(fail)?;
    }
    module.finalize_definitions().map_err(fail)?;
    for function in &image.functions {
        entries.push(module.get_finalized_function(FuncId::from_u32(function.id)));
    }
    let image_memory = memory
        .take()
        .ok_or_else(|| JitError::Compilation("artifact missing mappings".into()))?;
    drop(module);
    Ok(Loaded {
        _memory: image_memory,
        kind: image.kind,
        entries,
    })
}

impl Installed {
    pub(in crate::jit) fn len(&self) -> usize {
        self.modules.len()
    }
    pub(in crate::jit) fn kind(&self, module: usize) -> Kind {
        self.modules[module].kind
    }
    pub(in crate::jit) fn entry(&self, module: usize, function: usize) -> *const u8 {
        self.modules[module].entries[function]
    }
    pub(in crate::jit) fn functions(&self, module: usize) -> usize {
        self.modules[module].entries.len()
    }
    pub(super) fn mapped_bytes(&self, module: usize) -> usize {
        self.modules[module]
            ._memory
            .allocations
            .iter()
            .map(|segment| segment.bytes)
            .sum()
    }
}
