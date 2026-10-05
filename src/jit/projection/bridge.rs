use std::ffi::c_void;

use super::{native::Session, *};

pub(crate) trait Bridge<'gc> {
    fn view_pointer(&self) -> *mut c_void;
    fn flush(
        &mut self,
        ctx: Context<'gc>,
        registers: &mut LuaRegisters<'gc, '_>,
    ) -> Result<(), Error>;
    fn refresh(
        &mut self,
        registers: &LuaRegisters<'gc, '_>,
        upvalues: &[Lock<UpValue<'gc>>],
    ) -> Result<(), Error>;
    fn counts(&self) -> Counts;
}

impl<'gc, const CAPACITY: usize> Bridge<'gc> for Session<'_, 'gc, CAPACITY> {
    fn view_pointer(&self) -> *mut c_void {
        self.view_pointer()
    }
    fn flush(
        &mut self,
        ctx: Context<'gc>,
        registers: &mut LuaRegisters<'gc, '_>,
    ) -> Result<(), Error> {
        self.flush(ctx, registers)
    }
    fn refresh(
        &mut self,
        registers: &LuaRegisters<'gc, '_>,
        upvalues: &[Lock<UpValue<'gc>>],
    ) -> Result<(), Error> {
        self.refresh_closure(registers, upvalues)
    }
    fn counts(&self) -> Counts {
        self.counts()
    }
}

fn invoke<'gc, R, const CAPACITY: usize>(
    registers: &mut LuaRegisters<'gc, '_>,
    upvalues: &[Lock<UpValue<'gc>>],
    scratch: &mut [Slot],
    call: impl FnOnce(&mut LuaRegisters<'gc, '_>, *mut Slot, Option<&mut dyn Bridge<'gc>>) -> R,
) -> R {
    match Projection::<CAPACITY>::from_closure(registers, upvalues, scratch) {
        Ok(mut projection) => projection
            .with_native(scratch, |session| {
                call(registers, session.slot_pointer(), Some(session))
            })
            .expect("projection scratch prefix changed"),
        Err(_) => call(registers, scratch.as_mut_ptr(), None),
    }
}

pub(crate) fn with_frame<'gc, R>(
    registers: &mut LuaRegisters<'gc, '_>,
    upvalues: &[Lock<UpValue<'gc>>],
    scratch: &mut [Slot],
    call: impl FnOnce(&mut LuaRegisters<'gc, '_>, *mut Slot, Option<&mut dyn Bridge<'gc>>) -> R,
) -> R {
    match upvalues.len() {
        0 => invoke::<_, 0>(registers, upvalues, scratch, call),
        1 => invoke::<_, 1>(registers, upvalues, scratch, call),
        2 => invoke::<_, 2>(registers, upvalues, scratch, call),
        3..=4 => invoke::<_, 4>(registers, upvalues, scratch, call),
        5..=8 => invoke::<_, 8>(registers, upvalues, scratch, call),
        9..=16 => invoke::<_, 16>(registers, upvalues, scratch, call),
        17..=32 => invoke::<_, 32>(registers, upvalues, scratch, call),
        33..=64 => invoke::<_, 64>(registers, upvalues, scratch, call),
        65..=128 => invoke::<_, 128>(registers, upvalues, scratch, call),
        129..=256 => invoke::<_, 256>(registers, upvalues, scratch, call),
        _ => call(registers, scratch.as_mut_ptr(), None),
    }
}
