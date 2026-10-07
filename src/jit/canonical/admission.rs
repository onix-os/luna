use super::*;
use crate::{Closure, FunctionPrototype};
use ottavino_gc_arena::Gc;

pub(in crate::jit) struct Admitted<'program, 'gc> {
    program: &'program Program,
    ctx: Context<'gc>,
    caller: Closure<'gc>,
    callee: Gc<'gc, FunctionPrototype<'gc>>,
    identity: (usize, usize),
}

impl<'program, 'gc> Admitted<'program, 'gc> {
    pub(in crate::jit) fn caller(&self) -> Closure<'gc> {
        self.caller
    }

    pub(in crate::jit) fn new(
        program: &'program Program,
        ctx: Context<'gc>,
        caller: Closure<'gc>,
        identity: (usize, usize),
    ) -> Option<Self> {
        let manager = ctx.jit().0.borrow();
        if manager.config.mode != crate::JitMode::Auto
            || !program.origin.same_root(&manager.memory)
            || ctx.hook_enabled()
        {
            return None;
        }
        drop(manager);
        let registry = ctx.jit_registry().borrow();
        let site = &program.site;
        if registry.identity(ctx, caller.prototype()) != Some(site.caller) || site.registers > 256 {
            return None;
        }
        let callee = registry.resolve(ctx, site.callee)?;
        if usize::from(callee.stack_size) != site.registers {
            return None;
        }
        let Operation::Call {
            func,
            args,
            returns,
        } = caller.prototype().opcodes.get(site.pc)?.decode()
        else {
            return None;
        };
        if func != site.function
            || args.to_constant() != Some(site.arguments)
            || returns.to_constant() != Some(0)
        {
            return None;
        }
        match site.pattern.right {
            Operand::Register(register) if register != site.pattern.read => {
                if register.0 >= site.arguments || register.0 >= callee.fixed_params {
                    return None;
                }
            }
            Operand::Constant(index) => {
                if !matches!(
                    callee.constants.get(usize::from(index)),
                    Some(crate::Constant::Integer(_))
                ) {
                    return None;
                }
            }
            _ => {}
        }
        Some(Self {
            program,
            ctx,
            caller,
            callee,
            identity,
        })
    }

    #[cfg(test)]
    fn preflight(&self, host: &mut ActivationHost<'gc, '_>, budget: u32, prefix: u32) -> bool {
        if !self.preflight_state(
            host.lua_ready(),
            host.frame_identity(),
            host.fuel(),
            budget,
            prefix,
        ) {
            return false;
        }
        host.with_registers(|caller, registers| self.preflight_registers(caller, &registers))
    }

    fn preflight_state(
        &self,
        ready: bool,
        identity: (usize, usize),
        available: &crate::Fuel,
        budget: u32,
        prefix: u32,
    ) -> bool {
        if !ready {
            return false;
        }
        self.ctx.clear_hook_at(identity.1);
        if budget < 4
            || self.ctx.hook_enabled()
            || self.ctx.jit().0.borrow().config.mode != crate::JitMode::Auto
            || identity != self.identity
        {
            return false;
        }
        let site = &self.program.site;
        let mut fuel = available.clone();
        fuel.consume(prefix.try_into().unwrap());
        fuel.consume(8 + i32::from(site.arguments));
        if !fuel.should_continue() {
            return false;
        }
        true
    }

    fn preflight_registers(
        &self,
        caller: Closure<'gc>,
        registers: &crate::thread::LuaRegisters<'gc, '_>,
    ) -> bool {
        let site = &self.program.site;
        if caller != self.caller || *registers.pc != site.pc {
            return false;
        }
        let index = usize::from(site.function.0);
        let Some(Value::Function(Function::Closure(callee))) =
            registers.stack_frame.get(index).copied()
        else {
            return false;
        };
        if !Gc::ptr_eq(callee.prototype(), self.callee) {
            return false;
        }
        let Some(upvalue) = callee.upvalues().get(usize::from(site.pattern.upvalue)) else {
            return false;
        };
        let Some(origin) = registers.projection_origin(upvalue.get()) else {
            return false;
        };
        let capture = match origin {
            Origin::Upper(_, value) | Origin::Register(_, value) => value,
            Origin::Closed(_) => return false,
        };
        if !matches!(capture, Value::Integer(_))
            || index + 1 + usize::from(site.arguments) > registers.stack_frame.len()
        {
            return false;
        }
        match site.pattern.right {
            Operand::Register(register) if register != site.pattern.read => matches!(
                registers
                    .stack_frame
                    .get(index + 1 + usize::from(register.0)),
                Some(Value::Integer(_))
            ),
            _ => true,
        }
    }

    #[cfg(test)]
    pub(in crate::jit) fn invoke(
        &self,
        host: &mut ActivationHost<'gc, '_>,
        budget: u32,
        prefix: u32,
    ) -> Option<crate::jit::PairOutcome> {
        if !self.preflight(host, budget, prefix) {
            return None;
        }
        self.invoke_preflighted(host, budget, prefix)
    }

    pub(in crate::jit) fn materialize_and_invoke(
        &self,
        host: &mut ActivationHost<'gc, '_>,
        budget: u32,
        prefix: u32,
        materialize: impl FnOnce(Closure<'gc>, &mut crate::thread::LuaRegisters<'gc, '_>) -> bool,
    ) -> Option<crate::jit::PairOutcome> {
        let ready = host.lua_ready();
        let identity = host.frame_identity();
        #[cfg(test)]
        let declined = host.test_pairs_declined();
        #[cfg(not(test))]
        let declined = false;
        let admitted = host.with_registers(|caller, mut registers| {
            materialize(caller, &mut registers)
                && !declined
                && self.ctx.jit().call_pairs_enabled()
                && self.preflight_state(ready, identity, registers.fuel(), budget, prefix)
                && self.preflight_registers(caller, &registers)
        });
        if !admitted {
            return None;
        }
        self.invoke_preflighted(host, budget, prefix)
    }

    #[inline(never)]
    fn invoke_preflighted(
        &self,
        host: &mut ActivationHost<'gc, '_>,
        budget: u32,
        prefix: u32,
    ) -> Option<crate::jit::PairOutcome> {
        let mut scratch = [MaybeUninit::uninit(); 256];
        let mut session = Session::new(self.ctx, host, &self.program.site, &mut scratch);
        session.callee = Some(self.callee);
        session.prefix = prefix;
        #[cfg(test)]
        if self.program.custom_hooks {
            unsafe {
                self.program
                    .code
                    .invoke(std::ptr::addr_of_mut!(session).cast(), budget.min(64));
            }
        } else {
            session.invoke_compact(&self.program.code, budget.min(64));
        }
        #[cfg(not(test))]
        session.invoke_compact(&self.program.code, budget.min(64));
        let outcome = session.finish();
        if outcome.calls == 0 {
            return None;
        }
        self.ctx.jit().record_pair_execution(
            outcome.calls,
            if outcome.result.is_ok() {
                outcome.returns
            } else {
                0
            },
        );
        Some(outcome)
    }
}
