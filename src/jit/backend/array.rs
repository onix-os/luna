use super::*;
use crate::jit::{array_window as window, work};

struct Binding {
    plan: window::Plan,
    entry: window::KernelEntry,
}

pub(in crate::jit) struct Kernels {
    _memory: Memory,
    bindings: BudgetVec<Binding, BudgetAllocator>,
    registers: usize,
}

impl Kernels {
    pub(in crate::jit) fn invoke<'gc>(
        &self,
        ctx: crate::Context<'gc>,
        canonical: &[crate::Value<'gc>],
        slots: &mut [Slot],
        pc: usize,
        budget: u32,
    ) -> Option<window::Outcome> {
        if slots.len() != self.registers || canonical.len() < self.registers {
            return None;
        }
        let binding = self
            .bindings
            .iter()
            .find(|binding| (binding.plan.start..=binding.plan.end).contains(&pc))?;
        unsafe {
            window::invoke_kernel(
                binding.entry,
                binding.plan,
                ctx,
                &canonical[..self.registers],
                slots,
                pc,
                budget,
            )
        }
    }
}

pub(in crate::jit) fn compile_pair(
    source: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    limits: work::Limits,
    #[cfg(test)] failure: Failure,
) -> Result<Code, JitError> {
    let mut ordinary = compile_in(
        source,
        total.clone(),
        limit,
        metadata.clone(),
        limits,
        #[cfg(test)]
        failure,
    )?;
    ordinary.array_kernels = compile(
        source,
        total,
        limit,
        metadata,
        limits,
        #[cfg(test)]
        failure,
    )
    .ok();
    Ok(ordinary)
}

fn compile(
    source: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    limits: work::Limits,
    #[cfg(test)] failure: Failure,
) -> Result<Kernels, JitError> {
    let expansion = work::Expansion::admit(
        source,
        work::Limits {
            instructions: limits.instructions / 2,
            blocks: limits.blocks / 2,
            relocations: limits.relocations,
        },
    )?;
    let workspace = source.operations.allocator().clone();
    let mut plans = BudgetVec::new_in(workspace.clone());
    for end in 0..source.operations.len() {
        if let Some(plan) = window::Plan::new(source, end) {
            plans
                .try_reserve(1)
                .map_err(|_| JitError::ResourceLimit("array plans"))?;
            plans.push(plan);
        }
    }
    if plans.is_empty() {
        return Err(JitError::Compilation("no array loop regions".into()));
    }
    let mut bindings = BudgetVec::new_in(metadata.clone());
    bindings
        .try_reserve_exact(plans.len())
        .map_err(|_| JitError::ResourceLimit("array bindings"))?;
    let mut ids = BudgetVec::new_in(workspace.clone());
    ids.try_reserve_exact(plans.len())
        .map_err(|_| JitError::ResourceLimit("array function identities"))?;
    let _templates = Reservation::new(workspace.0.clone(), 64 * 1024)
        .map_err(|_| JitError::ResourceLimit("array template workspace"))?;
    let signature_bytes = plans
        .len()
        .checked_mul(3 * 5 * Layout::new::<AbiParam>().size())
        .ok_or(JitError::ResourceLimit("array signatures"))?;
    let _signatures = Reservation::new(workspace.0.clone(), signature_bytes)
        .map_err(|_| JitError::ResourceLimit("array signatures"))?;
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
    .map_err(|_| JitError::ResourceLimit("array memory handoff"))?;
    let (provider, _provider_charge) = global_box::try_new(Provider(memory.clone()), metadata)
        .map_err(|_| JitError::ResourceLimit("array memory provider"))?;
    let mut jit = native_builder(cranelift_native::builder())?;
    jit.memory_provider(provider);
    let mut module = JITModule::new(jit);
    let (mut instructions, mut blocks) = (0usize, 0usize);
    for &plan in &plans {
        let mut context = module.make_context();
        context.func = window::program(source, plan, module.isa())?;
        for block in context.func.layout.blocks() {
            blocks = blocks
                .checked_add(1)
                .ok_or(JitError::ResourceLimit("array blocks"))?;
            for inst in context.func.layout.block_insts(block) {
                instructions = instructions
                    .checked_add(1)
                    .ok_or(JitError::ResourceLimit("array instructions"))?;
                if matches!(
                    context.func.dfg.insts[inst].opcode(),
                    cranelift_codegen::ir::Opcode::Call
                        | cranelift_codegen::ir::Opcode::CallIndirect
                        | cranelift_codegen::ir::Opcode::ReturnCall
                        | cranelift_codegen::ir::Opcode::ReturnCallIndirect
                ) {
                    return Err(JitError::Compilation("array kernel contains a call".into()));
                }
            }
        }
        expansion.verify_actual(instructions, blocks)?;
        let id = module
            .declare_anonymous_function(&context.func.signature)
            .map_err(fail)?;
        context
            .compile(module.isa(), &mut Default::default())
            .map_err(|error| fail(error.into()))?;
        let compiled = context.compiled_code().unwrap();
        if !compiled.buffer.relocs().is_empty() {
            return Err(JitError::Compilation("array unexpected relocation".into()));
        }
        module
            .define_function_bytes(
                id,
                u64::from(compiled.buffer.alignment),
                compiled.code_buffer(),
                &[],
            )
            .map_err(fail)?;
        ids.push(id);
    }
    module.finalize_definitions().map_err(fail)?;
    for (plan, id) in plans.into_iter().zip(ids) {
        let entry = unsafe {
            std::mem::transmute::<*const u8, window::KernelEntry>(module.get_finalized_function(id))
        };
        bindings.push(Binding { plan, entry });
    }
    let image = memory
        .take()
        .ok_or_else(|| JitError::Compilation("missing array mappings".into()))?;
    drop(module);
    Ok(Kernels {
        _memory: image,
        bindings,
        registers: source.registers,
    })
}

#[cfg(test)]
mod tests;
