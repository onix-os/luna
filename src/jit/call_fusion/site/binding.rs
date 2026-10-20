use ottavino_gc_arena::Gc;

use super::{abi, Argument, Leaf, Site, Snapshot};
use crate::{
    jit::{projection::Origin, resources::BudgetAllocator, JitError},
    thread::LuaRegisters,
    Closure, Function, FunctionPrototype, Value,
};

#[cfg(test)]
mod tests;

pub(in crate::jit) struct Input {
    parent: Snapshot,
    child: Snapshot,
    site: Site,
    fixed_params: u8,
    varargs: bool,
}

struct Bound<'entry, 'gc, 'frame> {
    site: Site,
    sum: i64,
    argument: i64,
    _registers: &'entry LuaRegisters<'gc, 'frame>,
    _scope: &'entry mut (),
}

impl Input {
    pub(in crate::jit) fn new(
        parent: &FunctionPrototype<'_>,
        prototype: u8,
        max_instructions: usize,
        allocator: BudgetAllocator,
    ) -> Result<Option<Self>, JitError> {
        let Some(child) = parent.prototypes.get(usize::from(prototype)) else {
            return Ok(None);
        };
        let instructions = parent
            .opcodes
            .len()
            .checked_add(child.opcodes.len())
            .ok_or(JitError::ResourceLimit("fused source size overflow"))?;
        if instructions > max_instructions {
            return Err(JitError::ResourceLimit("fused source instructions"));
        }
        if parent.opcodes.len() > 256
            || parent.constants.len() > 256
            || child.opcodes.len() != 4
            || !child.constants.is_empty()
        {
            return Ok(None);
        }
        let child_source = Snapshot::new_in(child, 4, allocator.clone())?;
        let Some(leaf) = Leaf::recognize(
            &child_source,
            child.fixed_params,
            child.has_varargs,
            &child.upvalues,
        ) else {
            return Ok(None);
        };
        let parent_source = Snapshot::new_in(parent, 256, allocator)?;
        if parent_source
            .constants
            .iter()
            .any(|s| s.tag == abi::REFERENCE)
        {
            return Ok(None);
        }
        let Some(site) = Site::recognize(&parent_source, leaf, prototype) else {
            return Ok(None);
        };
        Ok(Some(Self {
            parent: parent_source,
            child: child_source,
            site,
            fixed_params: parent.fixed_params,
            varargs: parent.has_varargs,
        }))
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    pub(super) fn sources(&self) -> (&Snapshot, &Snapshot, Site) {
        (&self.parent, &self.child, self.site)
    }

    fn with_binding<'gc, 'frame, R>(
        &self,
        caller: Closure<'gc>,
        registers: &LuaRegisters<'gc, 'frame>,
        run: impl for<'entry> FnOnce(Bound<'entry, 'gc, 'frame>) -> R,
    ) -> Option<R> {
        let prototype = caller.prototype();
        if !registers.fixed_list_stack()
            || *registers.pc != self.site.region_pc as usize
            || registers.stack_frame.len() != usize::from(self.site.registers)
            || prototype.fixed_params != self.fixed_params
            || prototype.has_varargs != self.varargs
            || !matches_source(&self.parent, &prototype)
        {
            return None;
        }
        let Value::Function(Function::Closure(callee)) =
            *registers.stack_frame.get(usize::from(self.site.holder))?
        else {
            return None;
        };
        let child = *prototype.prototypes.get(usize::from(self.site.prototype))?;
        if !Gc::ptr_eq(callee.prototype(), child)
            || child.fixed_params != 1
            || child.has_varargs
            || child.upvalues.as_ref()
                != [crate::types::UpValueDescriptor::ParentLocal(
                    crate::types::RegisterIndex(self.site.leaf.parent),
                )]
            || !matches_source(&self.child, &child)
        {
            return None;
        }
        let [capture] = callee.upvalues() else {
            return None;
        };
        let Origin::Register(index, Value::Integer(sum)) =
            registers.projection_origin(capture.get())?
        else {
            return None;
        };
        if index != usize::from(self.site.leaf.parent) {
            return None;
        }
        let argument = match self.site.argument {
            Argument::Integer(value) => value,
            Argument::Register(index) => {
                let Value::Integer(value) = *registers.stack_frame.get(usize::from(index))? else {
                    return None;
                };
                value
            }
        };
        let mut scope = ();
        Some(run(Bound {
            site: self.site,
            sum,
            argument,
            _registers: registers,
            _scope: &mut scope,
        }))
    }
}

fn matches_source(source: &Snapshot, prototype: &FunctionPrototype<'_>) -> bool {
    source.registers == usize::from(prototype.stack_size)
        && source.upvalues == prototype.upvalues.len()
        && source.prototypes == prototype.prototypes.len()
        && source.operations.len() == prototype.opcodes.len()
        && source.constants.len() == prototype.constants.len()
        && source
            .operations
            .iter()
            .zip(prototype.opcodes.iter())
            .all(|(expected, actual)| *expected == actual.decode())
        && source
            .constants
            .iter()
            .zip(prototype.constants.iter())
            .all(|(expected, &actual)| {
                let actual = abi::Slot::from_constant(actual);
                actual.tag == expected.tag && actual.bits == expected.bits
            })
}
