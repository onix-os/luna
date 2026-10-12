use super::*;
use crate::{
    jit::{ir::Snapshot, projection::Origin},
    thread::activation::{atomic_call::Spec, ActivationHost},
    Context, Function as LuaFunction, FunctionPrototype, JitMode, Value as LuaValue,
};
use ottavino_gc_arena::Gc;

struct Binding<'gc> {
    descriptor: super::super::Descriptor,
    caller: Gc<'gc, FunctionPrototype<'gc>>,
    callee: Gc<'gc, FunctionPrototype<'gc>>,
}

#[derive(Debug, PartialEq, Eq)]
struct Progress {
    calls: u32,
    slices: u32,
    instructions: u32,
    pending: u32,
}

fn matches(snapshot: &Snapshot, prototype: &FunctionPrototype<'_>) -> bool {
    snapshot.registers == usize::from(prototype.stack_size)
        && snapshot.upvalues == prototype.upvalues.len()
        && snapshot.prototypes == prototype.prototypes.len()
        && snapshot.operations.len() == prototype.opcodes.len()
        && snapshot.constants.len() == prototype.constants.len()
        && snapshot
            .operations
            .iter()
            .copied()
            .eq(prototype.opcodes.iter().map(|op| op.decode()))
        && snapshot
            .constants
            .iter()
            .zip(prototype.constants.iter().copied())
            .all(|(slot, constant)| {
                let expected = abi::Slot::from_constant(constant);
                (slot.tag, slot.bits) == (expected.tag, expected.bits)
            })
}

impl<'gc> Binding<'gc> {
    fn new(
        plan: &Plan<'_>,
        caller: Gc<'gc, FunctionPrototype<'gc>>,
        callee: Gc<'gc, FunctionPrototype<'gc>>,
    ) -> Option<Self> {
        let descriptor = plan.descriptor().ok()?;
        if !matches(plan.caller, &caller)
            || !matches(plan.callee, &callee)
            || plan.pair.arguments > callee.fixed_params
        {
            return None;
        }
        Some(Self {
            descriptor,
            caller,
            callee,
        })
    }

    fn run(
        &self,
        ctx: Context<'gc>,
        host: &mut ActivationHost<'gc, '_>,
        limit: usize,
        budget: u32,
        prefix: u32,
        invoke: impl FnOnce(&mut State),
    ) -> Option<Progress> {
        let plan = self.descriptor;
        let between = u32::try_from(plan.end - plan.start).ok()?;
        if !(4..=64).contains(&budget)
            || prefix >= budget
            || between >= budget
            || limit < 2
            || !host.lua_ready()
        {
            return None;
        }
        host.clear_hook(ctx);
        if ctx.hook_enabled() || ctx.jit().0.borrow().config.mode != JitMode::Auto {
            return None;
        }
        let maximum = (limit / 2).min((59 / (between + 5) + 1) as usize) as u32;
        let work = 5 + (maximum - 1) * (between + 5);
        let width = plan.registers;
        let mut roots = [LuaValue::Nil; MAX_REGISTERS];
        let mut state = State {
            slots: [abi::Slot {
                tag: abi::NIL,
                bits: 0,
            }; MAX_REGISTERS],
            upper: abi::Slot {
                tag: abi::NIL,
                bits: 0,
            },
            root: u64::from(plan.function.0),
            pc: plan.call as u64,
            fuel: host.fuel().remaining(),
            interrupted: u32::from(host.fuel().is_interrupted()),
            budget: work,
            instructions: 0,
            calls: 0,
            reason: INVALID,
            capture: MAX_REGISTERS as u32,
        };
        let (caller, spec) = host.with_registers(|caller, registers| {
            if !Gc::ptr_eq(caller.prototype(), self.caller) || *registers.pc != plan.call
                || registers.stack_frame.len() != width
            {
                return None;
            }
            let LuaValue::Function(LuaFunction::Closure(callee)) = registers.stack_frame[usize::from(plan.function.0)] else {
                return None;
            };
            if !Gc::ptr_eq(callee.prototype(), self.callee) {
                return None;
            }
            let origin = registers.projection_origin(callee.upvalues().get(usize::from(plan.pattern.upvalue))?.get())?;
            let (capture, captured) = match origin {
                Origin::Register(index, LuaValue::Integer(value)) => ((false, index), value),
                Origin::Upper(index, LuaValue::Integer(value)) => ((true, index), value),
                _ => return None,
            };
            if !abi::roots::capture(&mut roots[..width], &mut state.slots[..width], registers.stack_frame) {
                return None;
            }
            for slot in &mut state.slots[..width] {
                if slot.tag == abi::REFERENCE && matches!(roots[slot.bits as usize], LuaValue::Function(LuaFunction::Closure(value)) if value == callee) {
                    slot.bits = state.root;
                }
            }
            state.upper = abi::Slot::from_value(LuaValue::Integer(captured));
            state.capture = if capture.0 { MAX_REGISTERS as u32 } else { u32::try_from(capture.1).ok()? };
            Some((caller, Spec {
                callee,
                function: plan.function,
                arguments: plan.arguments,
                capture,
                read: plan.pattern.read,
                result: plan.pattern.result,
                start: plan.return_start,
                prefix,
            }))
        })?;
        let window = host.snapshot_window(&spec, caller, width, plan.call)?;
        state.fuel -= prefix as i32;
        let initial_fuel = state.fuel;
        let capture = state.capture;
        invoke(&mut state);
        if state.calls == 0 {
            return None;
        }
        let completed = state
            .calls
            .checked_mul(5)?
            .checked_add((state.calls - 1).checked_mul(between)?)?;
        let pending = state.instructions.checked_sub(completed)?;
        let expected = if state.pc == plan.end as u64 {
            0
        } else if state.pc == (plan.end + 1) as u64 {
            1
        } else if (plan.start as u64..=plan.call as u64).contains(&state.pc) {
            u32::try_from(1 + (state.pc as usize - plan.start)).ok()?
        } else {
            return None;
        };
        let consumed = state
            .calls
            .checked_mul(19 + u32::from(plan.arguments))?
            .checked_add((state.calls - 1).checked_mul(between)?)?
            .checked_add(pending)?;
        if state.calls > maximum
            || pending != expected
            || pending > between
            || state.instructions > work
            || state.budget != work - state.instructions
            || state.fuel != initial_fuel.checked_sub(i32::try_from(consumed).ok()?)?
            || state.reason > FUEL
            || state.interrupted != 0
            || state.root != u64::from(plan.function.0)
            || state.capture != capture
        {
            return None;
        }
        let mut values = [LuaValue::Nil; MAX_REGISTERS];
        let mut checked = state.slots;
        let snapshot = abi::roots::call::Snapshot::new(&roots[..width], &mut checked[..width])?;
        if !snapshot.publish(&mut values[..width]) {
            return None;
        }
        let fuel = state.fuel.checked_add(pending as i32)?;
        let captured = state.upper;
        if captured.tag != abi::INTEGER
            || !window.publish_region(
                &values[..width],
                state.pc as usize,
                fuel,
                captured.bits as i64,
            )
        {
            return None;
        }
        Some(Progress {
            calls: state.calls,
            slices: 2 * state.calls,
            instructions: prefix + state.instructions - 2 * state.calls,
            pending,
        })
    }
}

mod tests;
