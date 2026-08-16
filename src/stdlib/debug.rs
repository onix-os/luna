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
//! **Not here:** `getlocal` and `setlocal` need a register→name table the compiler does not emit.

use crate::{Callback, CallbackReturn, Closure, Context, Function, IntoValue, Table, Value};

/// Format one frame the way `debug.traceback` does.
fn describe(closure: Closure<'_>, line: crate::compiler::LineNumber) -> std::string::String {
    let proto = closure.prototype();
    format!(
        "\t{}:{}: in function",
        proto.chunk_name.display_lossy(),
        line
    )
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
                Some(up) => {
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
                Some(up) => {
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

    ctx.set_global("debug", debug);
}
