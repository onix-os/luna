use std::{
    any::Any,
    panic::{catch_unwind, AssertUnwindSafe},
};

#[cfg(not(miri))]
use crate::Closure;
use crate::{thread::activation::ActivationHost, Context};

use super::{abi, helpers};

#[repr(C)]
pub(crate) struct RegionView {
    pub data: *mut std::ffi::c_void,
    pub slots: *mut abi::Slot,
    pub pc: u64,
    pub budget: u32,
    pub host: *mut abi::Host,
    pub exit: abi::Exit,
}

pub(super) fn publish_region(
    frame: &mut Frame<'_, '_, '_>,
    slots: &mut [abi::Slot],
    host: &mut abi::Host,
    view: &mut RegionView,
) {
    *host = frame.publish(slots.len());
    view.host = std::ptr::from_mut(host);
    view.slots = slots.as_mut_ptr();
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<RegionView>() == 56);
    assert!(std::mem::offset_of!(RegionView, host) == 32);
    assert!(std::mem::offset_of!(RegionView, exit) == 40);
};
#[cfg(not(miri))]
use super::{backend, ir::Snapshot, resources::MappingCounter, work, JitError};

pub(super) const SYMBOLS: [(u32, &str, abi::HelperEntry); 18] = [
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
        "luna_scoped_get_table_r_v2",
        call::<{ abi::HELPER_GET_TABLE }>,
    ),
    (
        abi::HELPER_GET_TABLE | abi::HELPER_C_CONSTANT,
        "luna_scoped_get_table_c_v2",
        call::<{ abi::HELPER_GET_TABLE | abi::HELPER_C_CONSTANT }>,
    ),
    (
        abi::HELPER_SET_TABLE,
        "luna_scoped_set_table_rr_v2",
        call::<{ abi::HELPER_SET_TABLE }>,
    ),
    (
        abi::HELPER_SET_TABLE | abi::HELPER_C_CONSTANT,
        "luna_scoped_set_table_rc_v2",
        call::<{ abi::HELPER_SET_TABLE | abi::HELPER_C_CONSTANT }>,
    ),
    (
        abi::HELPER_SET_TABLE | abi::HELPER_B_CONSTANT,
        "luna_scoped_set_table_cr_v2",
        call::<{ abi::HELPER_SET_TABLE | abi::HELPER_B_CONSTANT }>,
    ),
    (
        abi::HELPER_SET_TABLE | abi::HELPER_B_CONSTANT | abi::HELPER_C_CONSTANT,
        "luna_scoped_set_table_cc_v2",
        call::<{ abi::HELPER_SET_TABLE | abi::HELPER_B_CONSTANT | abi::HELPER_C_CONSTANT }>,
    ),
    (
        abi::HELPER_GET_UP_TABLE,
        "luna_scoped_get_up_table_r_v2",
        call::<{ abi::HELPER_GET_UP_TABLE }>,
    ),
    (
        abi::HELPER_GET_UP_TABLE | abi::HELPER_C_CONSTANT,
        "luna_scoped_get_up_table_c_v2",
        call::<{ abi::HELPER_GET_UP_TABLE | abi::HELPER_C_CONSTANT }>,
    ),
    (
        abi::HELPER_SET_UP_TABLE,
        "luna_scoped_set_up_table_rr_v2",
        call::<{ abi::HELPER_SET_UP_TABLE }>,
    ),
    (
        abi::HELPER_SET_UP_TABLE | abi::HELPER_C_CONSTANT,
        "luna_scoped_set_up_table_rc_v2",
        call::<{ abi::HELPER_SET_UP_TABLE | abi::HELPER_C_CONSTANT }>,
    ),
    (
        abi::HELPER_SET_UP_TABLE | abi::HELPER_B_CONSTANT,
        "luna_scoped_set_up_table_cr_v2",
        call::<{ abi::HELPER_SET_UP_TABLE | abi::HELPER_B_CONSTANT }>,
    ),
    (
        abi::HELPER_SET_UP_TABLE | abi::HELPER_B_CONSTANT | abi::HELPER_C_CONSTANT,
        "luna_scoped_set_up_table_cc_v2",
        call::<{ abi::HELPER_SET_UP_TABLE | abi::HELPER_B_CONSTANT | abi::HELPER_C_CONSTANT }>,
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
    (
        abi::HELPER_SET_LIST,
        "luna_scoped_set_list_v1",
        call::<{ abi::HELPER_SET_LIST }>,
    ),
];

#[cfg(not(miri))]
pub(super) struct Code {
    code: backend::Code,
    source: u64,
    origin: MappingCounter,
}

#[cfg(not(miri))]
pub(super) struct BoundCode<'code, 'gc> {
    code: &'code Code,
    source: Closure<'gc>,
}

#[cfg(not(miri))]
impl Code {
    #[cfg(test)]
    pub(super) fn new<'gc>(ctx: Context<'gc>, source: Closure<'gc>) -> Result<Self, JitError> {
        let identity = ctx
            .jit_registry()
            .borrow()
            .identity(ctx, source.prototype())
            .ok_or_else(|| JitError::Compilation("unregistered scoped source".into()))?;
        let (memory, metadata, snapshots, config) = {
            let manager = ctx.jit().0.borrow();
            (
                manager.memory.clone(),
                manager.metadata.clone(),
                manager.snapshots.clone(),
                manager.config.clone(),
            )
        };
        let snapshot = Snapshot::new_in(
            &source.prototype(),
            config.max_prototype_instructions,
            snapshots,
        )?;
        Self::compile(&snapshot, identity, memory, metadata, &config)
    }

    pub(super) fn compile(
        snapshot: &Snapshot,
        identity: u64,
        memory: MappingCounter,
        metadata: super::resources::BudgetAllocator,
        config: &super::JitConfig,
    ) -> Result<Self, JitError> {
        let code = backend::compile_scoped_in(
            snapshot,
            memory.clone(),
            config.max_code_bytes,
            metadata,
            work::Limits::from(config),
        )?;
        Ok(Self {
            code,
            source: identity,
            origin: memory,
        })
    }

    pub(super) fn bind<'gc>(
        &self,
        ctx: Context<'gc>,
        source: Closure<'gc>,
    ) -> Option<BoundCode<'_, 'gc>> {
        if !self.origin.same_root(&ctx.jit().0.borrow().memory)
            || ctx
                .jit_registry()
                .borrow()
                .identity(ctx, source.prototype())
                != Some(self.source)
        {
            return None;
        }
        Some(BoundCode { code: self, source })
    }

    pub(super) fn registers(&self) -> usize {
        self.code.registers
    }

    pub(super) fn belongs_to(&self, memory: &MappingCounter) -> bool {
        self.origin.same_root(memory)
    }

    pub(super) fn accepts_pc(&self, pc: usize) -> bool {
        self.code.entries.get(pc).copied().unwrap_or(false)
    }

    pub(super) fn entry(&self) -> abi::Entry {
        self.code.linked_entry()
    }

    pub(super) fn relocations(&self) -> usize {
        self.code.relocations
    }
}

#[cfg(not(miri))]
impl<'gc> BoundCode<'_, 'gc> {
    #[cfg(test)]
    pub(super) fn invoke(
        &self,
        frame: &mut Frame<'gc, '_, '_>,
        slots: &mut [abi::Slot],
        budget: u32,
    ) -> Option<abi::Exit> {
        let pc = self.prepare(frame, slots)?;
        let mut host = frame.publish(slots.len());
        let exit = unsafe { self.code.code.invoke_host(slots, pc, budget, &mut host) };
        if frame.panic.is_none() {
            frame
                .host
                .with_registers(|_, registers| *registers.pc = exit.pc as usize);
        }
        Some(exit)
    }

    pub(super) fn source(&self) -> Closure<'gc> {
        self.source
    }

    pub(super) fn accepts_entry(&self, source: Closure<'gc>, pc: usize) -> bool {
        source == self.source && self.code.accepts_pc(pc)
    }

    pub(super) fn prepare(
        &self,
        frame: &mut Frame<'gc, '_, '_>,
        slots: &[abi::Slot],
    ) -> Option<usize> {
        let manager = frame.ctx.jit().0.borrow();
        if manager.config.mode != super::JitMode::Auto
            || !self.code.origin.same_root(&manager.memory)
            || frame.panic.is_some()
            || !frame.host.lua_ready()
            || slots.len() != self.code.registers()
            || slots.iter().any(|slot| slot.tag > abi::REFERENCE)
        {
            return None;
        }
        drop(manager);
        frame.host.clear_hook(frame.ctx);
        if frame.ctx.hook_enabled() {
            return None;
        }
        frame.host.with_registers(|closure, registers| {
            (registers.stack_frame.len() >= slots.len()
                && self.accepts_entry(closure, *registers.pc))
            .then_some(*registers.pc)
        })
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
    pub(super) fn publish(&mut self, slots: usize) -> abi::Host {
        self.slot_count = slots;
        abi::Host {
            data: std::ptr::from_mut(self).cast(),
            projection: std::ptr::null_mut(),
        }
    }
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
        scoped.kinds |= 1 << (abi::helper_kind(KIND) - 1);
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
