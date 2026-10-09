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

    fn policy(&self, host: &mut ActivationHost<'gc, '_>, budget: u32) -> bool {
        if !host.lua_ready() {
            return false;
        }
        host.clear_hook(self.ctx);
        if budget < 4
            || self.ctx.hook_enabled()
            || self.ctx.jit().0.borrow().config.mode != crate::JitMode::Auto
            || host.frame_identity() != self.identity
        {
            return false;
        }
        true
    }

    fn operands(&self, host: &mut ActivationHost<'gc, '_>, prefix: u32) -> bool {
        let site = &self.program.site;
        let mut fuel = host.fuel().clone();
        fuel.consume(prefix.try_into().unwrap());
        fuel.consume(8 + i32::from(site.arguments));
        if !fuel.should_continue() {
            return false;
        }
        host.with_registers(|caller, registers| {
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
        })
    }

    pub(in crate::jit) fn invoke(
        &self,
        host: &mut ActivationHost<'gc, '_>,
        budget: u32,
        prefix: u32,
    ) -> Option<crate::jit::PairOutcome> {
        if !self.policy(host, budget) {
            return None;
        }
        #[cfg(test)]
        let atomic = !self.program.custom_hooks;
        #[cfg(not(test))]
        let atomic = true;
        if atomic
            && super::atomic::invoke(
                self.ctx,
                host,
                &self.program.site,
                &self.program.code,
                budget,
                prefix,
                Some((self.caller, self.callee)),
            )
        {
            self.ctx.jit().record_pair_execution(1, 1);
            return Some(crate::jit::PairOutcome {
                calls: 1,
                returns: 1,
                result: Ok(()),
            });
        }
        if !self.operands(host, prefix) {
            return None;
        }
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

    pub(in crate::jit) fn invoke_rooted(
        &self,
        host: &mut ActivationHost<'gc, '_>,
        budget: u32,
        prefix: u32,
        snapshot: &mut abi::roots::call::Snapshot<'_, 'gc>,
    ) -> bool {
        #[cfg(test)]
        if self.program.custom_hooks {
            return false;
        }
        if !self.policy(host, budget) {
            return false;
        }
        if !super::rooted::invoke(
            self.ctx,
            host,
            &self.program.site,
            &self.program.code,
            budget,
            prefix,
            (self.caller, self.callee),
            snapshot,
        ) {
            return false;
        }
        self.ctx.jit().record_pair_execution(1, 1);
        true
    }
}
