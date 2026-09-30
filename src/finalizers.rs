use ottavino_gc_arena::{lock::RefLock, Collect, Finalization, Gc, GcWeak, Mutation};

use crate::{
    table::TableInner, thread::ThreadInner, userdata::UserDataInner, Table, Thread, UserData, Value,
};

#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct Finalizers<'gc>(Gc<'gc, RefLock<FinalizersState<'gc>>>);

impl<'gc> Finalizers<'gc> {
    const THREAD_ERR: &'static str = "thread finalization was missed";

    pub(crate) fn new(mc: &Mutation<'gc>) -> Self {
        Finalizers(Gc::new(mc, RefLock::default()))
    }

    pub(crate) fn register_thread(&self, mc: &Mutation<'gc>, ptr: Gc<'gc, ThreadInner<'gc>>) {
        self.0.borrow_mut(mc).threads.push(Gc::downgrade(ptr));
    }

    /// Register a userdata whose metatable carries a `__gc` handler.
    ///
    /// Only such userdata are registered, so the registry does not grow for the overwhelming
    /// majority that never needs finalizing. Registration happens when the metatable is attached,
    /// because that is the first moment `__gc` can be known — `UserData::new_static` attaches none.
    /// Register a table whose metatable carries a `__gc` handler.
    pub(crate) fn register_table(&self, mc: &Mutation<'gc>, ptr: Gc<'gc, TableInner<'gc>>) {
        let mut state = self.0.borrow_mut(mc);
        if state
            .registered
            .insert((RegistryKind::Table, Gc::as_ptr(ptr) as *const ()))
        {
            state.finalizable_tables.push(Gc::downgrade(ptr));
        }
    }

    pub(crate) fn register_userdata(&self, mc: &Mutation<'gc>, ptr: Gc<'gc, UserDataInner<'gc>>) {
        let mut state = self.0.borrow_mut(mc);
        // Re-attaching a metatable must not enrol the same object twice, or its handler would run
        // once per registration.
        if state
            .registered
            .insert((RegistryKind::Userdata, Gc::as_ptr(ptr) as *const ()))
        {
            state.finalizable.push(Gc::downgrade(ptr));
        }
    }

    /// First stage of two-stage finalization.
    ///
    /// This stage can cause resurrection, so the arena must be *fully re-marked* before stage two
    /// (`Finalizers::finalize`).
    /// Register a table whose keys are held weakly.
    pub(crate) fn register_weak_keys(&self, mc: &Mutation<'gc>, ptr: Gc<'gc, TableInner<'gc>>) {
        let mut state = self.0.borrow_mut(mc);
        if state
            .registered
            .insert((RegistryKind::WeakKeys, Gc::as_ptr(ptr) as *const ()))
        {
            state.weak_key_tables.push(Gc::downgrade(ptr));
        }
    }

    pub(crate) fn unregister_weak_keys(&self, mc: &Mutation<'gc>, ptr: Gc<'gc, TableInner<'gc>>) {
        let address = Gc::as_ptr(ptr);
        let mut state = self.0.borrow_mut(mc);
        state
            .registered
            .remove(&(RegistryKind::WeakKeys, address as *const ()));
        state
            .weak_key_tables
            .retain(|weak| weak.as_ptr() != address);
    }

    /// One round of ephemeron marking: revive the value of every entry whose key is still alive.
    ///
    /// Returns how many entries have a live key. The caller re-marks and calls again until that
    /// count stops growing, which is the fixed point: reviving a value can make another table's
    /// key reachable, which keeps *that* entry's value, and so on.
    ///
    /// The count is the termination signal rather than "did anything get revived", because a
    /// revival does not necessarily survive the next full mark — asking that question instead
    /// loops forever. Live keys only ever increase, so this always terminates.
    /// Whether any weak-key table exists at all.
    ///
    /// Checked before the fixed-point loop, which costs a full re-mark per round: a program that
    /// never writes `__mode = "k"` must not pay for the machinery.
    pub(crate) fn has_ephemerons(&self) -> bool {
        !self.0.borrow().weak_key_tables.is_empty()
    }

    pub(crate) fn mark_ephemerons(&self, fc: &Finalization<'gc>) -> usize {
        let mut state = self.0.borrow_mut(fc);
        let tables = state.weak_key_tables.clone();
        state.ephemeron_roots.clear();

        let mut live = 0;
        let mut roots = Vec::new();
        for weak in tables {
            let Some(table) = weak.upgrade(fc) else {
                continue;
            };
            live += crate::Table::from_inner(table).revive_live_entries(fc, &mut roots);
        }
        state.ephemeron_roots = roots;
        live
    }

    pub(crate) fn prepare(&self, fc: &Finalization<'gc>) {
        let mut state = self.0.borrow_mut(fc);

        // Marking is over for this cycle, so everything the ephemeron pass revived is already
        // marked and will survive the sweep on its own. Dropping the roots here is what keeps them
        // out of the *next* cycle's first mark: held any longer, a value that refers back to its
        // own key would make that key reachable again, and the entry could never be collected.
        state.ephemeron_roots.clear();

        for &ptr in &state.threads {
            let thread = Thread::from_inner(ptr.upgrade(fc).expect(Self::THREAD_ERR));
            thread.resurrect_live_upvalues(fc).unwrap();
        }

        // A `__gc` handler is Lua, and Lua cannot be called from inside a collection. So a dead
        // userdata with a handler is *resurrected* here — which is what makes it safe to hand to
        // the host afterwards — and queued for the handler to run outside the arena.
        let mut queued = Vec::new();

        // Taken out so the `registered` set can be pruned from inside the same pass. A registry
        // entry and its set entry must go together: the collector can hand a freed address to a new
        // object, and a set entry left behind would refuse to register it.
        let mut finalizable = std::mem::take(&mut state.finalizable);
        finalizable.retain(|&weak| {
            let Some(ptr) = weak.upgrade(fc) else {
                // Already collected in an earlier cycle with nothing to run.
                state
                    .registered
                    .remove(&(RegistryKind::Userdata, weak.as_ptr() as *const ()));
                return false;
            };
            if Gc::is_dead(fc, ptr) {
                queued.push(Value::UserData(UserData::from_inner(ptr)));
                // Dropped from the registry so the handler runs exactly once, as in PUC-Rio, even
                // if the handler resurrects the object.
                state
                    .registered
                    .remove(&(RegistryKind::Userdata, Gc::as_ptr(ptr) as *const ()));
                false
            } else {
                true
            }
        });
        state.finalizable = finalizable;

        let mut finalizable_tables = std::mem::take(&mut state.finalizable_tables);
        finalizable_tables.retain(|&weak| {
            let Some(ptr) = weak.upgrade(fc) else {
                state
                    .registered
                    .remove(&(RegistryKind::Table, weak.as_ptr() as *const ()));
                return false;
            };
            if Gc::is_dead(fc, ptr) {
                queued.push(Value::Table(Table::from_inner(ptr)));
                state
                    .registered
                    .remove(&(RegistryKind::Table, Gc::as_ptr(ptr) as *const ()));
                false
            } else {
                true
            }
        });
        state.finalizable_tables = finalizable_tables;

        // Weak-key tables were never pruned at all, so the ephemeron pass walked every table ever
        // declared `__mode = "k"` for the life of the state.
        let mut weak_key_tables = std::mem::take(&mut state.weak_key_tables);
        weak_key_tables.retain(|&weak| {
            if weak.upgrade(fc).is_some() {
                true
            } else {
                state
                    .registered
                    .remove(&(RegistryKind::WeakKeys, weak.as_ptr() as *const ()));
                false
            }
        });
        state.weak_key_tables = weak_key_tables;

        state.pending.extend(queued);
    }

    /// Second stage of two-stage finalization.
    ///
    /// Assuming stage one was called (`Finalizers::prepare`) and the arena fully re-marked, this
    /// method will *not* cause any resurrection.
    ///
    /// The arena must *immediately* transition to `CollectionPhase::Collecting` afterwards to not
    /// miss any finalizers.
    pub(crate) fn finalize(&self, fc: &Finalization<'gc>) {
        let mut state = self.0.borrow_mut(fc);
        state.threads.retain(|&ptr| {
            let ptr = ptr.upgrade(fc).expect(Self::THREAD_ERR);
            if Gc::is_dead(fc, ptr) {
                Thread::from_inner(ptr).reset(fc).unwrap();
                false
            } else {
                true
            }
        });
    }

    /// Take the userdata whose `__gc` handlers are waiting to run.
    ///
    /// They were resurrected during `prepare`, so they are alive and safe to use; the host runs
    /// their handlers once collection is over.
    pub(crate) fn take_pending(&self, mc: &Mutation<'gc>) -> Vec<Value<'gc>> {
        std::mem::take(&mut self.0.borrow_mut(mc).pending)
    }

    /// Whether any handler is waiting, without taking them.
    pub(crate) fn has_pending(&self) -> bool {
        !self.0.borrow().pending.is_empty()
    }
}

#[derive(Default, Collect)]
#[collect(no_drop)]
struct FinalizersState<'gc> {
    threads: Vec<GcWeak<'gc, ThreadInner<'gc>>>,
    /// Userdata with a `__gc` handler that has not run yet.
    finalizable: Vec<GcWeak<'gc, UserDataInner<'gc>>>,
    /// Tables whose metatable carries `__gc`. Kept apart from userdata so neither path pays for
    /// the other's indirection.
    finalizable_tables: Vec<GcWeak<'gc, TableInner<'gc>>>,
    // Tables declared `__mode = "k"`. Visited during finalization to put back the values of
    // entries whose key survived — the ephemeron step.
    weak_key_tables: Vec<GcWeak<'gc, TableInner<'gc>>>,
    // The values of live-keyed entries, held strongly for the rest of the cycle.
    //
    // Resurrecting a value marks it, but every later `mark_all` re-marks from the roots and a
    // value reachable only through a weak slot goes white again. Rooting it here is what makes it
    // survive to the sweep. Rebuilt each round, so a value whose key has died simply stops being
    // added and is collected on the following cycle.
    ephemeron_roots: Vec<Value<'gc>>,
    /// Resurrected and awaiting their handler.
    pending: Vec<Value<'gc>>,
    // Which objects are already in one of the registries above, so registering is a hash lookup
    // rather than a scan of everything registered so far.
    //
    // Scanning made registration quadratic: 20,000 tables carrying `__gc` cost 82ms to create, and
    // the cost grew with the square of the count. The tag separates the registries, because one
    // table can be both finalizable and weak-keyed.
    //
    // Entries are removed exactly when the matching registry entry is dropped. That matters: an
    // address freed by the collector can be handed straight back to a new object, and a stale entry
    // here would silently refuse to register it.
    #[collect(require_static)]
    registered: std::collections::HashSet<(RegistryKind, *const ())>,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
enum RegistryKind {
    Userdata,
    Table,
    WeakKeys,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_reattachment_preserves_other_registrations_and_allows_reenrollment() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let mode = Table::new(&ctx);
            mode.set_field(ctx, "__mode", "k");
            mode.set_field(
                ctx,
                "__gc",
                crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return)),
            );
            let table = Table::new(&ctx);
            let peer = Table::new(&ctx);
            table.set_metatable(ctx, Some(mode));
            peer.set_metatable(ctx, Some(mode));
            let address = Gc::as_ptr(table.into_inner()) as *const ();
            let peer_address = Gc::as_ptr(peer.into_inner()) as *const ();
            for (name, ephemeron) in [
                ("k", true),
                ("kv", false),
                ("kv", false),
                ("k", true),
                ("k", true),
                ("kv", false),
            ] {
                mode.set_field(ctx, "__mode", name);
                table.set_metatable(ctx, Some(mode));
                let finalizers = ctx.finalizers();
                let state = finalizers.0.borrow();
                assert_eq!(state.finalizable_tables.len(), 2);
                assert_eq!(state.weak_key_tables.len(), 1 + usize::from(ephemeron));
                assert_eq!(state.registered.len(), 3 + usize::from(ephemeron));
                assert!(state.registered.contains(&(RegistryKind::Table, address)));
                assert!(state
                    .registered
                    .contains(&(RegistryKind::Table, peer_address)));
                assert!(state
                    .registered
                    .contains(&(RegistryKind::WeakKeys, peer_address)));
                assert_eq!(
                    state
                        .registered
                        .contains(&(RegistryKind::WeakKeys, address)),
                    ephemeron
                );
                assert_eq!(
                    state
                        .weak_key_tables
                        .iter()
                        .any(|weak| weak.as_ptr() as *const () == address),
                    ephemeron
                );
            }
        });
    }
}
