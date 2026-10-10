use super::*;

impl Session<'_, '_, '_, '_> {
    pub(super) fn complete_rooted_deferred(&mut self, view: &mut View) -> RootedCompletion {
        if self.frame.panic.is_some()
            || view.exit.instructions >= self.budget
            || self.limit - self.outcome.slices < 2
            || view.exit.pc as usize != self.region.pair.program.key().pc
            || !self.frame.host.pairing_enabled(self.frame.ctx)
        {
            return RootedCompletion::Unavailable;
        }
        let Some(roots) = self.roots.as_deref() else {
            return RootedCompletion::Unavailable;
        };
        let Some(mut snapshot) = abi::roots::call::Snapshot::new(roots, self.slots) else {
            return RootedCompletion::Unavailable;
        };
        let ready = self.frame.host.with_registers(|caller, registers| {
            assert_eq!(caller, self.admitted.caller());
            if registers.stack_frame.len() < snapshot.len() {
                return false;
            }
            *registers.pc = view.exit.pc as usize;
            true
        });
        if !ready {
            return RootedCompletion::Unavailable;
        }
        self.rooted_pending = true;
        record(
            self.frame.ctx,
            &view.exit,
            std::mem::take(&mut self.frame.count),
        );
        #[cfg(test)]
        tests::rooted_checkpoint(1);
        let completed = self.admitted.invoke_rooted(
            self.frame.host,
            self.budget,
            view.exit.instructions,
            &mut snapshot,
        );
        #[cfg(test)]
        if completed {
            tests::ROOTED_COMPLETIONS.with(|count| count.set(count.get() + 1));
            tests::rooted_checkpoint(2);
        }
        if completed {
            self.outcome.slices += 2;
            self.outcome.pairs += 1;
        }
        let continuing = completed
            && self.outcome.slices < self.limit
            && self.frame.host.fuel().should_continue()
            && self.frame.host.lua_ready()
            && self.frame.host.frame_identity() == self.identity;
        let pc = self.frame.host.with_registers(|caller, registers| {
            let pc = (continuing
                && caller == self.admitted.caller()
                && registers.stack_frame.len() >= snapshot.len()
                && self.region.caller.accepts_pc(*registers.pc))
            .then_some(*registers.pc);
            if pc.is_none() {
                assert!(snapshot.publish(registers.stack_frame));
            }
            pc
        });
        self.rooted_pending = pc.is_some();
        if !completed {
            return RootedCompletion::Declined;
        }
        let Some(pc) = pc else {
            return RootedCompletion::Complete(0);
        };
        #[cfg(test)]
        tests::ROOTED_DEFERRED.with(|count| count.set(count.get() + 1));
        view.pc = pc as u64;
        self.publish(view);
        RootedCompletion::Complete(1)
    }
}
