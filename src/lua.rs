use std::cell::Cell;
use std::ops;

use ottavino_gc_arena::{
    arena::{CollectionPhase, Root},
    metrics::Metrics,
    Arena, Collect, Gc, Mutation, Rootable,
};

/// How deep Lua may recurse before the call raises a catchable error.
///
/// A stackless VM does not recurse on the Rust stack, so this is a memory bound rather than the
/// ~200 C levels PUC-Rio can manage: deep recursion is a feature worth keeping. It exists so that
/// an accidentally unbounded recursion in a user's script fails with an error a host can catch
/// instead of running until the machine is out of memory.
pub const DEFAULT_MAX_CALL_DEPTH: usize = 100_000;

/// What a `collectgarbage` call is waiting for the host to do.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GcRequest {
    None,
    Collect,
    Stop,
    Restart,
    Step,
}

use crate::{
    finalizers::Finalizers,
    stash::{Fetchable, Stashable},
    stdlib::{
        load_base, load_coroutine, load_debug, load_io, load_math, load_os, load_package,
        load_string, load_table, load_utf8,
    },
    string::InternedStringSet,
    thread::BadThreadMode,
    Error, ExternError, FromMultiValue, FromValue, Fuel, IntoValue, Registry, RuntimeError,
    Singleton, StashedExecutor, String, Table, TypeError, Value,
};

/// A value representing the main "execution context" of a Lua state.
///
/// It provides access to the table of global variables, the registry, the string interner, and
/// other state that most every piece of running Lua code will need access to.
///
/// It is a cheap, copyable reference type that references internal state variables inside a [`Lua`]
/// instance.
///
/// As a convenience, it also contains the [`ottavino_gc_arena::Mutation`] reference provided by `gc-arena`
/// when mutating a [`ottavino_gc_arena::Arena`]. This allows code that uses luna to accept a single `ctx:
/// Context<'gc>` parameter, rather than having to accept both the luna `ctx` *and* the usual
/// `mc: &Mutation<'gc>` parameter.
///
/// To access the contained [`Mutation`] context, there is a `Deref` impl on `Context` that derefs
/// to `Mutation` that can be used like so:
///
/// ```
/// # use ottavino_gc_arena::Gc;
/// # use luna::Lua;
/// # let mut lua = Lua::empty();
/// lua.enter(|ctx| {
///     // Create a new `Gc<'gc, i32>` pointer using the `&Mutation` held inside `ctx`
///     let p = Gc::new(&ctx, 13);
/// });
/// ```
#[derive(Copy, Clone)]
pub struct Context<'gc> {
    mutation: &'gc Mutation<'gc>,
    state: &'gc State<'gc>,
}

impl<'gc> Context<'gc> {
    /// Get a reference to [`Mutation`] (the `gc-arena` mutation handle) out of the `Context`
    /// object.
    ///
    /// This can also be done automatically with `Deref` coercion.
    pub fn mutation(self) -> &'gc Mutation<'gc> {
        self.mutation
    }

    pub fn globals(self) -> Table<'gc> {
        self.state.globals
    }

    pub fn registry(self) -> Registry<'gc> {
        self.state.registry
    }

    /// How deep a call chain may get before a call raises an error.
    ///
    /// Threads read this when they are created, so changing it does not affect coroutines that
    /// already exist.
    pub fn max_call_depth(self) -> usize {
        self.state.max_call_depth.get()
    }

    pub fn set_max_call_depth(self, depth: usize) {
        self.state.max_call_depth.set(depth);
    }

    /// Ask the host to act on the collector before the next slice.
    pub fn request_gc(self, request: GcRequest) {
        self.state.gc_request.set(request);
    }

    pub fn interned_strings(self) -> InternedStringSet<'gc> {
        self.state.strings
    }

    pub fn finalizers(self) -> Finalizers<'gc> {
        self.state.finalizers
    }

    // Calls `ctx.globals().get(key)`
    pub fn get_global<V: FromValue<'gc>>(self, key: &'static str) -> Result<V, TypeError> {
        self.state.globals.get(self, key)
    }

    // Calls `ctx.globals().get_value(key)`
    pub fn get_global_value(self, key: &'static str) -> Value<'gc> {
        self.state.globals.get_value(self, key)
    }

    // Calls `ctx.globals().set_field(key, value)`
    pub fn set_global<V: IntoValue<'gc>>(self, key: &'static str, value: V) -> Value<'gc> {
        self.state.globals.set_field(self, key, value)
    }

    /// Calls `ctx.registry().singleton::<S>(ctx)`.
    pub fn singleton<S>(self) -> &'gc Root<'gc, S>
    where
        S: for<'a> Rootable<'a> + 'static,
        Root<'gc, S>: Sized + Singleton<'gc> + Collect,
    {
        self.state.registry.singleton::<S>(self)
    }

    /// Calls `ctx.registry().stash(ctx, s)`.
    pub fn stash<S: Stashable<'gc>>(self, s: S) -> S::Stashed {
        self.state.registry.stash(&self, s)
    }

    /// Calls `ctx.registry().fetch(f)`.
    pub fn fetch<F: Fetchable>(self, f: &F) -> F::Fetched<'gc> {
        self.state.registry.fetch(f)
    }

    /// Calls `ctx.interned_strings().intern(&ctx, s)`.
    pub fn intern(self, s: &[u8]) -> String<'gc> {
        self.state.strings.intern(&self, s)
    }

    /// Calls `ctx.interned_strings().intern_static(&ctx, s)`.
    pub fn intern_static(self, s: &'static [u8]) -> String<'gc> {
        self.state.strings.intern_static(&self, s)
    }
}

impl<'gc> ops::Deref for Context<'gc> {
    type Target = Mutation<'gc>;

    fn deref(&self) -> &Self::Target {
        self.mutation
    }
}

/// A Lua execution environment.
///
/// This is the top-level `luna` type. In order to load and call any Lua code, the first step is
/// to create a `Lua` instance.
pub struct Lua {
    arena: Arena<Rootable![State<'_>]>,
    // Host-side configuration rather than collected state: nothing in the arena reads it.
    memory_limit: Option<usize>,
    // Whether `enter` collects on its own, or the host has taken the schedule over.
    gc_automatic: bool,
}

impl Default for Lua {
    fn default() -> Self {
        Lua::core()
    }
}

impl Lua {
    /// Create a new `Lua` instance with no parts of the stdlib loaded.
    pub fn empty() -> Self {
        Lua {
            arena: Arena::<Rootable![State<'_>]>::new(|mc| State::new(mc)),
            memory_limit: None,
            gc_automatic: true,
        }
    }

    /// Create a new `Lua` instance with the core stdlib loaded.
    pub fn core() -> Self {
        let mut lua = Self::empty();
        lua.load_core();
        lua
    }

    /// Create a new `Lua` instance with all of the stdlib loaded.
    pub fn full() -> Self {
        let mut lua = Lua::core();
        lua.load_io();
        lua.load_os();
        lua.load_package();
        lua.load_debug();
        lua
    }

    /// Load the core parts of the stdlib that do not allow performing any I/O.
    ///
    /// Calls:
    ///   - `load_base`
    ///   - `load_coroutine`
    ///   - `load_math`
    ///   - `load_string`
    ///   - `load_table`
    pub fn load_core(&mut self) {
        self.enter(|ctx| {
            load_base(ctx);
            load_coroutine(ctx);
            load_math(ctx);
            load_string(ctx);
            load_table(ctx);
            load_utf8(ctx);
        })
    }

    /// Load the parts of the stdlib that allow I/O.
    pub fn load_io(&mut self) {
        self.enter(|ctx| {
            load_io(ctx);
        })
    }

    /// Load the `os` library: time, dates, the environment, and file removal and renaming.
    ///
    /// Separate from [`Lua::load_core`] because it reaches outside the sandbox — a script with
    /// `os` can read the environment and delete files.
    pub fn load_os(&mut self) {
        self.enter(|ctx| {
            load_os(ctx);
        })
    }

    /// Load the `debug` library.
    ///
    /// Separate from the core, like `os`: it hands scripts introspection over the running program.
    /// `sethook`, `getlocal` and `setlocal` are absent — see the module docs for why.
    pub fn load_debug(&mut self) {
        self.enter(|ctx| {
            load_debug(ctx);
        })
    }

    /// Load the `package` library and `require`.
    ///
    /// There is no C loader: `package.cpath` and `package.loadlib` do not exist.
    pub fn load_package(&mut self) {
        self.enter(|ctx| {
            load_package(ctx);
        })
    }

    /// Size of all memory used by this Lua context.
    ///
    /// This is equivalent to `self.gc_metrics().total_allocation()`. This counts all `Gc` allocated
    /// memory and also all data Lua datastructures held inside `Gc`, as they are tracked as
    /// "external allocations" in `gc-arena`.
    pub fn total_memory(&self) -> usize {
        self.gc_metrics().total_allocation()
    }

    /// Finish the current collection cycle completely, calls `ottavino_gc_arena::Arena::collect_all()`.
    pub fn gc_collect(&mut self) {
        if self.arena.collection_phase() != CollectionPhase::Sweeping {
            self.arena.mark_all().unwrap().finalize(|fc, root| {
                root.finalizers.prepare(fc);
            });
            self.arena.mark_all().unwrap().finalize(|fc, root| {
                root.finalizers.finalize(fc);
            });
        }

        self.arena.collect_all();
        assert!(self.arena.collection_phase() == CollectionPhase::Sleeping);
    }

    /// Stop the collector pacing itself.
    ///
    /// This suspends only the *automatic* schedule. Debt keeps accruing, so an explicit
    /// [`Lua::gc_step`] or [`Lua::gc_collect`] still has work to do — which is the point, and did
    /// not used to be true. Stopping was previously implemented by driving the arena's sleep
    /// threshold to `usize::MAX`, which meant nothing was ever owed and a manual step silently did
    /// nothing either. Suspending the schedule and leaving the debt alone is the honest version.
    pub fn gc_stop(&mut self) {
        self.gc_automatic = false;
    }

    /// Resume automatic pacing after [`Lua::gc_stop`].
    pub fn gc_restart(&mut self) {
        self.gc_automatic = true;
    }

    /// Whether the collector is pacing itself.
    ///
    /// The same question as [`Lua::gc_is_automatic`]; both spellings exist because Lua asks it as
    /// `collectgarbage("isrunning")` and Rust asks it about the schedule.
    pub fn gc_is_running(&self) -> bool {
        self.gc_automatic
    }

    pub fn gc_metrics(&self) -> &Metrics {
        self.arena.metrics()
    }

    /// Enter the garbage collection arena and perform some operation.
    ///
    /// In order to interact with Lua or do any useful work with Lua values, you must do so from
    /// *within* the garbage collection arena. All values branded with the `'gc` branding lifetime
    /// must forever live *inside* the arena, and cannot escape it.
    ///
    /// Garbage collection takes place *in-between* calls to `Lua::enter`, no garbage will be
    /// collected concurrently with accessing the arena.
    ///
    /// Runs one incremental slice of collection before returning, unless the host has taken the
    /// schedule over with [`Lua::set_gc_pacing`].
    pub fn enter<F, T>(&mut self, f: F) -> T
    where
        F: for<'gc> FnOnce(Context<'gc>) -> T,
    {
        let r = self.arena.mutate(move |mc, state| f(state.ctx(mc)));

        // Carry out whatever `collectgarbage` asked for while it was running. It could not do this
        // itself: acting on the collector needs `&mut Lua`, and a callback only has a `Context`.
        let request = self.arena.mutate(|_, state| {
            let request = state.gc_request.get();
            state.gc_request.set(GcRequest::None);
            request
        });
        match request {
            GcRequest::None => {}
            GcRequest::Collect => self.gc_collect(),
            GcRequest::Step => self.gc_step(None),
            GcRequest::Stop => self.gc_stop(),
            GcRequest::Restart => self.gc_restart(),
        }

        // Collection is a *separate* job from running the mutator, and only happens here when the
        // host has not taken it over. A host that calls `gc_step` itself owns the schedule; see
        // `Lua::set_gc_pacing`.
        if self.gc_automatic {
            self.gc_step(Some(Self::AUTOMATIC_GRANULARITY));
        }

        r
    }

    /// How much debt must accumulate before an automatic step does any work.
    const AUTOMATIC_GRANULARITY: f64 = 1024.0;

    /// Run one slice of collection.
    ///
    /// `threshold` is the allocation debt below which the slice does nothing — that is the
    /// automatic schedule, where collection is paid for in proportion to what was allocated.
    ///
    /// `None` means *make progress now*, and is what an explicit call should mean. It matters
    /// because the arena's incremental work is debt-driven: a host that has stopped the collector
    /// is not allocating, so no debt accrues, and a purely debt-driven step would stall halfway
    /// through a cycle forever. With `None` the phase is advanced directly instead.
    ///
    /// This is the whole of luna's collector schedule. The VM hands control back between
    /// `Executor::step` calls and the host decides what to spend here, which is what makes fuel
    /// metering and the memory ceiling possible — a collector running on its own schedule could
    /// not be paced this way.
    pub fn gc_step(&mut self, threshold: Option<f64>) {
        let forced = threshold.is_none();
        if let Some(threshold) = threshold {
            if self.arena.metrics().allocation_debt() <= threshold {
                return;
            }
        }

        if self.arena.collection_phase() == CollectionPhase::Sweeping {
            if forced {
                self.arena.collect_all();
            } else {
                self.arena.collect_debt();
            }
            return;
        }

        let marked = if forced {
            Some(self.arena.mark_all().unwrap())
        } else {
            self.arena.mark_debt()
        };

        if let Some(marked) = marked {
            marked.finalize(|fc, root| {
                root.finalizers.prepare(fc);
            });
            self.arena.mark_all().unwrap().finalize(|fc, root| {
                root.finalizers.finalize(fc);
            });
            // Immediately transition to `CollectionPhase::Sweeping`.
            self.arena.mark_all().unwrap().start_sweeping();
        }
    }

    /// Whether `enter` collects on its own.
    ///
    /// True unless the host has taken the schedule over with [`Lua::set_gc_pacing`].
    pub fn gc_is_automatic(&self) -> bool {
        self.gc_automatic
    }

    /// Decide whether the collector paces itself, or the host does.
    ///
    /// With `false`, `enter` stops collecting and nothing is reclaimed until [`Lua::gc_step`] or
    /// [`Lua::gc_collect`] is called. That is the point: a game can spend a frame budget on it, a
    /// shell can spend idle time, and neither has to accept a collection at a moment it did not
    /// choose.
    pub fn set_gc_pacing(&mut self, automatic: bool) {
        self.gc_automatic = automatic;
    }

    /// A version of `Lua::enter` that expects failure and automatically converts [`Error`] into
    /// [`ExternError`], allowing the error type to escape the arena.
    pub fn try_enter<F, R>(&mut self, f: F) -> Result<R, ExternError>
    where
        F: for<'gc> FnOnce(Context<'gc>) -> Result<R, Error<'gc>>,
    {
        self.enter(move |ctx| f(ctx).map_err(Error::into_extern))
    }

    /// Run the given executor to completion.
    ///
    /// This will periodically exit the arena in order to collect garbage concurrently with running
    /// Lua code.
    pub fn finish(&mut self, executor: &StashedExecutor) -> Result<(), BadThreadMode> {
        const FUEL_PER_GC: i32 = 4096;

        loop {
            let mut fuel = Fuel::with(FUEL_PER_GC);

            let finished = self.enter(|ctx| ctx.fetch(executor).step(ctx, &mut fuel))?;

            // A sequence awaiting a foreign future cannot make progress here: polling it needs the
            // arena released, which only `finish_async` does. Stopping the executor and saying so
            // beats looping forever on a slice that can never advance.
            if self.enter(|ctx| ctx.fetch(executor).is_waiting()) {
                self.enter(|ctx| ctx.fetch(executor).stop(&ctx));
                return Err(BadThreadMode {
                    found: crate::ThreadMode::Waiting,
                    expected: Some(crate::ThreadMode::Normal),
                });
            }

            if finished {
                break;
            }

            // Handlers the collector queued run here, between slices, where there is an
            // `Executor` to drive them and the arena is not mid-cycle.
            if self.enter(|ctx| ctx.finalizers().has_pending()) {
                self.run_finalizers();
            }

            // Checked between slices rather than per allocation. That is coarse — a single huge
            // allocation inside one slice can overshoot before anyone looks — but it exists at all
            // only because the stackless VM hands control back here on a schedule the host sets.
            // A refusing allocator would be exact and is a much larger change.
            if let Some(limit) = self.memory_limit() {
                if self.total_memory() > limit {
                    // Collect before giving up: a script that has merely produced a lot of garbage
                    // should not be killed for it. Two cycles, because two-stage finalization
                    // resurrects finalizable objects on the first pass and only finds them dead on
                    // the second.
                    self.gc_collect();
                    self.gc_collect();
                }
                if self.total_memory() > limit {
                    self.enter(|ctx| ctx.fetch(executor).stop(&ctx));
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    /// The ceiling `finish` enforces on this instance's memory, if any.
    pub fn memory_limit(&self) -> Option<usize> {
        self.memory_limit
    }

    /// Stop execution once this instance is using more than `limit` bytes.
    ///
    /// Enforcement is *slice-granular*: the check happens between `Executor::step` calls in
    /// [`Lua::finish`], so usage can overshoot within a single slice. Pass `None` to remove it.
    ///
    /// A host driving `Executor::step` itself should do the same check in its own loop.
    pub fn set_memory_limit(&mut self, limit: Option<usize>) {
        self.memory_limit = limit;
    }

    /// Run any `__gc` handlers left waiting by the collector.
    ///
    /// A handler is Lua, and Lua cannot be called from inside a collection: the arena is mid-cycle
    /// and there is no `Executor` to drive. So the collector resurrects a dead object with a handler
    /// and queues it, and this drains the queue afterwards — the same deferral `collectgarbage`
    /// uses for the verbs that need `&mut Lua`.
    ///
    /// Called automatically by [`Lua::finish`], so a host driving execution normally never needs
    /// it. Call it directly if you drive `Executor::step` yourself.
    ///
    /// A handler that raises does not stop the others: the error is reported through the `warn`
    /// global if one is installed, matching what PUC-Rio does, and the remaining handlers still run.
    pub fn run_finalizers(&mut self) {
        loop {
            // Stashed on the way out: a `UserData<'gc>` cannot outlive the `enter` that produced it.
            let pending: Vec<crate::StashedValue> = self.enter(|ctx| {
                ctx.finalizers()
                    .take_pending(&ctx)
                    .into_iter()
                    .map(|ud| ctx.stash(ud))
                    .collect()
            });
            if pending.is_empty() {
                break;
            }

            for stashed in pending {
                let executor = self.try_enter(|ctx| {
                    let object = ctx.fetch(&stashed);
                    let handler = match crate::meta_ops::get_metatable(ctx, object) {
                        Some(mt) => mt.get_value(ctx, crate::MetaMethod::Gc),
                        None => Value::Nil,
                    };
                    if handler.is_nil() {
                        return Ok(None);
                    }
                    let function = crate::meta_ops::call(ctx, handler)?;
                    Ok(Some(
                        ctx.stash(crate::Executor::start(ctx, function, object)),
                    ))
                });

                let Ok(Some(executor)) = executor else {
                    continue;
                };

                if let Err(err) = self.execute::<()>(&executor) {
                    self.report_finalizer_error(&err.to_string());
                }
            }
        }
    }

    /// Report a `__gc` handler's error the way PUC-Rio does, without disturbing execution.
    fn report_finalizer_error(&mut self, message: &str) {
        let reported = self.try_enter(|ctx| {
            let warn = ctx.get_global_value("warn");
            if warn.is_nil() {
                return Ok(None);
            }
            let function = crate::meta_ops::call(ctx, warn)?;
            let text = ctx.intern(format!("error in __gc handler: {message}").as_bytes());
            Ok(Some(ctx.stash(crate::Executor::start(ctx, function, text))))
        });

        match reported {
            Ok(Some(executor)) => {
                let _ = self.execute::<()>(&executor);
            }
            // No `warn` installed, or it could not be called; the collector must not care.
            _ => eprintln!("Lua warning: error in __gc handler: {message}"),
        }
    }
    /// Run the given executor to completion and then take return values from the returning thread.
    ///
    /// This is equivalent to calling `Lua::finish` on an executor and then calling
    /// `Executor::take_result` yourself.
    pub fn execute<R: for<'gc> FromMultiValue<'gc>>(
        &mut self,
        executor: &StashedExecutor,
    ) -> Result<R, ExternError> {
        self.finish(executor).map_err(RuntimeError::new)?;
        self.try_enter(|ctx| ctx.fetch(executor).take_result::<R>(ctx)?)
    }

    /// Run the given executor to completion, awaiting any foreign futures it waits on.
    ///
    /// The asynchronous counterpart of [`Lua::finish`], and the only place in luna that awaits.
    /// A sequence that calls
    /// [`AsyncSequence::await_future`](crate::async_callback::AsyncSequence::await_future) parks
    /// its future and ends its slice; this takes the future *outside* `enter` — where the arena is
    /// not borrowed — awaits it, and steps again.
    ///
    /// The fuel budget is per slice, as in `finish`, and a slice that ends by parking a future has
    /// not spent the rest of its fuel. Waiting on I/O therefore never looks like a runaway script.
    #[cfg(feature = "async")]
    pub async fn finish_async(&mut self, executor: &StashedExecutor) -> Result<(), BadThreadMode> {
        const FUEL_PER_GC: i32 = 4096;

        loop {
            let mut fuel = Fuel::with(FUEL_PER_GC);
            let finished = self.enter(|ctx| ctx.fetch(executor).step(ctx, &mut fuel))?;

            // Taken and awaited with the arena released. Nothing else may hold a `'gc` value here.
            let parked = self.enter(|ctx| ctx.fetch(executor).take_pending_future(&ctx));
            if let Some(future) = parked {
                future.await;
                continue;
            }

            if self.enter(|ctx| ctx.finalizers().has_pending()) {
                self.run_finalizers();
            }

            if let Some(limit) = self.memory_limit() {
                if self.total_memory() > limit {
                    self.gc_collect();
                    self.gc_collect();
                }
                if self.total_memory() > limit {
                    self.enter(|ctx| ctx.fetch(executor).stop(&ctx));
                    return Ok(());
                }
            }

            if finished {
                return Ok(());
            }
        }
    }

    /// [`Lua::execute`] for an executor that may await foreign futures.
    #[cfg(feature = "async")]
    pub async fn execute_async<R: for<'gc> FromMultiValue<'gc>>(
        &mut self,
        executor: &StashedExecutor,
    ) -> Result<R, ExternError> {
        self.finish_async(executor)
            .await
            .map_err(RuntimeError::new)?;
        self.try_enter(|ctx| ctx.fetch(executor).take_result::<R>(ctx)?)
    }
}

#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
struct State<'gc> {
    globals: Table<'gc>,
    registry: Registry<'gc>,
    strings: InternedStringSet<'gc>,
    finalizers: Finalizers<'gc>,
    max_call_depth: Gc<'gc, Cell<usize>>,
    // What `collectgarbage` asked for. A callback has no `&mut Lua`, so it leaves a request here
    // and `Lua::enter` carries it out once `arena.mutate` has returned.
    gc_request: Gc<'gc, Cell<GcRequest>>,
}

impl<'gc> State<'gc> {
    fn new(mc: &Mutation<'gc>) -> State<'gc> {
        Self {
            globals: Table::new(mc),
            registry: Registry::new(mc),
            strings: InternedStringSet::new(mc),
            finalizers: Finalizers::new(mc),
            max_call_depth: Gc::new(mc, Cell::new(DEFAULT_MAX_CALL_DEPTH)),
            gc_request: Gc::new(mc, Cell::new(GcRequest::None)),
        }
    }

    fn ctx(&'gc self, mutation: &'gc Mutation<'gc>) -> Context<'gc> {
        Context {
            mutation,
            state: self,
        }
    }
}
