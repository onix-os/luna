use super::*;

pub(super) fn invoke<'gc>(
    ctx: Context<'gc>,
    host: &mut ActivationHost<'gc, '_>,
    site: &Site,
    code: &CallCode,
    budget: u32,
    prefix: u32,
    binding: Option<(Closure<'gc>, Gc<'gc, FunctionPrototype<'gc>>)>,
) -> bool {
    if budget < 4 || site.returns != 0 {
        return false;
    }
    let spec = host.with_registers(|caller, registers| {
        let caller_matches = match binding {
            Some((expected, _)) => caller == expected,
            None => {
                ctx.jit_registry()
                    .borrow()
                    .identity(ctx, caller.prototype())
                    == Some(site.caller)
            }
        };
        if *registers.pc != site.pc || !caller_matches {
            return None;
        }
        let Value::Function(Function::Closure(callee)) =
            *registers.stack_frame.get(usize::from(site.function.0))?
        else {
            return None;
        };
        if !matches_callee(ctx, site, binding.map(|(_, callee)| callee), callee)
            || usize::from(callee.prototype().stack_size) != site.registers
        {
            return None;
        }
        let pattern = site.cell()?;
        let capture = match registers
            .projection_origin(callee.upvalues().get(usize::from(pattern.upvalue))?.get())?
        {
            Origin::Upper(index, Value::Integer(_)) => (true, index),
            Origin::Register(index, Value::Integer(_)) => (false, index),
            _ => return None,
        };
        Some(crate::thread::activation::atomic_call::Spec {
            callee,
            function: site.function,
            arguments: site.arguments,
            capture,
            read: pattern.read,
            result: pattern.result,
            start: site.start,
            prefix,
        })
    });
    let Some(spec) = spec else {
        return false;
    };
    if !host.atomic_call(spec, |arguments, capture| {
        let mut frame = code.compact().prepare_arguments(arguments, capture)?;
        let output = code.compact().invoke(&mut frame).unwrap();
        Some((output.read, output.result, output.capture))
    }) {
        return false;
    }
    for _ in 0..2 {
        let mut stats = ctx.jit().interpreter_stats();
        stats.dispatches = 1;
        stats.reported_instructions = Some(0);
    }
    let mut manager = ctx.jit().0.borrow_mut();
    manager.stats.native_upvalue_reads = manager.stats.native_upvalue_reads.saturating_add(1);
    manager.stats.native_upvalue_writes = manager.stats.native_upvalue_writes.saturating_add(1);
    manager.stats.record_native_exit(&Exit {
        pc: 3,
        instructions: 3,
        reason: Kind::Interpreter as u32,
    });
    true
}

/// Run a paired call to a value leaf without a callee frame, as `invoke` does for cell leaves.
pub(super) fn invoke_value<'gc>(
    ctx: Context<'gc>,
    host: &mut ActivationHost<'gc, '_>,
    site: &Site,
    budget: u32,
    prefix: u32,
) -> bool {
    let Leaf::Value(pattern) = site.leaf else {
        return false;
    };
    if budget <= leaf::ValuePattern::RETURN_PC as u32 {
        return false;
    }
    let spec = host.with_registers(|caller, registers| {
        if *registers.pc != site.pc
            || ctx
                .jit_registry()
                .borrow()
                .identity(ctx, caller.prototype())
                != Some(site.caller)
        {
            return None;
        }
        let Value::Function(Function::Closure(callee)) =
            *registers.stack_frame.get(usize::from(site.function.0))?
        else {
            return None;
        };
        if !matches_callee(ctx, site, None, callee)
            || usize::from(callee.prototype().stack_size) != site.registers
        {
            return None;
        }
        Some(crate::thread::activation::atomic_call::ValueSpec {
            callee,
            function: site.function,
            arguments: site.arguments,
            result: pattern.result,
            prefix,
        })
    });
    let Some(spec) = spec else {
        return false;
    };
    let callee = spec.callee;
    let computed = host.atomic_value_call(spec, |arguments| {
        let argument =
            |register: crate::types::RegisterIndex| match arguments.get(usize::from(register.0)) {
                Some(Value::Integer(value)) => Some(*value),
                _ => None,
            };
        let left = argument(pattern.left)?;
        let right = match pattern.right {
            Operand::Register(register) => argument(register)?,
            Operand::Constant(index) => {
                match callee.prototype().constants.get(usize::from(index)) {
                    Some(crate::Constant::Integer(value)) => *value,
                    _ => return None,
                }
            }
        };
        Some(match pattern.arithmetic {
            leaf::Arithmetic::Add => left.wrapping_add(right),
            leaf::Arithmetic::Sub => left.wrapping_sub(right),
            leaf::Arithmetic::Mul => left.wrapping_mul(right),
        })
    });
    if !computed {
        return false;
    }
    for _ in 0..2 {
        let mut stats = ctx.jit().interpreter_stats();
        stats.dispatches = 1;
        stats.reported_instructions = Some(0);
    }
    ctx.jit().0.borrow_mut().stats.record_native_exit(&Exit {
        pc: leaf::ValuePattern::RETURN_PC as u64,
        instructions: leaf::ValuePattern::RETURN_PC as u32,
        reason: Kind::Interpreter as u32,
    });
    true
}
