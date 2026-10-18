use super::*;
use crate::jit::{
    inline_loop::{
        native::{
            bridge::{Binding, Progress},
            State,
        },
        Descriptor, Plan,
    },
    pairs::Key,
    resources::LedgerRef,
    work::Limits,
};
use crate::{thread::activation::ActivationHost, Context};

type InlineEntry = unsafe extern "C" fn(*mut State);

pub(in crate::jit) struct Code {
    _memory: Memory,
    entry: InlineEntry,
    descriptor: Descriptor,
    key: Key,
}

impl Code {
    pub(in crate::jit) fn compile(
        key: Key,
        caller: &Snapshot,
        callee: &Snapshot,
        total: MappingCounter,
        limit: usize,
        metadata: BudgetAllocator,
        limits: Limits,
        #[cfg(test)] failure: Failure,
    ) -> Result<Self, JitError> {
        let workspace = caller.operations.allocator().clone();
        if !LedgerRef::ptr_eq(&workspace.0, &callee.operations.allocator().0)
            || !total.same_root(&MappingCounter::new(workspace.0.clone()))
            || !total.same_root(&MappingCounter::new(metadata.0.clone()))
        {
            return Err(JitError::Compilation(
                "inline loop ledger provenance".into(),
            ));
        }
        let plan = Plan::new(caller, callee, key.pc, limits)?;
        let descriptor = plan.descriptor()?;
        let _templates = Reservation::new(workspace.0.clone(), 32 * 1024)
            .map_err(|_| JitError::ResourceLimit("inline loop template workspace"))?;
        let _signatures =
            Reservation::new(workspace.0.clone(), 3 * Layout::new::<AbiParam>().size())
                .map_err(|_| JitError::ResourceLimit("inline loop signatures"))?;
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
        jit.memory_provider(provider);
        let mut module = JITModule::new(jit);
        let config = module.isa().frontend_config();
        let convention = module.isa().default_call_conv();
        let function = plan.function(config, convention)?;
        plan.verify_function(&function, config, convention)?;
        cranelift_codegen::verify_function(&function, module.isa())
            .map_err(|error| JitError::Compilation(error.to_string()))?;
        let id = module
            .declare_anonymous_function(&function.signature)
            .map_err(fail)?;
        if id.as_u32() != 0 {
            return Err(JitError::Compilation(
                "inline loop function identity".into(),
            ));
        }
        let mut context = module.make_context();
        context.func = function;
        context
            .compile(module.isa(), &mut Default::default())
            .map_err(|error| fail(error.into()))?;
        let compiled = context.compiled_code().unwrap();
        if !compiled.buffer.relocs().is_empty() {
            return Err(JitError::Compilation(
                "inline loop unexpected relocation".into(),
            ));
        }
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
            .ok_or_else(|| JitError::Compilation("missing inline loop mappings".into()))?;
        drop(module);
        Ok(Self {
            _memory: image,
            entry: unsafe { std::mem::transmute::<*const u8, InlineEntry>(pointer) },
            descriptor,
            key,
        })
    }

    pub(in crate::jit) fn run<'gc>(
        &self,
        ctx: Context<'gc>,
        host: &mut ActivationHost<'gc, '_>,
        limit: usize,
        budget: u32,
        prefix: u32,
    ) -> Option<Progress> {
        if !self._memory.total.same_root(&ctx.jit().0.borrow().memory) {
            return None;
        }
        let binding = {
            let registry = ctx.jit_registry().borrow();
            Binding::bind(
                self.descriptor,
                registry.resolve(ctx, self.key.caller)?,
                registry.resolve(ctx, self.key.callee)?,
            )?
        };
        let mut exit = None;
        let progress = binding.run(ctx, host, limit, budget, prefix, |state| {
            unsafe { (self.entry)(state) };
            exit = Some(state.exit());
        });
        if let Some(mut exit) = exit {
            exit.instructions = progress.as_ref().map_or(0, |p| p.instructions - prefix);
            ctx.jit().0.borrow_mut().stats.record_native_exit(&exit);
        }
        progress
    }
}

#[cfg(test)]
mod tests;
