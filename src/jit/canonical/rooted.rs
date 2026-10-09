use super::*;
use crate::jit::abi::roots::call::Snapshot;

pub(super) fn invoke<'gc>(
    ctx: Context<'gc>,
    host: &mut ActivationHost<'gc, '_>,
    site: &Site,
    code: &CallCode,
    budget: u32,
    prefix: u32,
    binding: (Closure<'gc>, Gc<'gc, FunctionPrototype<'gc>>),
    snapshot: &mut Snapshot<'_, 'gc>,
) -> bool {
    if budget < 4 || site.returns != 0 {
        return false;
    }
    let function = usize::from(site.function.0);
    let Some(Value::Function(Function::Closure(callee))) = snapshot.get(function) else {
        return false;
    };
    if !Gc::ptr_eq(callee.prototype(), binding.1)
        || usize::from(callee.prototype().stack_size) != site.registers
    {
        return false;
    }
    let input = host.with_registers(|caller, registers| {
        if caller != binding.0 {
            return None;
        }
        let origin = registers.projection_origin(
            callee
                .upvalues()
                .get(usize::from(site.pattern.upvalue))?
                .get(),
        )?;
        match origin {
            Origin::Upper(index, Value::Integer(value)) => Some(((true, index), value)),
            Origin::Register(index, _) => {
                let Value::Integer(value) = snapshot.get(index)? else {
                    return None;
                };
                Some(((false, index), value))
            }
            _ => None,
        }
    });
    #[cfg(test)]
    assert_eq!(
        input,
        callee
            .upvalues()
            .get(usize::from(site.pattern.upvalue))
            .and_then(|value| {
                host.snapshot_capture(binding.0, value.get(), |index| snapshot.integer(index))
            })
    );
    let Some((capture, value)) = input else {
        return false;
    };
    let right = match site.pattern.right {
        Operand::Register(index) if index != site.pattern.read => {
            if index.0 >= site.arguments {
                return false;
            }
            let Some(Value::Integer(value)) = snapshot.get(function + 1 + usize::from(index.0))
            else {
                return false;
            };
            Some(value)
        }
        _ => None,
    };
    let spec = crate::thread::activation::atomic_call::Spec {
        callee,
        function: site.function,
        arguments: site.arguments,
        capture,
        read: site.pattern.read,
        result: site.pattern.result,
        start: site.start,
        prefix,
    };
    let Some(window) = host.snapshot_window(&spec, binding.0, snapshot.len(), site.pc) else {
        return false;
    };
    let Some(mut frame) = code.compact().prepare_inputs(value, right) else {
        return false;
    };
    let output = code.compact().invoke(&mut frame).unwrap();
    assert!(snapshot.call(
        function,
        usize::from(site.arguments),
        site.registers,
        usize::from(site.pattern.read.0),
        usize::from(site.pattern.result.0),
        (!capture.0).then_some(capture.1),
        (output.read, output.result, output.capture)
    ));
    window.commit(output.capture);
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
