pub mod any;
pub mod async_callback;
pub mod callback;
pub mod closure;
pub mod compiler;
pub mod constant;
pub mod conversion;
pub mod error;
pub mod finalizers;
pub mod fuel;
pub mod function;
pub mod io;
pub mod lua;
pub mod meta_ops;
pub mod opcode;
pub mod registry;
pub mod stack;
pub mod stash;
pub mod stdlib;
pub mod string;
pub mod table;
pub mod thread;
pub mod types;
pub mod userdata;
pub mod value;

pub use self::{
    async_callback::{async_sequence, SequenceReturn},
    callback::{BoxSequence, Callback, CallbackFn, CallbackReturn, Sequence, SequencePoll},
    closure::{Closure, CompilerError, FunctionPrototype},
    constant::Constant,
    conversion::{Either, FromMultiValue, FromValue, IntoMultiValue, IntoValue, Variadic},
    error::{Error, ExternError, RuntimeError, TypeError},
    fuel::Fuel,
    function::Function,
    lua::{Context, GcRequest, Lua},
    meta_ops::MetaMethod,
    registry::{Registry, Singleton},
    stack::Stack,
    stash::{
        StashedCallback, StashedClosure, StashedError, StashedExecutor, StashedFunction,
        StashedString, StashedTable, StashedThread, StashedUserData, StashedValue,
    },
    string::String,
    table::Table,
    thread::{Execution, Executor, ExecutorMode, Thread, ThreadMode},
    userdata::UserData,
    value::Value,
};

/// The result of most luna operations that can raise a Lua error.
pub type Result<'gc, T> = std::result::Result<T, Error<'gc>>;

/// Everything an embedder normally wants, under names that do not collide with `std`.
///
/// `use luna::*` pulls in `String`, `Error`, `Table` and `Function` under bare names, which shadow
/// `std::string::String` and `std::error::Error` and make the resulting code hard to read. The
/// prelude renames the two that clash and leaves the rest alone:
///
/// ```
/// use luna::prelude::*;
///
/// let mut lua = Lua::core();
/// lua.enter(|ctx| {
///     let t = LuaTable::new(&ctx);
///     t.set(ctx, "answer", 42).unwrap();
/// });
/// ```
pub mod prelude {
    pub use crate::{
        BoxSequence, Callback, CallbackReturn, Context, Execution, Executor, Fuel, Lua,
        SequencePoll, Variadic,
    };

    pub use crate::{
        Closure as LuaClosure, Error as LuaError, Function as LuaFunction, String as LuaString,
        Table as LuaTable, Thread as LuaThread, UserData as LuaUserData, Value as LuaValue,
    };

    pub use crate::{FromMultiValue, FromValue, IntoMultiValue, IntoValue};
}
