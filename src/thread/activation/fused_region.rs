use super::*;
use crate::jit::call_fusion::runtime::Prepared;

#[cfg(test)]
mod tests;

impl<'gc> LuaFrame<'gc, '_> {
    pub(in crate::thread) fn try_fused_region(
        &mut self,
        ctx: Context<'gc>,
        prepared: &Prepared,
        requested: u32,
        deferred: u32,
    ) -> Option<u32> {
        if ctx.hook_enabled() || !ctx.jit().active() {
            return None;
        }
        let budget = prepared.funded(self.fuel, deferred, requested)?;
        let region = prepared.region();
        let Some(Frame::Lua {
            base,
            stack_size,
            pc,
            closure,
            is_variable: false,
            expected_return: None,
            ..
        }) = self.state.frames.last()
        else {
            return None;
        };
        let caller = *closure;
        let top = base.checked_add(region.registers)?;
        let bottom = base.checked_add(region.function)?;
        if *stack_size != region.registers
            || *pc != region.entry
            || region.registers > 256
            || region.function.checked_add(2)? > region.registers
            || region.resume >= caller.prototype().opcodes.len()
            || region.entry.checked_add(3)? != region.resume
            || self.stack.len() != top
            || self.stack.capacity() < bottom.checked_add(region.callee)?
            || self.state.frames.len() >= self.state.max_call_depth
            || self.state.frames.len() >= self.state.frames.capacity()
            || !self.state.captures_below(bottom)
        {
            return None;
        }
        let completed = prepared.invoke(ctx, caller, &mut self.registers(), budget)?;
        self.fuel.consume(prepared.extra_fuel());
        Some(completed)
    }
}
