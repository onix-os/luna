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
    inline_callee: Option<Snapshot>,
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
        let inline_callee = (snapshot.registers <= 16)
            .then(|| {
                let callee = ctx
                    .jit_registry()
                    .borrow()
                    .resolve(ctx, pair.program.key().callee)?;
                let source = Snapshot::new_in(
                    &callee,
                    manager.config.max_prototype_instructions,
                    manager.snapshots.clone(),
                )
                .ok()?;
                crate::jit::inline_loop::Plan::new(
                    &snapshot,
                    &source,
                    pair.program.key().pc,
                    Limits::from(&manager.config),
                )
                .ok()?;
                Some(source)
            })
            .flatten();
        Ok(Self {
            snapshot,
            inline_callee,
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
        let mut inline_callee = self.inline_callee;
        let compile_caller = || {
            scoped_helpers::Code::compile_region(
                &self.snapshot,
                self.source,
                self.memory.clone(),
                self.metadata.clone(),
                &self.config,
            )
        };
        let caller = match compile_caller() {
            Err(JitError::ResourceLimit(_)) if inline_callee.is_some() => {
                drop(inline_callee.take());
                compile_caller()?
            }
            result => result?,
        };
        let mut snapshot = Some(self.snapshot);
        if inline_callee.is_none() {
            drop(snapshot.take());
        }
        let remaining = Limits {
            instructions: 64,
            blocks: 4,
            relocations: limits.relocations.saturating_sub(caller.relocations()),
        };
        let compile_driver = || {
            region::compile(
                caller.entry(),
                boundary,
                self.memory.clone(),
                self.config.max_code_bytes,
                self.metadata.clone(),
                self.workspace.clone(),
                remaining,
                region::Fault::None,
            )
        };
        let driver = match compile_driver() {
            Err(JitError::ResourceLimit(_)) if snapshot.is_some() => {
                drop(snapshot.take());
                drop(inline_callee.take());
                compile_driver()?
            }
            result => result?,
        };
        let inline = snapshot
            .as_ref()
            .zip(inline_callee.as_ref())
            .and_then(|(caller, callee)| {
                let code = crate::jit::backend::inline_loop::Code::compile(
                    self.pair.program.key(),
                    caller,
                    callee,
                    self.memory.clone(),
                    self.config.max_code_bytes,
                    self.metadata.clone(),
                    Limits {
                        instructions: limits.instructions - expansion.instructions - 64,
                        blocks: limits.blocks - expansion.blocks - 4,
                        relocations: limits.relocations,
                    },
                    #[cfg(test)]
                    crate::jit::backend::Failure::None,
                )
                .ok()?;
                Shared::try_new(code, self.metadata.clone()).ok()
            });
        Ok(Region {
            caller,
            ordinary: self.ordinary,
            pair: self.pair,
            driver,
            inline,
            source: self.source,
            config: self.config,
        })
    }
}
