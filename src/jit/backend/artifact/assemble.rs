use super::*;
use crate::jit::{array_window, compiler_wire, flow, readonly_lookup as read, work};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit) enum Route {
    Ordinary,
    Arrays,
    Readonly,
}

pub(in crate::jit) struct Assembly<'a> {
    pub source: &'a Snapshot,
    pub route: Route,
    pub resume: bool,
    pub total: MappingCounter,
    pub limit: usize,
    pub metadata: BudgetAllocator,
    pub work: work::Limits,
    pub failure: Failure,
}

fn invalid() -> InstallError {
    InstallError::Packet(Error::Module)
}

fn relocations(image: &ModuleImage) -> Result<usize, JitError> {
    image
        .functions
        .iter()
        .try_fold(0usize, |sum, function| {
            sum.checked_add(function.relocations.len())
        })
        .ok_or(JitError::ResourceLimit("native relocations"))
}

pub(in crate::jit) fn assemble(
    packet: &[u8],
    binding: Binding<'_>,
    limits: Limits,
    assembly: Assembly<'_>,
) -> Result<Code, InstallError> {
    let Assembly {
        source,
        route,
        resume,
        total,
        limit,
        metadata,
        work,
        failure,
    } = assembly;
    source.verify().map_err(InstallError::Native)?;
    let workspace = source.operations.allocator().clone();
    let encoded = compiler_wire::encode(
        source,
        compiler_wire::Limits {
            bytes: binding.source.len(),
            operations: source.operations.len(),
            constants: source.constants.len(),
        },
        workspace.clone(),
    )
    .map_err(|error| match error {
        compiler_wire::Error::Allocation => {
            InstallError::Native(JitError::ResourceLimit("artifact source binding"))
        }
        _ => InstallError::Packet(Error::Identity),
    })?;
    if *encoded != *binding.source {
        return Err(InstallError::Packet(Error::Identity));
    }
    drop(encoded);
    let bundle =
        decode(packet, binding, limits, workspace.clone()).map_err(InstallError::Packet)?;
    if *environment_in(workspace.clone()).map_err(InstallError::Packet)? != *bundle.environment {
        return Err(InstallError::Packet(Error::Identity));
    }
    let protection = link::protection(&bundle.environment, link::Architecture::native())
        .map_err(InstallError::Packet)?;
    let ordinary = bundle.modules.first().ok_or_else(invalid)?;
    if ordinary.kind != Kind::Ordinary || ordinary.functions.len() != 1 {
        return Err(invalid());
    }
    let read_plan = read::Plan::recognize(source);
    let resume_plan = read::binding::integer_resume::Plan::recognize(source);
    let mut arrays = BudgetVec::new_in(workspace.clone());
    if bundle
        .modules
        .iter()
        .any(|image| image.kind == Kind::Arrays)
    {
        for end in 0..source.operations.len() {
            if let Some(plan) = array_window::Plan::new(source, end) {
                arrays
                    .try_reserve(1)
                    .map_err(|_| InstallError::Native(JitError::ResourceLimit("array plans")))?;
                arrays.push(plan);
            }
        }
    }
    for image in bundle.modules.iter().skip(1) {
        match image.kind {
            Kind::Readonly
                if route == Route::Readonly
                    && read_plan.is_some()
                    && image.functions.len() == 1 => {}
            Kind::Resume
                if route == Route::Readonly
                    && resume
                    && resume_plan.is_some()
                    && image.functions.len() == 1 => {}
            Kind::Arrays
                if route == Route::Arrays
                    && !arrays.is_empty()
                    && image.functions.len() == arrays.len() => {}
            _ => return Err(invalid()),
        }
    }
    let bound = work::Expansion::admit(source, work).map_err(InstallError::Native)?;
    let ordinary_relocations = relocations(ordinary).map_err(InstallError::Native)?;
    if ordinary_relocations > work.relocations {
        return Err(InstallError::Native(JitError::ResourceLimit(
            "native relocations",
        )));
    }
    let graph = flow::FlowGraph::new(source).map_err(InstallError::Native)?;
    let entries = crate::jit::entry_bytes::EntryBytes::try_new(
        graph.nodes.iter().map(|node| node.lowering.native()),
        metadata.clone(),
    )
    .map_err(InstallError::Native)?;
    let loaded = install::load_in(
        ordinary,
        total.clone(),
        limit,
        metadata.clone(),
        workspace.clone(),
        failure,
        protection,
    )
    .map_err(InstallError::Native)?;
    let mut code = Image {
        _memory: loaded._memory,
        entry: loaded.entries[0],
        entry_arity: 5,
        byte_len: ordinary.functions[0].bytes.len(),
        preferred_entry: false,
        relocations: ordinary_relocations,
        registers: source.registers,
        entries,
        projected_upvalues: false,
        continuations: None,
        bound_entry: None,
        scalar_leaf: None,
        cell_kernel: false,
        entry_kind: payload::EntryKind::Ordinary,
        strided_prepared: false,
        binding_requirements: None,
        packed_integer_loop: false,
        integer_activation: false,
        scalar_kernel: None,
        array_kernels: None,
    }
    .ordinary()
    .map_err(InstallError::Native)?;
    drop(loaded.entries);
    if let Some(image) = bundle
        .modules
        .iter()
        .find(|image| image.kind == Kind::Arrays)
    {
        let array_work = work::Limits {
            instructions: work.instructions / 2,
            blocks: work.blocks / 2,
            relocations: work.relocations,
        };
        if work::Expansion::admit(source, array_work).is_ok() {
            if let Ok(loaded) = install::load_in(
                image,
                total.clone(),
                limit,
                metadata.clone(),
                workspace.clone(),
                failure,
                protection,
            ) {
                code.array_kernels = array::from_artifact(
                    loaded._memory,
                    &arrays,
                    &loaded.entries,
                    source.registers,
                    metadata.clone(),
                )
                .ok();
            }
        }
    }
    let Some(image) = bundle
        .modules
        .iter()
        .find(|image| image.kind == Kind::Readonly)
    else {
        return Ok(code);
    };
    let Ok(_owner) = Reservation::new(
        metadata.0.clone(),
        crate::jit::owner::Shared::<Code>::allocation_bytes(),
    ) else {
        return Ok(code);
    };
    let remaining = work::Limits {
        instructions: work.instructions.saturating_sub(bound.instructions),
        blocks: work.blocks.saturating_sub(bound.blocks),
        relocations: work.relocations.saturating_sub(ordinary_relocations),
    };
    let Ok(read_bound) = work::Expansion::admit(source, remaining)
        .and_then(|bound| readonly_lookup::expansion(bound, remaining))
    else {
        return Ok(code);
    };
    let child_relocations = relocations(image).map_err(InstallError::Native)?;
    if child_relocations > remaining.relocations {
        return Ok(code);
    }
    let Ok(loaded) = install::load_in(
        image,
        total.clone(),
        limit,
        metadata.clone(),
        workspace.clone(),
        failure,
        protection,
    ) else {
        return Ok(code);
    };
    let mut child = readonly_lookup::from_artifact(
        loaded._memory,
        loaded.entries[0],
        read_plan.unwrap(),
        source.registers,
        child_relocations,
    );
    drop(loaded.entries);
    if let Some(image) = bundle
        .modules
        .iter()
        .find(|image| image.kind == Kind::Resume)
    {
        let extra = work::Limits {
            instructions: remaining
                .instructions
                .saturating_sub(read_bound.instructions),
            blocks: remaining.blocks.saturating_sub(read_bound.blocks),
            relocations: remaining.relocations.saturating_sub(child_relocations),
        };
        if extra.instructions >= 512 && extra.blocks >= 32 {
            if let Ok(loaded) = install::load_in(
                image,
                total,
                limit,
                metadata.clone(),
                workspace.clone(),
                failure,
                protection,
            ) {
                child.integer_resume = Some((
                    resume_plan.unwrap(),
                    integer_resume::from_artifact(loaded._memory, loaded.entries[0]),
                ));
            }
        }
    }
    let previous = metadata.0.limit();
    match failure {
        Failure::RefuseReadonlyOwnerStorage => metadata.0.set_limit(metadata.0.current()),
        Failure::RefuseReadonlyOwnerAllocation => metadata.0.fail_after(0),
        _ => {}
    }
    code.readonly_lookup = crate::jit::owner::Shared::try_new(child, metadata.clone()).ok();
    match failure {
        Failure::RefuseReadonlyOwnerStorage => metadata.0.set_limit(previous),
        Failure::RefuseReadonlyOwnerAllocation => metadata.0.fail_after(usize::MAX),
        _ => {}
    }
    if code.readonly_lookup.is_some() {
        code.relocations += child_relocations;
    }
    Ok(code)
}
