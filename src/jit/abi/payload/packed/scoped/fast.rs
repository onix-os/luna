use super::*;

pub(super) struct Control<'gc> {
    ctx: crate::Context<'gc>,
    closure: crate::Closure<'gc>,
    counts: NonNull<helpers::Counts>,
    pc: NonNull<usize>,
}

#[path = "fast_tests.rs"]
mod tests;

impl<'gc> Control<'gc> {
    pub(super) fn new(frame: &mut helpers::Frame<'gc, '_, '_, '_>) -> Self {
        Self {
            ctx: frame.ctx,
            closure: frame.closure,
            counts: NonNull::from(&mut frame.count),
            pc: NonNull::from(&mut *frame.registers.pc),
        }
    }

    fn begin(&mut self, pc: u32, outcome: &mut Option<(u32, u32)>) {
        unsafe {
            self.counts.as_mut().calls += 1;
            *self.pc.as_ptr() = pc as usize + 1;
        }
        *outcome = Some((pc, abi::HELPER_PANICKED));
    }

    fn finish<const KIND: u32>(
        &mut self,
        completed: bool,
        pc: u32,
        outcome: &mut Option<(u32, u32)>,
    ) -> u32 {
        let status = if completed {
            abi::HELPER_COMPLETED
        } else {
            abi::HELPER_DECLINED
        };
        *outcome = Some((pc, status));
        unsafe {
            let counts = self.counts.as_mut();
            if completed {
                counts.completed += 1;
                counts.upvalue_reads += u64::from(matches!(
                    KIND,
                    abi::HELPER_GET_UPVALUE | abi::HELPER_GET_UP_TABLE
                ));
                counts.table_reads += u64::from(matches!(
                    KIND,
                    abi::HELPER_GET_TABLE | abi::HELPER_GET_UP_TABLE
                ));
                counts.table_writes += u64::from(KIND == abi::HELPER_SET_TABLE);
                counts.allocations += u64::from(KIND == abi::HELPER_NEW_TABLE);
                *self.pc.as_ptr() = pc as usize + 1;
            } else {
                counts.declined += 1;
                *self.pc.as_ptr() = pc as usize;
            }
        }
        status
    }
}

impl<'gc, const CAPACITY: usize> FrameScope<'_, 'gc, '_, '_, '_, CAPACITY> {
    fn operand(&self, operand: u32) -> Option<Value<'gc>> {
        if operand & abi::CONSTANT_OPERAND != 0 {
            self.control
                .closure
                .prototype()
                .constants
                .get((operand & !abi::CONSTANT_OPERAND) as usize)
                .copied()
                .map(Into::into)
        } else {
            self.scope.read(operand as usize).ok()
        }
    }

    fn store_helper<const KIND: u32>(
        &mut self,
        dest: u32,
        value: Value<'gc>,
        pc: u32,
    ) -> Result<(), Refusal> {
        let control = &mut self.control;
        let result = &mut self.helper_result;
        self.scope.replace_then(dest as usize, value, false, || {
            control.finish::<KIND>(true, pc, result);
        })
    }

    pub(super) fn fast_helper<const KIND: u32>(
        &mut self,
        a: u32,
        b: u32,
        c: u32,
        pc: u32,
    ) -> Result<Option<u32>, Refusal> {
        match KIND {
            abi::HELPER_GET_UPVALUE => self.upvalue_read(a, b, pc),
            abi::HELPER_GET_TABLE | abi::HELPER_GET_UP_TABLE => {
                self.table_read::<KIND>(a, b, c, pc)
            }
            abi::HELPER_SET_TABLE => self.table_write(a, b, c, pc),
            abi::HELPER_NEW_TABLE => {
                let ctx = self.control.ctx;
                self.new_table_with(a, pc, || {
                    crate::Table::from_parts(
                        &ctx,
                        crate::table::RawTable::with_capacity(&ctx, b as usize, c as usize),
                        None,
                    )
                })
            }
            _ => Ok(None),
        }
    }

    fn upvalue_read(&mut self, dest: u32, source: u32, pc: u32) -> Result<Option<u32>, Refusal> {
        use crate::thread::{LuaRegisters, ScopedUpvalue};
        if dest as usize >= self.scope.count {
            return Ok(None);
        }
        let Some(upvalue) = self.control.closure.upvalues().get(source as usize) else {
            return Ok(None);
        };
        let origin = unsafe {
            LuaRegisters::scoped_upvalue(
                std::ptr::addr_of!(*(*self.frame.as_ptr()).registers),
                upvalue.get(),
            )
        };
        let Some(origin) = origin else {
            return Ok(None);
        };
        let origin = match origin {
            ScopedUpvalue::Register(index) => {
                if index >= self.scope.count {
                    return Ok(None);
                }
                ScopedUpvalue::Value(self.scope.read(index)?)
            }
            origin => origin,
        };
        self.control.begin(pc, &mut self.helper_result);
        let value = match origin {
            ScopedUpvalue::Value(value) => value,
            ScopedUpvalue::Foreign(open) => open.get(&self.control.ctx),
            ScopedUpvalue::Register(_) => unreachable!(),
        };
        self.store_helper::<{ abi::HELPER_GET_UPVALUE }>(dest, value, pc)?;
        Ok(Some(abi::HELPER_COMPLETED))
    }

    fn new_table_with(
        &mut self,
        dest: u32,
        pc: u32,
        allocate: impl FnOnce() -> crate::Table<'gc>,
    ) -> Result<Option<u32>, Refusal> {
        if dest as usize >= self.scope.count {
            return Ok(None);
        }
        self.control.begin(pc, &mut self.helper_result);
        let table = allocate();
        self.store_helper::<{ abi::HELPER_NEW_TABLE }>(dest, Value::Table(table), pc)?;
        Ok(Some(abi::HELPER_COMPLETED))
    }

    fn table_write(
        &mut self,
        receiver: u32,
        key: u32,
        value: u32,
        pc: u32,
    ) -> Result<Option<u32>, Refusal> {
        if receiver as usize >= self.scope.count {
            return Ok(None);
        }
        let Some((table, key, value)) = self
            .operand(receiver)
            .zip(self.operand(key))
            .zip(self.operand(value))
            .map(|((t, k), v)| (t, k, v))
        else {
            return Ok(None);
        };
        self.control.begin(pc, &mut self.helper_result);
        let completed = helpers::write_table_value(self.control.ctx, table, key, value);
        Ok(Some(self.control.finish::<{ abi::HELPER_SET_TABLE }>(
            completed,
            pc,
            &mut self.helper_result,
        )))
    }

    fn table_read<const KIND: u32>(
        &mut self,
        dest: u32,
        receiver: u32,
        operand: u32,
        pc: u32,
    ) -> Result<Option<u32>, Refusal> {
        if dest as usize >= self.scope.count || operand & abi::CONSTANT_OPERAND == 0 {
            return Ok(None);
        }
        let table = if KIND == abi::HELPER_GET_UP_TABLE {
            let Some(upvalue) = self.control.closure.upvalues().get(receiver as usize) else {
                return Ok(None);
            };
            let crate::closure::UpValueState::Closed(value) = upvalue.get().get() else {
                return Ok(None);
            };
            value
        } else {
            let Some(value) = self.operand(receiver) else {
                return Ok(None);
            };
            if !matches!(value, Value::Table(_)) {
                return Ok(None);
            }
            value
        };
        let Some(key) = self.operand(operand) else {
            return Ok(None);
        };
        self.control.begin(pc, &mut self.helper_result);
        let mut stored = Ok(());
        let completed = helpers::with_table_value(self.control.ctx, table, key, |value| {
            stored = self.store_helper::<KIND>(dest, value, pc);
        });
        stored?;
        Ok(Some(if completed {
            abi::HELPER_COMPLETED
        } else {
            self.control
                .finish::<KIND>(false, pc, &mut self.helper_result)
        }))
    }
}
