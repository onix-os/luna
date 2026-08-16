use std::{
    fmt,
    hash::{Hash, Hasher},
};

use allocator_api2::{boxed, vec, SliceExt};
use ottavino_gc_arena::{allocator_api::MetricsAlloc, lock::Lock, Collect, Gc, Mutation};
use thiserror::Error;

use crate::{
    compiler::{self, CompiledPrototype, FunctionRef, LineNumber},
    opcode::OpCode,
    thread::OpenUpValue,
    types::UpValueDescriptor,
    Constant, Context, String, Table, Value,
};

// Note: These errors must not have #[error(transparent)] so that
// anyhow::Error::root_cause and downcasting work as expected by the
// interpreter. (Even though that gives slightly cleaner error messages).
#[derive(Debug, Error)]
pub enum CompilerError {
    #[error("parse error")]
    Parsing(#[from] compiler::ParseError),
    #[error("compile error")]
    Compilation(#[from] compiler::CompileError),
    #[error("bad binary chunk")]
    Dump(#[from] crate::dump::DumpError),
    // Reachable only for a binary chunk: source always compiles to a prototype whose only upvalue
    // is `_ENV`, but a chunk read off disk can claim otherwise.
    #[error("chunk cannot be a top-level function")]
    NotTopLevel(#[from] ClosureError),
}

/// A compiled Lua function.
///
/// In Lua jargon, a "prototype" is only executable code, it has none of its "upvalues" set and
/// cannot be called directly.
///
/// If a prototype has only an single (optional) `_ENV` upvalue, then it can be turned into an
/// executable `Closure` by binding it with its environment with [`Closure::new`].
#[derive(Collect)]
#[collect(no_drop)]
pub struct FunctionPrototype<'gc> {
    pub chunk_name: String<'gc>,
    pub reference: FunctionRef<String<'gc>>,
    pub fixed_params: u8,
    pub has_varargs: bool,
    pub stack_size: u16,
    pub constants: boxed::Box<[Constant<String<'gc>>], MetricsAlloc<'gc>>,
    pub opcodes: boxed::Box<[OpCode], MetricsAlloc<'gc>>,
    pub opcode_line_numbers: boxed::Box<[(usize, LineNumber)], MetricsAlloc<'gc>>,
    pub upvalues: boxed::Box<[UpValueDescriptor], MetricsAlloc<'gc>>,
    pub prototypes: boxed::Box<[Gc<'gc, FunctionPrototype<'gc>>], MetricsAlloc<'gc>>,
    /// Named locals with the opcode ranges they were live for.
    ///
    /// Always emitted rather than gated behind a compile option: measured at a few percent of a
    /// prototype's size, because the names are interned strings shared with the constant table
    /// rather than fresh allocations. PUC-Rio does the same and offers `strip` to drop them.
    pub locals: boxed::Box<[crate::compiler::LocalVarInfo<String<'gc>>], MetricsAlloc<'gc>>,
}

/// A summary rather than a dump.
///
/// Deriving this printed every opcode, constant and nested prototype, which is almost never what a
/// caller wants and costs several KB of binary in `Debug` impls for the opcode representation. The
/// fields are all public if the detail is actually wanted.
impl<'gc> fmt::Debug for FunctionPrototype<'gc> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("FunctionPrototype")
            .field("chunk_name", &self.chunk_name)
            .field("fixed_params", &self.fixed_params)
            .field("has_varargs", &self.has_varargs)
            .field("stack_size", &self.stack_size)
            .field("opcodes", &self.opcodes.len())
            .field("constants", &self.constants.len())
            .field("upvalues", &self.upvalues.len())
            .field("prototypes", &self.prototypes.len())
            .finish()
    }
}

impl<'gc> FunctionPrototype<'gc> {
    pub fn from_compiled(
        mc: &Mutation<'gc>,
        chunk_name: String<'gc>,
        compiled_function: &CompiledPrototype<String<'gc>>,
        keep_locals: bool,
    ) -> Self {
        Self::from_compiled_map_strings(mc, chunk_name, compiled_function, keep_locals, |s| *s)
    }

    pub fn from_compiled_map_strings<S>(
        mc: &Mutation<'gc>,
        chunk_name: String<'gc>,
        compiled_function: &CompiledPrototype<S>,
        keep_locals: bool,
        map_string: impl Fn(&S) -> String<'gc>,
    ) -> Self {
        fn new<'gc, S>(
            mc: &Mutation<'gc>,
            chunk_name: String<'gc>,
            compiled_function: &CompiledPrototype<S>,
            keep_locals: bool,
            map_string: impl Fn(&S) -> String<'gc> + Copy,
        ) -> FunctionPrototype<'gc> {
            let alloc = MetricsAlloc::new(mc);

            let mut constants = vec::Vec::new_in(alloc.clone());
            constants.extend(
                compiled_function
                    .constants
                    .iter()
                    .map(|c| c.as_string_ref().map_string(map_string)),
            );

            let opcodes = SliceExt::to_vec_in(compiled_function.opcodes.as_slice(), alloc.clone());
            let opcode_line_numbers = SliceExt::to_vec_in(
                compiled_function.opcode_line_numbers.as_slice(),
                alloc.clone(),
            );
            let upvalues =
                SliceExt::to_vec_in(compiled_function.upvalues.as_slice(), alloc.clone());

            // Sorted by where each scope begins: the compiler emits them as scopes *end*, which is
            // innermost-first, and `debug.getlocal` indexes them in declaration order.
            let mut locals = vec::Vec::new_in(alloc.clone());
            locals.extend(
                compiled_function
                    .locals
                    .iter()
                    .filter(|_| keep_locals)
                    .map(|l| crate::compiler::LocalVarInfo {
                        name: map_string(&l.name),
                        register: l.register,
                        start_pc: l.start_pc,
                        end_pc: l.end_pc,
                    }),
            );
            locals.sort_by_key(|l: &crate::compiler::LocalVarInfo<String<'gc>>| {
                (l.start_pc, l.register.0)
            });

            let mut prototypes = vec::Vec::new_in(alloc);
            prototypes.extend(
                compiled_function
                    .prototypes
                    .iter()
                    .map(|cf| Gc::new(mc, new(mc, chunk_name, cf, keep_locals, map_string))),
            );

            FunctionPrototype {
                chunk_name,
                reference: compiled_function
                    .reference
                    .as_string_ref()
                    .map_strings(map_string),
                fixed_params: compiled_function.fixed_params,
                has_varargs: compiled_function.has_varargs,
                stack_size: compiled_function.stack_size,
                constants: constants.into_boxed_slice(),
                opcodes: opcodes.into_boxed_slice(),
                opcode_line_numbers: opcode_line_numbers.into_boxed_slice(),
                upvalues: upvalues.into_boxed_slice(),
                prototypes: prototypes.into_boxed_slice(),
                locals: locals.into_boxed_slice(),
            }
        }

        new(mc, chunk_name, compiled_function, keep_locals, &map_string)
    }

    pub fn compile(
        ctx: Context<'gc>,
        source_name: &str,
        source: &[u8],
    ) -> Result<FunctionPrototype<'gc>, CompilerError> {
        #[derive(Copy, Clone)]
        struct Interner<'gc>(Context<'gc>);

        impl<'gc> compiler::StringInterner for Interner<'gc> {
            type String = String<'gc>;

            fn intern(&mut self, s: &[u8]) -> Self::String {
                self.0.intern(s)
            }
        }

        let interner = Interner(ctx);

        let chunk = compiler::parse_chunk(source, interner)?;
        let compiled_function = compiler::compile_chunk(&chunk, interner)?;

        Ok(FunctionPrototype::from_compiled(
            &ctx,
            ctx.intern(source_name.as_bytes()),
            &compiled_function,
            ctx.debug_locals(),
        ))
    }
}

#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub enum UpValueState<'gc> {
    Open(OpenUpValue<'gc>),
    Closed(Value<'gc>),
}

pub type UpValueInner<'gc> = Lock<UpValueState<'gc>>;

#[derive(Debug, Collect, Copy, Clone)]
#[collect(no_drop)]
pub struct UpValue<'gc>(Gc<'gc, UpValueInner<'gc>>);

impl<'gc> UpValue<'gc> {
    pub fn new(mc: &Mutation<'gc>, state: UpValueState<'gc>) -> Self {
        Self(Gc::new(mc, Lock::new(state)))
    }

    pub fn from_inner(inner: Gc<'gc, UpValueInner<'gc>>) -> Self {
        Self(inner)
    }

    pub fn into_inner(self) -> Gc<'gc, UpValueInner<'gc>> {
        self.0
    }

    pub fn get(self) -> UpValueState<'gc> {
        self.0.get()
    }

    pub fn set(self, mc: &Mutation<'gc>, state: UpValueState<'gc>) {
        self.0.set(mc, state)
    }
}

#[derive(Debug, Copy, Clone, Error)]
pub enum ClosureError {
    #[error("cannot use prototype with upvalues other than _ENV to create top-level closure")]
    HasUpValues,
    #[error("closure requires _ENV upvalue but no environment was provided")]
    RequiresEnv,
}

#[derive(Collect)]
#[collect(no_drop)]
pub struct ClosureInner<'gc> {
    proto: Gc<'gc, FunctionPrototype<'gc>>,
    upvalues: vec::Vec<UpValue<'gc>, MetricsAlloc<'gc>>,
}

/// As [`FunctionPrototype`]: the upvalue *count*, not every captured value. Walking them would
/// print the whole reachable object graph, and would instantiate `Debug` for all of it.
impl<'gc> fmt::Debug for ClosureInner<'gc> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("ClosureInner")
            .field("proto", &*self.proto)
            .field("upvalues", &self.upvalues.len())
            .finish()
    }
}

/// A garbage collected pointer to an executable Lua function.
///
/// A `Closure` represents a [`FunctionPrototype`] bound to an environment. A closure "closes over"
/// free variables that it references, and as such, calling a `Closure` may reference (and mutate!)
/// these closed over variables. In Lua jargon, these references that closures "close over" are
/// called "upvalues".
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct Closure<'gc>(Gc<'gc, ClosureInner<'gc>>);

impl<'gc> PartialEq for Closure<'gc> {
    fn eq(&self, other: &Closure<'gc>) -> bool {
        Gc::ptr_eq(self.0, other.0)
    }
}

impl<'gc> Eq for Closure<'gc> {}

impl<'gc> Hash for Closure<'gc> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Gc::as_ptr(self.0).hash(state)
    }
}

impl<'gc> Closure<'gc> {
    /// Create a top-level closure, prototype must not have any upvalues besides _ENV.
    pub fn new(
        mc: &Mutation<'gc>,
        proto: FunctionPrototype<'gc>,
        environment: Option<Table<'gc>>,
    ) -> Result<Closure<'gc>, ClosureError> {
        let proto = Gc::new(mc, proto);
        let mut upvalues = vec::Vec::new_in(MetricsAlloc::new(mc));

        if !proto.upvalues.is_empty() {
            if proto.upvalues.len() > 1 || proto.upvalues[0] != UpValueDescriptor::Environment {
                return Err(ClosureError::HasUpValues);
            } else if let Some(environment) = environment {
                upvalues.push(UpValue(Gc::new(
                    mc,
                    Lock::new(UpValueState::Closed(Value::Table(environment))),
                )));
            } else {
                return Err(ClosureError::RequiresEnv);
            }
        }

        Ok(Closure(Gc::new(mc, ClosureInner { proto, upvalues })))
    }

    pub fn from_parts(
        mc: &Mutation<'gc>,
        proto: Gc<'gc, FunctionPrototype<'gc>>,
        upvalues: vec::Vec<UpValue<'gc>, MetricsAlloc<'gc>>,
    ) -> Self {
        Self(Gc::new(mc, ClosureInner { proto, upvalues }))
    }

    pub fn from_inner(inner: Gc<'gc, ClosureInner<'gc>>) -> Self {
        Self(inner)
    }

    pub fn into_inner(self) -> Gc<'gc, ClosureInner<'gc>> {
        self.0
    }

    /// Compile a top-level closure from source, using the globals table as the `_ENV` table.
    pub fn load(
        ctx: Context<'gc>,
        name: Option<&str>,
        source: &[u8],
    ) -> Result<Closure<'gc>, CompilerError> {
        Self::load_with_env(ctx, name, source, ctx.globals())
    }

    /// Compile a top-level closure from source, using the given table as the `_ENV` table.
    pub fn load_with_env(
        ctx: Context<'gc>,
        name: Option<&str>,
        source: &[u8],
        env: Table<'gc>,
    ) -> Result<Closure<'gc>, CompilerError> {
        // A dumped chunk is loaded rather than compiled. It is checked on the way in — see
        // `crate::dump` — because nothing about the bytes is trustworthy.
        let proto = if crate::dump::is_binary_chunk(source) {
            crate::dump::undump(ctx, source)?
        } else {
            FunctionPrototype::compile(ctx, name.unwrap_or("<anonymous>"), source)?
        };
        Ok(Closure::new(&ctx, proto, Some(env))?)
    }

    pub fn prototype(self) -> Gc<'gc, FunctionPrototype<'gc>> {
        self.0.proto
    }

    pub fn upvalues(self) -> &'gc [UpValue<'gc>] {
        &Gc::as_ref(self.0).upvalues
    }
}
