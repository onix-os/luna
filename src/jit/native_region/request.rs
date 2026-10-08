use super::{boundary, region, scoped_helpers, Region};
use crate::{
    closure::FunctionPrototype,
    jit::{
        ir::Snapshot,
        owner::Shared,
        resources::{BudgetAllocator, MappingCounter},
        work::{Expansion, Limits},
        JitConfig, JitError, JitMode, Manager, Prepared, PreparedPair, Runtime,
    },
    Context,
};
use ottavino_gc_arena::Gc;

pub(crate) struct Request {
    snapshot: Snapshot,
    source: u64,
    ordinary: Prepared,
    pair: PreparedPair,
    config: JitConfig,
    memory: MappingCounter,
    metadata: BudgetAllocator,
    workspace: BudgetAllocator,
}

impl Request {
    pub(in crate::jit) fn prepare<'gc>(
        ctx: Context<'gc>,
        caller: Gc<'gc, FunctionPrototype<'gc>>,
        pair: PreparedPair,
    ) -> Result<Self, JitError> {
        let source = ctx
            .jit_registry()
            .borrow()
            .identity(ctx, caller)
            .ok_or_else(|| JitError::Compilation("unregistered region caller".into()))?;
        if pair.program.key().caller != source {
            return Err(JitError::Compilation("region pair caller".into()));
        }
        let ordinary = ctx
            .jit()
            .lookup(source)
            .ok_or_else(|| JitError::Compilation("missing region fallback".into()))?;
        let manager = ctx.jit().0.borrow();
        if manager.config.mode != JitMode::Auto
            || !manager
                .pairs
                .as_ref()
                .is_some_and(|pairs| pairs.contains(&pair))
        {
            return Err(JitError::Compilation("region dependency admission".into()));
        }
        let snapshot = Snapshot::new_in(
            &caller,
            manager.config.max_prototype_instructions,
            manager.snapshots.clone(),
        )?;
        Ok(Self {
            snapshot,
            source,
            ordinary,
            pair,
            config: manager.config.clone(),
            memory: manager.memory.clone(),
            metadata: manager.metadata.clone(),
            workspace: manager.snapshots.clone(),
        })
    }

    fn admitted(&self, manager: &Manager) -> bool {
        self.config == manager.config
            && self.memory.same_root(&manager.memory)
            && manager
                .code
                .get(&self.source)
                .is_some_and(|code| Shared::ptr_eq(&code.code, &self.ordinary.code))
            && manager
                .pairs
                .as_ref()
                .is_some_and(|pairs| pairs.contains(&self.pair))
    }

    pub(crate) fn pin_dependencies(&self) -> (Prepared, PreparedPair) {
        (
            Prepared {
                code: self.ordinary.code.clone(),
            },
            PreparedPair {
                program: self.pair.program.clone(),
            },
        )
    }

    pub(crate) fn compile(self, runtime: &Runtime) -> Result<Region, JitError> {
        if !self.admitted(&runtime.0.borrow()) {
            return Err(JitError::Compilation("stale region request".into()));
        }
        let limits = Limits::from(&self.config);
        let expansion = Expansion::admit(&self.snapshot, limits)?;
        if expansion
            .instructions
            .checked_add(64)
            .is_none_or(|count| count > limits.instructions)
            || expansion
                .blocks
                .checked_add(4)
                .is_none_or(|count| count > limits.blocks)
        {
            return Err(JitError::ResourceLimit("connected region IR"));
        }
        let caller = scoped_helpers::Code::compile_region(
            &self.snapshot,
            self.source,
            self.memory.clone(),
            self.metadata.clone(),
            &self.config,
        )?;
        drop(self.snapshot);
        let remaining = Limits {
            instructions: 64,
            blocks: 4,
            relocations: limits.relocations.saturating_sub(caller.relocations()),
        };
        let driver = region::compile(
            caller.entry(),
            boundary,
            self.memory,
            self.config.max_code_bytes,
            self.metadata,
            self.workspace,
            remaining,
            region::Fault::None,
        )?;
        Ok(Region {
            caller,
            ordinary: self.ordinary,
            pair: self.pair,
            driver,
            source: self.source,
            config: self.config,
        })
    }
}
