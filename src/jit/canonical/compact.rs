use super::*;

pub(super) struct CompactSession<'gc, 'host, 'borrow> {
    ctx: Context<'gc>,
    host: &'borrow mut ActivationHost<'gc, 'host>,
    site: &'borrow Site,
    callee: Option<Gc<'gc, FunctionPrototype<'gc>>>,
    pub(super) calls: usize,
    pub(super) returns: usize,
    pub(super) error: Option<crate::thread::VMError>,
    pub(super) panic: Option<Box<dyn Any + Send>>,
    prefix: u32,
    #[cfg(test)]
    pub(super) exit: Exit,
    #[cfg(test)]
    pub(super) target: Option<(bool, usize)>,
}

impl<'gc, 'host, 'borrow> CompactSession<'gc, 'host, 'borrow> {
    pub(super) fn new(
        ctx: Context<'gc>,
        host: &'borrow mut ActivationHost<'gc, 'host>,
        site: &'borrow Site,
        callee: Option<Gc<'gc, FunctionPrototype<'gc>>>,
        prefix: u32,
    ) -> Self {
        Self {
            ctx,
            host,
            site,
            callee,
            prefix,
            calls: 0,
            returns: 0,
            error: None,
            panic: None,
            #[cfg(test)]
            exit: Exit::default(),
            #[cfg(test)]
            target: None,
        }
    }

    pub(super) fn invoke(&mut self, code: &CallCode, budget: u32) {
        let (pc, function, arguments, start) = code.operands();
        let result = catch_unwind(AssertUnwindSafe(|| {
            if self.calls != 0
                || (pc, function, arguments)
                    != (
                        self.site.pc as u64,
                        self.site.function.0,
                        self.site.arguments,
                    )
            {
                return;
            }
            self.calls = 1;
            match physical_call(self.ctx, self.host, self.site, self.prefix) {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    self.error = Some(error);
                    return;
                }
            }
            if budget <= 3 {
                self.fallback(code, budget, start);
                return;
            }
            let ctx = self.ctx;
            let site = self.site;
            let callee = self.callee;
            let prepared = self.host.with_registers(|closure, registers| {
                if *registers.pc != 0
                    || !matches_callee(ctx, site, callee, closure)
                    || registers.stack_frame.len() < site.registers
                {
                    return None;
                }
                let upvalue = closure
                    .upvalues()
                    .get(usize::from(site.pattern.upvalue))?
                    .get();
                let (target, value) = match registers.projection_origin(upvalue)? {
                    Origin::Upper(index, Value::Integer(value)) => ((true, index), value),
                    Origin::Register(index, Value::Integer(value)) if index < site.registers => {
                        ((false, index), value)
                    }
                    _ => return None,
                };
                let frame = code.compact().prepare(
                    registers.stack_frame,
                    (!target.0).then_some(target.1),
                    value,
                )?;
                Some((target, frame))
            });
            let Some((target, mut frame)) = prepared else {
                self.fallback(code, budget, start);
                return;
            };
            #[cfg(test)]
            {
                self.target = Some(target);
            }
            let output = code
                .compact()
                .invoke(&mut frame)
                .expect("compact source binding");
            let exit = Exit {
                pc: 3,
                instructions: 3,
                reason: Kind::Interpreter as u32,
            };
            #[cfg(test)]
            {
                self.exit = Exit {
                    pc: exit.pc,
                    instructions: exit.instructions,
                    reason: exit.reason,
                };
            }
            assert_eq!((start, self.calls, self.returns), (site.start.0, 1, 0));
            let (upper, index) = target;
            self.host.with_registers(|closure, mut registers| {
                assert!(matches_callee(ctx, site, callee, closure));
                assert_eq!(*registers.pc, 0);
                assert!(registers.stack_frame.len() >= site.registers);
                assert!(registers.projection_read(upper, index).is_some());
                registers.stack_frame[usize::from(site.pattern.read.0)] =
                    Value::Integer(output.read);
                registers.stack_frame[usize::from(site.pattern.result.0)] =
                    Value::Integer(output.result);
                registers.projection_write(upper, index, Value::Integer(output.capture));
                *registers.pc = 3;
            });
            {
                let mut manager = ctx.jit().0.borrow_mut();
                manager.stats.native_upvalue_reads =
                    manager.stats.native_upvalue_reads.saturating_add(1);
                manager.stats.native_upvalue_writes =
                    manager.stats.native_upvalue_writes.saturating_add(1);
                manager.stats.record_native_exit(&exit);
            }
            self.returns = 1;
            let result = self.host.return_fixed(ctx, site.start, site.returns, 3);
            let mut stats = ctx.jit().interpreter_stats();
            stats.dispatches = 1;
            stats.reported_instructions = result.as_ref().ok().map(|_| 0);
            drop(stats);
            if let Err(error) = result {
                self.error = Some(error);
            }
        }));
        if let Err(payload) = result {
            self.panic = Some(payload);
        }
    }

    #[inline(never)]
    fn fallback(&mut self, code: &CallCode, budget: u32, start: u8) {
        let mut slots = [MaybeUninit::uninit(); 256];
        let mut session = Session::new(self.ctx, self.host, self.site, &mut slots);
        session.callee = self.callee;
        session.calls = self.calls;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let frame = session.prepare_leaf();
            if !frame.is_null() {
                unsafe { code.invoke_leaf(frame, budget) };
                session.leave(frame, 3, u32::from(start));
            }
        }));
        if let Err(payload) = result {
            session.panic = Some(payload);
        }
        self.returns = session.returns;
        self.error = session.error.take();
        self.panic = session.panic.take();
        #[cfg(test)]
        {
            self.exit = std::mem::take(&mut session.frame.exit);
            self.target = session.target;
        }
    }

    pub(super) fn finish(mut self) -> crate::jit::PairOutcome {
        let payload = self.panic.take();
        let outcome = crate::jit::PairOutcome {
            calls: self.calls,
            returns: self.returns,
            result: self.error.take().map_or(Ok(()), Err),
        };
        drop(self);
        if let Some(payload) = payload {
            std::panic::resume_unwind(payload);
        }
        outcome
    }
}
