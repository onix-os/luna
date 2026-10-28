use super::*;

#[cfg(not(miri))]
mod tests;

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
    let protection = link::protection(&bundle.environment, link::Architecture::native())
        .map_err(InstallError::Packet)?;
    let mut modules = BudgetVec::new_in(metadata.clone());
    modules
        .try_reserve_exact(bundle.modules.len())
        .map_err(|_| InstallError::Native(JitError::ResourceLimit("artifact module owners")))?;
    for image in &bundle.modules {
        modules.push(
            load_in(
                image,
                total.clone(),
                mapping_limit,
                metadata.clone(),
                metadata.clone(),
                failure,
                protection,
            )
            .map_err(InstallError::Native)?,
        );
    }
    Ok(Installed { modules })
}

pub(super) fn load_in(
    image: &ModuleImage,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    workspace: BudgetAllocator,
    failure: Failure,
    protection: BranchProtection,
) -> Result<Loaded, JitError> {
    let architecture = link::Architecture::native();
    let mut layouts = BudgetVec::new_in(workspace.clone());
    layouts
        .try_reserve_exact(image.functions.len())
        .map_err(|_| JitError::ResourceLimit("artifact layouts"))?;
    for function in &image.functions {
        layouts.push(
            link::Layout::new(function.bytes.len(), &function.relocations, architecture)
                .map_err(|_| JitError::Unavailable("artifact relocation layout"))?,
        );
    }
    let mut entries = BudgetVec::new_in(workspace);
    entries
        .try_reserve_exact(image.functions.len())
        .map_err(|_| JitError::ResourceLimit("artifact entries"))?;
    let status = MemoryStatus::try_new(metadata.clone())?;
    let fail = || {
        status
            .error()
            .unwrap_or(JitError::Unavailable("artifact native memory"))
    };
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return Err(JitError::Unavailable(
            "cannot determine native memory page size",
        ));
    }
    let mut memory = Memory {
        allocations: BudgetVec::new_in(metadata),
        total,
        status: status.clone(),
        failure,
        limit,
        page: page as usize,
    };
    for (function, layout) in image.functions.iter().zip(&layouts) {
        let pointer = memory
            .allocate(
                layout.bytes,
                u64::from(function.alignment),
                JITMemoryKind::Executable,
            )
            .map_err(|_| fail())?;
        // The slice covers the exact writable allocation owned by memory.
        let bytes = unsafe { std::slice::from_raw_parts_mut(pointer, layout.bytes) };
        bytes[..function.bytes.len()].copy_from_slice(&function.bytes);
        entries.push(pointer.cast_const());
    }
    for ((function, layout), &entry) in image.functions.iter().zip(&layouts).zip(&entries) {
        // All mappings remain writable and unexposed until finalization.
        let bytes = unsafe { std::slice::from_raw_parts_mut(entry.cast_mut(), layout.bytes) };
        let mut patcher = link::Patcher::new(
            bytes,
            *layout,
            architecture,
            entry.expose_provenance() as u64,
        )
        .map_err(|_| JitError::Unavailable("artifact relocation address"))?;
        for relocation in &function.relocations {
            let target = target(image, &entries, relocation)
                .ok_or(JitError::Unavailable("artifact relocation target"))?;
            patcher
                .apply(relocation, target)
                .map_err(|_| JitError::Unavailable("artifact relocation cannot be installed"))?;
        }
    }
    memory.finalize(protection).map_err(|_| fail())?;
    Ok(Loaded {
        _memory: memory,
        kind: image.kind,
        entries,
    })
}

fn target(image: &ModuleImage, entries: &[*const u8], relocation: &Relocation) -> Option<u64> {
    let declaration = image.declarations.get(relocation.index as usize)?;
    let base = if declaration.0[0] == 0 {
        let index = image
            .functions
            .iter()
            .position(|function| function.id == relocation.index)?;
        entries.get(index)?.expose_provenance() as u64
    } else {
        if !(1..=12).contains(&declaration.0[0]) || relocation.target_kind != 0 {
            return None;
        }
        symbol(declaration.0[0]).1.expose_provenance() as u64
    };
    match relocation.target_kind {
        0 if relocation.target_offset == 0 => Some(base),
        1 if image.functions.iter().any(|function| {
            function.id == relocation.index
                && (relocation.target_offset as usize) < function.bytes.len()
        }) =>
        {
            base.checked_add(u64::from(relocation.target_offset))
        }
        _ => None,
    }
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
