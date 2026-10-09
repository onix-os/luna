use allocator_api2::vec;
use ottavino_gc_arena::{allocator_api::MetricsAlloc, lock::Lock, Gc};

use crate::{
    closure::UpValue, thread::thread::LuaRegisters, types::UpValueDescriptor, Closure, Context,
    FunctionPrototype,
};

#[inline(never)]
pub(super) fn closure<'gc>(
    ctx: Context<'gc>,
    registers: &mut LuaRegisters<'gc, '_>,
    proto: Gc<'gc, FunctionPrototype<'gc>>,
    outer: &[Lock<UpValue<'gc>>],
) -> Result<Closure<'gc>, super::VMError> {
    let mut upvalues = vec::Vec::with_capacity_in(proto.upvalues.len(), MetricsAlloc::new(&ctx));
    for &desc in proto.upvalues.iter() {
        match desc {
            UpValueDescriptor::Environment => return Err(super::VMError::BadEnvUpValue),
            UpValueDescriptor::ParentLocal(reg) => {
                upvalues.push(Lock::new(registers.open_upvalue(&ctx, reg)));
            }
            UpValueDescriptor::Outer(index) => {
                upvalues.push(Lock::new(outer[index.0 as usize].get()));
            }
        }
    }
    Ok(Closure::from_parts(&ctx, proto, upvalues))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        closure::UpValueState,
        thread::thread::{Frame, LuaFrame},
        types::{RegisterIndex, UpValueIndex},
        Fuel, Lua, Thread, Value,
    };

    fn with_frame<'gc>(ctx: Context<'gc>, test: impl FnOnce(&mut LuaFrame<'gc, '_>)) {
        let thread = Thread::new(ctx).into_inner();
        let caller = Closure::load(ctx, None, &b"return"[..]).unwrap();
        let mut state = thread.borrow_mut(&ctx);
        let storage = state.stack;
        let mut stack = storage.borrow_mut(&ctx);
        stack.extend([Value::Integer(11), Value::Integer(22), Value::Integer(33)]);
        state.frames.push(Frame::Lua {
            closure: caller,
            bottom: 0,
            base: 1,
            pc: 17,
            is_variable: false,
            stack_size: 2,
            expected_return: None,
        });
        let mut fuel = Fuel::with(7);
        fuel.interrupt();
        test(&mut LuaFrame {
            #[cfg(all(
                feature = "jit",
                not(miri),
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            pair_handoff: None,
            state: &mut state,
            #[cfg(feature = "jit")]
            stack: &mut stack,
            #[cfg(not(feature = "jit"))]
            stack,
            fuel: &mut fuel,
        });
    }

    fn prototype<'gc>(
        ctx: Context<'gc>,
        descriptors: &[UpValueDescriptor],
    ) -> Gc<'gc, FunctionPrototype<'gc>> {
        let mut proto = FunctionPrototype::compile(ctx, "capture-boundary", b"return").unwrap();
        let mut values = vec::Vec::new_in(MetricsAlloc::new(&ctx));
        values.extend_from_slice(descriptors);
        proto.upvalues = values.into_boxed_slice();
        Gc::new(&ctx, proto)
    }

    #[test]
    fn instantiation_shares_cells_without_sharing_slots() {
        Lua::empty().enter(|ctx| {
            let proto = prototype(
                ctx,
                &[
                    UpValueDescriptor::ParentLocal(RegisterIndex(0)),
                    UpValueDescriptor::ParentLocal(RegisterIndex(0)),
                    UpValueDescriptor::ParentLocal(RegisterIndex(1)),
                    UpValueDescriptor::Outer(UpValueIndex(0)),
                    UpValueDescriptor::Outer(UpValueIndex(0)),
                ],
            );
            let captured = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(77)));
            let outer = [Lock::new(captured)];
            with_frame(ctx, |frame| {
                let a = closure(ctx, &mut frame.registers(), proto, &outer).unwrap();
                let b = closure(ctx, &mut frame.registers(), proto, &outer).unwrap();
                assert_eq!(frame.state.open_upvalues.len(), 2);
                assert!(Gc::ptr_eq(a.prototype(), proto));
                for i in 0..5 {
                    assert!(Gc::ptr_eq(
                        a.upvalues()[i].get().into_inner(),
                        b.upvalues()[i].get().into_inner()
                    ));
                }
                assert!(Gc::ptr_eq(
                    a.upvalues()[0].get().into_inner(),
                    a.upvalues()[1].get().into_inner()
                ));
                assert!(Gc::ptr_eq(
                    a.upvalues()[3].get().into_inner(),
                    captured.into_inner()
                ));
                let replacement = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(99)));
                a.set_upvalue(&ctx, 3, replacement);
                assert!(Gc::ptr_eq(
                    outer[0].get().into_inner(),
                    captured.into_inner()
                ));
                assert!(Gc::ptr_eq(
                    a.upvalues()[4].get().into_inner(),
                    captured.into_inner()
                ));
                assert!(Gc::ptr_eq(
                    b.upvalues()[3].get().into_inner(),
                    captured.into_inner()
                ));
                let mut registers = frame.registers();
                registers.set_upvalue(&ctx, a.upvalues()[0].get(), Value::Integer(42));
                assert!(matches!(registers.stack_frame[0], Value::Integer(42)));
                assert!(matches!(registers.stack_frame[1], Value::Integer(33)));
                assert_eq!(*registers.pc, 17);
                drop(registers);
                assert_eq!(frame.fuel.remaining(), 7);
                assert!(frame.fuel.is_interrupted());
            });
        });
    }

    #[test]
    fn invalid_environment_preserves_prior_capture_order() {
        Lua::empty().enter(|ctx| {
            for position in 0..=2 {
                let mut descriptors = std::vec![
                    UpValueDescriptor::ParentLocal(RegisterIndex(0)),
                    UpValueDescriptor::ParentLocal(RegisterIndex(1)),
                ];
                descriptors.insert(position, UpValueDescriptor::Environment);
                let proto = prototype(ctx, &descriptors);
                with_frame(ctx, |frame| {
                    assert!(matches!(
                        closure(ctx, &mut frame.registers(), proto, &[]),
                        Err(super::super::VMError::BadEnvUpValue)
                    ));
                    assert_eq!(frame.state.open_upvalues.len(), position);
                    let registers = frame.registers();
                    assert_eq!(*registers.pc, 17);
                    assert!(matches!(registers.stack_frame[0], Value::Integer(22)));
                    assert!(matches!(registers.stack_frame[1], Value::Integer(33)));
                    drop(registers);
                    assert_eq!(frame.fuel.remaining(), 7);
                    assert!(frame.fuel.is_interrupted());
                });
            }
        });
    }
}
