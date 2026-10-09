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
        let capture = match registers.projection_origin(
            callee
                .upvalues()
                .get(usize::from(site.pattern.upvalue))?
                .get(),
        )? {
            Origin::Upper(index, Value::Integer(_)) => (true, index),
            Origin::Register(index, Value::Integer(_)) => (false, index),
            _ => return None,
        };
        Some(crate::thread::activation::atomic_call::Spec {
            callee,
            function: site.function,
            arguments: site.arguments,
            capture,
            read: site.pattern.read,
            result: site.pattern.result,
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
    let mut manager = ctx.jit().0.borrow_mut();
    manager.stats.record_atomic_call();
    true
}
