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
        load_base, load_coroutine, load_io, load_math, load_os, load_package, load_string,
        load_table, load_utf8,
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
    // The arena has no stop switch to read back, so the flag lives here.
    gc_running: bool,
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
            gc_running: true,
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

    /// Run one incremental slice of collection.
    pub fn gc_step(&mut self) {
        // `enter` already performs a slice when there is debt; forcing one through an empty
        // closure is the same path.
        self.enter(|_| ());
    }

    /// Stop the collector pacing itself.
    ///
    /// Implemented by driving the sleep threshold up rather than by a flag in the arena, which has
    /// no stop switch of its own: with nothing ever owed, no slice is ever due.
    pub fn gc_stop(&mut self) {
        self.arena
            .metrics()
            .set_pacing(ottavino_gc_arena::metrics::Pacing::default().with_min_sleep(usize::MAX));
        self.gc_running = false;
    }

    /// Resume normal pacing after [`Lua::gc_stop`].
    pub fn gc_restart(&mut self) {
        self.arena
            .metrics()
            .set_pacing(ottavino_gc_arena::metrics::Pacing::default());
        self.gc_running = true;
    }

    /// Whether the collector is pacing itself.
    pub fn gc_is_running(&self) -> bool {
        self.gc_running
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
    /// Automatically triggers garbage collection before returning if the allocation debt is larger
    /// than a small constant.
    pub fn enter<F, T>(&mut self, f: F) -> T
    where
        F: for<'gc> FnOnce(Context<'gc>) -> T,
    {
        const COLLECTOR_GRANULARITY: f64 = 1024.0;

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
            GcRequest::Step => self.gc_step(),
            GcRequest::Stop => self.gc_stop(),
            GcRequest::Restart => self.gc_restart(),
        }

        if self.arena.metrics().allocation_debt() > COLLECTOR_GRANULARITY {
            if self.arena.collection_phase() == CollectionPhase::Sweeping {
                self.arena.collect_debt();
            } else {
                if let Some(marked) = self.arena.mark_debt() {
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
        }
        r
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

            if self.enter(|ctx| ctx.fetch(executor).step(ctx, &mut fuel))? {
                break;
            }

            // Checked between slices rather than per allocation. That is coarse — a single huge
            // allocation inside one slice can overshoot before anyone looks — but it exists at all
            // only because the stackless VM hands control back here on a schedule the host sets.
            // A refusing allocator would be exact and is a much larger change.
            if let Some(limit) = self.memory_limit() {
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
