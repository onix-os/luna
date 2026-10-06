use std::{
    any::Any,
    panic::{catch_unwind, AssertUnwindSafe},
};

#[cfg(not(miri))]
use crate::Closure;
use crate::{thread::activation::ActivationHost, Context};

use super::{abi, helpers};
#[cfg(not(miri))]
use super::{backend, ir::Snapshot, resources::MappingCounter, work, JitError};

pub(super) const SYMBOLS: [(u32, &str, abi::HelperEntry); 9] = [
    (
        abi::HELPER_MOVE,
        "luna_scoped_move_v1",
        call::<{ abi::HELPER_MOVE }>,
    ),
    (
        abi::HELPER_CONSTANT,
        "luna_scoped_constant_v1",
        call::<{ abi::HELPER_CONSTANT }>,
    ),
    (
        abi::HELPER_NEW_TABLE,
        "luna_scoped_new_table_v1",
        call::<{ abi::HELPER_NEW_TABLE }>,
    ),
    (
        abi::HELPER_GET_TABLE,
        "luna_scoped_get_table_v1",
        call::<{ abi::HELPER_GET_TABLE }>,
    ),
    (
        abi::HELPER_SET_TABLE,
        "luna_scoped_set_table_v1",
        call::<{ abi::HELPER_SET_TABLE }>,
    ),
    (
        abi::HELPER_GET_UP_TABLE,
        "luna_scoped_get_up_table_v1",
        call::<{ abi::HELPER_GET_UP_TABLE }>,
    ),
    (
        abi::HELPER_SET_UP_TABLE,
        "luna_scoped_set_up_table_v1",
        call::<{ abi::HELPER_SET_UP_TABLE }>,
    ),
    (
        abi::HELPER_GET_UPVALUE,
        "luna_scoped_get_upvalue_v1",
        call::<{ abi::HELPER_GET_UPVALUE }>,
    ),
    (
        abi::HELPER_SET_UPVALUE,
        "luna_scoped_set_upvalue_v1",
        call::<{ abi::HELPER_SET_UPVALUE }>,
    ),
];

#[cfg(not(miri))]
pub(super) struct Code<'gc> {
    code: backend::Code,
    source: Closure<'gc>,
    origin: MappingCounter,
}

#[cfg(not(miri))]
impl<'gc> Code<'gc> {
    pub(super) fn new(ctx: Context<'gc>, source: Closure<'gc>) -> Result<Self, JitError> {
        let (memory, metadata, snapshots, limit, instructions, limits) = {
            let manager = ctx.jit().0.borrow();
            (
                manager.memory.clone(),
                manager.metadata.clone(),
                manager.snapshots.clone(),
                manager.config.max_code_bytes,
                manager.config.max_prototype_instructions,
                work::Limits::from(&manager.config),
            )
        };
        let snapshot = Snapshot::new_in(&source.prototype(), instructions, snapshots)?;
        let code = backend::compile_scoped_in(&snapshot, memory.clone(), limit, metadata, limits)?;
        Ok(Self {
            code,
            source,
            origin: memory,
        })
    }

    pub(super) fn invoke(
        &self,
        frame: &mut Frame<'gc, '_, '_>,
        slots: &mut [abi::Slot],
        budget: u32,
    ) -> Option<abi::Exit> {
        let manager = frame.ctx.jit().0.borrow();
        if manager.config.mode != super::JitMode::Auto
            || !self.origin.same_root(&manager.memory)
            || frame.panic.is_some()
            || !frame.host.lua_ready()
            || slots.len() != self.code.registers
            || slots.iter().any(|slot| slot.tag > abi::REFERENCE)
        {
            return None;
        }
        drop(manager);
        frame.host.clear_hook(frame.ctx);
        if frame.ctx.hook_enabled() {
            return None;
        }
        let pc = frame.host.with_registers(|closure, registers| {
            (closure == self.source
                && registers.stack_frame.len() >= slots.len()
                && self
                    .code
                    .entries
                    .get(*registers.pc)
                    .copied()
                    .unwrap_or(false))
            .then_some(*registers.pc)
        })?;
        frame.slot_count = slots.len();
        let mut host = abi::Host {
            data: std::ptr::from_mut(frame).cast(),
            projection: std::ptr::null_mut(),
        };
        let exit = unsafe { self.code.invoke_host(slots, pc, budget, &mut host) };
        if frame.panic.is_none() {
            frame
                .host
                .with_registers(|_, registers| *registers.pc = exit.pc as usize);
        }
        Some(exit)
    }
}

pub(super) struct Frame<'gc, 'host, 'borrow> {
    pub ctx: Context<'gc>,
    pub host: &'borrow mut ActivationHost<'gc, 'host>,
    pub count: helpers::Counts,
    pub kinds: u16,
    pub panic: Option<Box<dyn Any + Send>>,
    slot_count: usize,
}

impl<'gc, 'host, 'borrow> Frame<'gc, 'host, 'borrow> {
    pub(super) fn new(ctx: Context<'gc>, host: &'borrow mut ActivationHost<'gc, 'host>) -> Self {
        Self {
            ctx,
            host,
            count: helpers::Counts::default(),
            kinds: 0,
            panic: None,
            slot_count: 0,
        }
    }
}

unsafe extern "C" fn call<const KIND: u32>(
    host: *mut abi::Host,
    slots: *mut abi::Slot,
    a: u32,
    b: u32,
    c: u32,
    pc: u32,
) -> u32 {
    if host.is_null() {
        return abi::HELPER_DECLINED;
    }
    let scoped = unsafe { &mut *(*host).data.cast::<Frame<'_, '_, '_>>() };
    let result = catch_unwind(AssertUnwindSafe(|| {
        scoped.kinds |= 1 << (KIND - 1);
        scoped.host.with_registers(|closure, mut registers| {
            let mut frame = helpers::Frame {
                ctx: scoped.ctx,
                closure,
                registers: &mut registers,
                count: std::mem::take(&mut scoped.count),
                slot_count: scoped.slot_count,
                panic: None,
                projection: None,
            };
            let mut host = abi::Host {
                data: std::ptr::from_mut(&mut frame).cast(),
                projection: std::ptr::null_mut(),
            };
            let result = unsafe { helpers::call::<KIND>(&mut host, slots, a, b, c, pc) };
            scoped.count = frame.count;
            scoped.panic = frame.panic;
            result
        })
    }));
    match result {
        Ok(result) => result,
        Err(payload) => {
            scoped.panic = Some(payload);
            abi::HELPER_PANICKED
        }
    }
}

#[cfg(test)]
mod tests;
