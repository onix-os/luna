//! The `debug` library, minus the parts a stackless VM cannot offer.
//!
//! `getinfo` and `traceback` read the frame chain the executor snapshots before each native call.
//! `getupvalue`/`setupvalue` go through `Closure::upvalues`, which luna exposes directly — mlua has
//! no equivalent and has to route through Lua's own `debug` table to reach them.
//!
//! `sethook` supports line and count hooks. The dispatch point in the opcode loop costs nothing
//! when no hook is installed — the flag is read once per VM slice, leaving one predictable branch,
//! measured at no change to VM throughput. Call and return hooks are *rejected* rather than
//! silently ignored: they would have to fire from every path that pushes or pops a frame, several
//! of which have no `Context` to call Lua with.
//!
//! `getlocal` and `setlocal` read the register→name table the compiler emits alongside each
//! prototype. That table is not free — it costs about 47% of a prototype's memory on locals-dense
//! code — so `Context::set_debug_locals(false)` turns it off for chunks compiled afterwards.

use ottavino_gc_arena::{Collect, Gc, Rootable};

use crate::{
    closure::UpValue, Callback, CallbackReturn, Closure, Context, Function, IntoValue, Singleton,
    Table, UserData, Value,
};

/// The table `debug.getregistry` hands back.
///
/// PUC-Rio's registry is where C code stashes values out of reach of scripts. luna has no C API, so
/// this is simply a table the host and its scripts can share without putting anything in `_G`; the
/// globals table sits at index 2, where PUC-Rio's `LUA_RIDX_GLOBALS` puts it. Nothing in luna reads
/// it, so a host is free to own it entirely.
#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct LuaRegistry<'gc>(pub Table<'gc>);

impl<'gc> Singleton<'gc> for LuaRegistry<'gc> {
    fn create(ctx: Context<'gc>) -> Self {
        let registry = Table::new(&ctx);
        registry.set(ctx, 2, ctx.globals()).ok();
        LuaRegistry(registry)
    }
}

/// Identifies one upvalue, for `debug.upvalueid`.
///
/// PUC-Rio returns a light userdata holding the address. luna has no light userdata, so this is a
/// real userdata holding the upvalue itself — which also keeps the upvalue alive, so the address
/// cannot be recycled underneath an id that is still reachable. Two ids for the same upvalue are
/// the *same* object, so `==` and `rawequal` both answer correctly.
#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
struct UpValueIds<'gc>(Table<'gc>);

impl<'gc> Singleton<'gc> for UpValueIds<'gc> {
    fn create(ctx: Context<'gc>) -> Self {
        let ids = Table::new(&ctx);
        // Weak values: an id that nothing holds any more may go, and the next request rebuilds it.
        let meta = Table::new(&ctx);
        meta.set_field(ctx, "__mode", "v");
        ids.set_metatable(ctx, Some(meta));
        UpValueIds(ids)
    }
}

/// Format one frame the way `debug.traceback` does.
fn describe(closure: Closure<'_>, line: crate::compiler::LineNumber) -> std::string::String {
    let proto = closure.prototype();
    format!(
        "\t{}:{}: in function",
        proto.chunk_name.display_lossy(),
        line
    )
}

/// The `index`-th local live at `level`, in declaration order.
///
/// A register alone does not identify a local: the allocator reuses registers between blocks, so
/// only the entries whose opcode range covers this frame's `pc` are in scope right now.
fn resolve_local<'gc>(
    exec: &crate::Execution<'gc, '_>,
    level: i64,
    index: i64,
) -> Option<(
    crate::thread::UpperLuaFrame<'gc>,
    crate::compiler::LocalVarInfo<crate::String<'gc>>,
)> {
    // Level 1 is the caller of `getlocal`, matching PUC-Rio.
    let frame = exec.frame_at(usize::try_from(level - 1).ok()?)?;
    let index = usize::try_from(index - 1).ok()?;
    let proto = frame.closure.prototype();
    let local = proto
        .locals
        .iter()
        .filter(|l| l.start_pc <= frame.pc && frame.pc < l.end_pc)
        .nth(index)?
        .clone();
    Some((frame, local))
}

fn local_at<'gc>(
    ctx: Context<'gc>,
    exec: &crate::Execution<'gc, '_>,
    level: i64,
    index: i64,
) -> Option<(crate::String<'gc>, Value<'gc>)> {
    let (frame, local) = resolve_local(exec, level, index)?;
    let slot = frame.base + local.register.0 as usize;
    let thread = exec.current_thread().thread;
    let state = thread.into_inner().borrow();
    let stack = state.stack().borrow();
    let _ = ctx;
    Some((local.name, stack.get(slot).copied().unwrap_or_default()))
}

pub fn load_debug<'gc>(ctx: Context<'gc>) {
    let debug = Table::new(&ctx);

    debug.set_field(
        ctx,
        "traceback",
        Callback::from_fn(&ctx, |ctx, exec, mut stack| {
            let message: Option<Value> = stack.consume(ctx)?;

            // A non-string message is returned untouched, as PUC-Rio does, so that a traceback
            // handler can be used with error objects.
            if let Some(v) = message {
                if !matches!(v, Value::String(_) | Value::Nil) {
                    stack.replace(ctx, v);
                    return Ok(CallbackReturn::Return);
                }
            }

            let mut out = std::string::String::new();
            if let Some(Value::String(s)) = message {
                out.push_str(&s.display_lossy().to_string());
                out.push('\n');
            }
            out.push_str("stack traceback:");

            let mut level = 0;
            while let Some(frame) = exec.lua_frame_at(level) {
                out.push('\n');
                out.push_str(&describe(frame.closure, frame.current_line));
                level += 1;
            }

            stack.replace(ctx, ctx.intern(out.as_bytes()));
            Ok(CallbackReturn::Return)
        }),
    );

    debug.set_field(
        ctx,
        "getinfo",
        Callback::from_fn(&ctx, |ctx, exec, mut stack| {
            let first = stack.get(0);

            let info = Table::new(&ctx);

            // Either a level into the running stack, or a function to describe directly.
            let described = match first {
                Value::Integer(_) | Value::Number(_) => {
                    // Level 1 is the caller of `getinfo`, matching PUC-Rio.
                    let level = first.to_integer().unwrap_or(1).max(1) as usize - 1;
                    match exec.lua_frame_at(level) {
                        Some(frame) => {
                            info.set_field(ctx, "currentline", frame.current_line.0 as i64);
                            Some(frame.closure)
                        }
                        None => {
                            stack.replace(ctx, Value::Nil);
                            return Ok(CallbackReturn::Return);
                        }
                    }
                }
                Value::Function(Function::Closure(c)) => Some(c),
                Value::Function(Function::Callback(_)) => {
                    // A Rust callback has no prototype to describe.
                    info.set_field(ctx, "source", ctx.intern(b"=[C]"));
                    info.set_field(ctx, "short_src", ctx.intern(b"[C]"));
                    info.set_field(ctx, "what", ctx.intern(b"C"));
                    info.set_field(ctx, "currentline", -1i64);
                    info.set_field(ctx, "linedefined", -1i64);
                    None
                }
                _ => {
                    return Err("bad argument #1 to 'getinfo'".into_value(ctx).into());
                }
            };

            if let Some(closure) = described {
                let proto = closure.prototype();
                let name = proto.chunk_name.display_lossy().to_string();
                info.set_field(ctx, "source", ctx.intern(format!("@{name}").as_bytes()));
                info.set_field(ctx, "short_src", ctx.intern(name.as_bytes()));
                info.set_field(ctx, "what", ctx.intern(b"Lua"));
                info.set_field(ctx, "nparams", proto.fixed_params as i64);
                info.set_field(ctx, "isvararg", proto.has_varargs);
                info.set_field(ctx, "nups", closure.upvalues().len() as i64);
                info.set_field(
                    ctx,
                    "linedefined",
                    proto
                        .opcode_line_numbers
                        .first()
                        .map(|(_, l)| l.0 as i64)
                        .unwrap_or(-1),
                );
                info.set_field(
                    ctx,
                    "lastlinedefined",
                    proto
                        .opcode_line_numbers
                        .last()
                        .map(|(_, l)| l.0 as i64)
                        .unwrap_or(-1),
                );
                info.set_field(ctx, "func", closure);
            }

            stack.replace(ctx, info);
            Ok(CallbackReturn::Return)
        }),
    );

    debug.set_field(
        ctx,
        "getupvalue",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (function, index): (Function, i64) = stack.consume(ctx)?;
            let Function::Closure(closure) = function else {
                stack.replace(ctx, Value::Nil);
                return Ok(CallbackReturn::Return);
            };
            let upvalues = closure.upvalues();
            match usize::try_from(index - 1)
                .ok()
                .and_then(|i| upvalues.get(i))
            {
                // luna keeps no upvalue names, so the name slot is the index.
                Some(slot) => {
                    let up = slot.get();
                    // An open upvalue still aliases a live stack slot; read through it.
                    let value = match up.get() {
                        crate::closure::UpValueState::Closed(v) => v,
                        crate::closure::UpValueState::Open(open) => open.get(&ctx),
                    };
                    stack.replace(ctx, (ctx.intern(index.to_string().as_bytes()), value));
                }
                None => stack.replace(ctx, Value::Nil),
            }
            Ok(CallbackReturn::Return)
        }),
    );

    debug.set_field(
        ctx,
        "setupvalue",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (function, index, value): (Function, i64, Value) = stack.consume(ctx)?;
            let Function::Closure(closure) = function else {
                stack.replace(ctx, Value::Nil);
                return Ok(CallbackReturn::Return);
            };
            let upvalues = closure.upvalues();
            match usize::try_from(index - 1)
                .ok()
                .and_then(|i| upvalues.get(i))
            {
                Some(slot) => {
                    let up = slot.get();
                    match up.get() {
                        crate::closure::UpValueState::Open(open) => open.set(&ctx, value),
                        crate::closure::UpValueState::Closed(_) => {
                            up.set(&ctx, crate::closure::UpValueState::Closed(value))
                        }
                    }
                    stack.replace(ctx, ctx.intern(index.to_string().as_bytes()));
                }
                None => stack.replace(ctx, Value::Nil),
            }
            Ok(CallbackReturn::Return)
        }),
    );

    // `getlocal`/`setlocal` take a *level*, not a function: naming a function's parameters without
    // an activation would need no stack at all, and is the less useful half.
    debug.set_field(
        ctx,
        "getlocal",
        Callback::from_fn(&ctx, |ctx, exec, mut stack| {
            let (level, index): (i64, i64) = stack.consume(ctx)?;
            let Some((name, value)) = local_at(ctx, &exec, level, index) else {
                stack.replace(ctx, Value::Nil);
                return Ok(CallbackReturn::Return);
            };
            stack.replace(ctx, (name, value));
            Ok(CallbackReturn::Return)
        }),
    );

    debug.set_field(
        ctx,
        "setlocal",
        Callback::from_fn(&ctx, |ctx, exec, mut stack| {
            let (level, index, value): (i64, i64, Value) = stack.consume(ctx)?;
            let Some((frame, local)) = resolve_local(&exec, level, index) else {
                stack.replace(ctx, Value::Nil);
                return Ok(CallbackReturn::Return);
            };
            let slot = frame.base + local.register.0 as usize;
            let thread = exec.current_thread().thread;
            let state = thread.into_inner().borrow();
            state.stack().borrow_mut(&ctx)[slot] = value;
            stack.replace(ctx, local.name);
            Ok(CallbackReturn::Return)
        }),
    );

    debug.set_field(
        ctx,
        "sethook",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (hook, mask, count): (Value, Option<crate::String>, Option<i64>) =
                stack.consume(ctx)?;

            // No arguments clears the hook, as in PUC-Rio.
            if hook.is_nil() {
                ctx.set_debug_hook(Value::Nil, false, 0);
                return Ok(CallbackReturn::Return);
            }

            let mask = mask
                .map(|m| m.display_lossy().to_string())
                .unwrap_or_default();
            // Call and return hooks would have to fire from every path that pushes or pops a
            // frame, several of which have no `Context` to call Lua with. Rejecting the mask is
            // the honest answer: accepting one that never fires would be worse than saying no.
            if mask.contains('c') || mask.contains('r') {
                return Err(
                    "call and return hooks are not implemented; only 'l' and a count are"
                        .into_value(ctx)
                        .into(),
                );
            }

            let count = count.unwrap_or(0).max(0) as u32;
            let line = mask.contains('l');
            if !line && count == 0 {
                return Err("a hook needs the 'l' mask or a count"
                    .into_value(ctx)
                    .into());
            }

            ctx.set_debug_hook(hook, line, count);
            Ok(CallbackReturn::Return)
        }),
    );

    debug.set_field(
        ctx,
        "gethook",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let mask = if ctx.hook_line() { "l" } else { "" };
            stack.replace(ctx, (ctx.debug_hook(), ctx.intern(mask.as_bytes())));
            Ok(CallbackReturn::Return)
        }),
    );

    debug.set_field(
        ctx,
        "getmetatable",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            // Unlike the global, this one ignores `__metatable` — that is the point of it.
            let value = stack.get(0);
            stack.replace(ctx, crate::meta_ops::get_metatable(ctx, value));
            Ok(CallbackReturn::Return)
        }),
    );

    debug.set_field(
        ctx,
        "setmetatable",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (t, mt): (Table, Option<Table>) = stack.consume(ctx)?;
            t.set_metatable(ctx, mt);
            stack.replace(ctx, t);
            Ok(CallbackReturn::Return)
        }),
    );

    debug.set_field(
        ctx,
        "getregistry",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let LuaRegistry(registry) = *ctx.singleton::<Rootable![LuaRegistry<'_>]>();
            stack.replace(ctx, registry);
            Ok(CallbackReturn::Return)
        }),
    );

    debug.set_field(
        ctx,
        "getuservalue",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (value, n): (Value, Option<i64>) = stack.consume(ctx)?;
            let n = n.unwrap_or(1);
            match value {
                // A second return says whether the userdata has a value there at all, which is how
                // a script tells "absent" from "present and nil".
                Value::UserData(u) if n >= 1 => {
                    let value = u.user_value(ctx, n);
                    stack.replace(ctx, (value.unwrap_or(Value::Nil), value.is_some()));
                }
                Value::UserData(_) => stack.replace(ctx, (Value::Nil, false)),
                _ => stack.replace(ctx, Value::Nil),
            }
            Ok(CallbackReturn::Return)
        }),
    );

    debug.set_field(
        ctx,
        "setuservalue",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (udata, value, n): (UserData, Value, Option<i64>) = stack.consume(ctx)?;
            let n = n.unwrap_or(1);
            if n < 1 {
                return Err("bad argument #3 to 'setuservalue' (index out of range)"
                    .into_value(ctx)
                    .into());
            }
            udata.set_user_value(ctx, n, value);
            stack.replace(ctx, udata);
            Ok(CallbackReturn::Return)
        }),
    );

    debug.set_field(
        ctx,
        "upvalueid",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (function, index): (Function, i64) = stack.consume(ctx)?;
            let Function::Closure(closure) = function else {
                stack.replace(ctx, Value::Nil);
                return Ok(CallbackReturn::Return);
            };
            let Some(slot) = usize::try_from(index - 1)
                .ok()
                .and_then(|i| closure.upvalues().get(i))
            else {
                return Err("bad argument #2 to 'upvalueid' (index out of range)"
                    .into_value(ctx)
                    .into());
            };

            // Keyed by the upvalue's address, so the same upvalue always answers with the same id
            // however many closures reach it — which is the entire purpose of `upvalueid`.
            let upvalue = slot.get();
            let key = Value::Integer(Gc::as_ptr(upvalue.into_inner()) as usize as i64);
            let UpValueIds(ids) = *ctx.singleton::<Rootable![UpValueIds<'_>]>();
            let id = match ids.get_value(ctx, key) {
                Value::Nil => {
                    let id =
                        Value::UserData(UserData::new::<Rootable![UpValue<'_>]>(&ctx, upvalue));
                    ids.set(ctx, key, id).ok();
                    id
                }
                existing => existing,
            };
            stack.replace(ctx, id);
            Ok(CallbackReturn::Return)
        }),
    );

    debug.set_field(
        ctx,
        "upvaluejoin",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (f1, n1, f2, n2): (Function, i64, Function, i64) = stack.consume(ctx)?;
            let (Function::Closure(c1), Function::Closure(c2)) = (f1, f2) else {
                return Err("bad argument to 'upvaluejoin' (Lua function expected)"
                    .into_value(ctx)
                    .into());
            };
            let index = |n: i64, c: Closure<'_>, arg: &'static str| {
                usize::try_from(n - 1)
                    .ok()
                    .filter(|i| *i < c.upvalues().len())
                    .ok_or_else(|| {
                        crate::Error::from_value(
                            format!("bad argument {arg} to 'upvaluejoin' (index out of range)")
                                .into_value(ctx),
                        )
                    })
            };
            let (i1, i2) = (index(n1, c1, "#2")?, index(n2, c2, "#4")?);
            c1.set_upvalue(&ctx, i1, c2.upvalues()[i2].get());
            stack.clear();
            Ok(CallbackReturn::Return)
        }),
    );

    ctx.set_global("debug", debug);
}
