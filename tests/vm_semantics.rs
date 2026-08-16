//! VM behaviour that a script can observe: loop preparation, and what a callback sees of the stack
//! it was handed.

use luna::{Callback, CallbackReturn, Closure, Executor, ExternError, Lua, Value, Variadic};

fn eval<T: for<'gc> luna::FromMultiValue<'gc> + 'static>(source: &str) -> Result<T, ExternError> {
    let mut lua = Lua::full();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("probe"), source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<T>(&executor)
}

#[test]
fn an_integer_for_loop_with_a_zero_step_errors() -> Result<(), ExternError> {
    let message = eval::<String>(
        r#"
        local ok, err = pcall(function() for i = 1, 10, 0 do end end)
        assert(not ok)
        return tostring(err)
    "#,
    )?;
    assert!(message.contains("'for' step is zero"), "{message}");
    Ok(())
}

#[test]
fn a_float_for_loop_with_a_zero_step_errors() -> Result<(), ExternError> {
    let message = eval::<String>(
        r#"
        local ok, err = pcall(function() for i = 1.0, 10.0, 0.0 do end end)
        assert(not ok)
        return tostring(err)
    "#,
    )?;
    assert!(message.contains("'for' step is zero"), "{message}");
    Ok(())
}

#[test]
fn a_zero_step_held_in_a_variable_errors() -> Result<(), ExternError> {
    // The step is only known at run time here, so the check cannot be a compile-time one.
    let message = eval::<String>(
        r#"
        local step = 0
        local ok, err = pcall(function() for i = 1, 10, step do end end)
        assert(not ok)
        return tostring(err)
    "#,
    )?;
    assert!(message.contains("'for' step is zero"), "{message}");
    Ok(())
}

#[test]
fn a_zero_step_error_carries_its_position() -> Result<(), ExternError> {
    let message = eval::<String>(
        r#"
        local ok, err = pcall(function()
            for i = 1, 10, 0 do end
        end)
        return tostring(err)
    "#,
    )?;
    assert!(message.contains("probe:3"), "{message}");
    Ok(())
}

#[test]
fn the_zero_step_is_rejected_before_the_first_iteration() -> Result<(), ExternError> {
    assert_eq!(
        eval::<i64>(
            r#"
            local runs = 0
            pcall(function() for i = 1, 10, 0 do runs = runs + 1 end end)
            return runs
        "#
        )?,
        0
    );
    Ok(())
}

#[test]
fn returns_reach_the_caller_whatever_their_count() -> Result<(), ExternError> {
    // Returning leaves the caller's registers above the results alone, so what it does still see
    // has to be exactly the results it asked for — including the `nil`s for the ones it did not
    // get, and nothing left over from the frame that returned.
    assert_eq!(
        eval::<bool>(
            r#"
            local function none() end
            local function one() return 1 end
            local function three() return 1, 2, 3 end
            local function passthrough(...) return ... end

            local a, b, c = none()
            assert(a == nil and b == nil and c == nil)

            local d, e, f = one()
            assert(d == 1 and e == nil and f == nil)

            local g, h, i, j = three()
            assert(g == 1 and h == 2 and i == 3 and j == nil)

            assert(select('#', three()) == 3)
            assert(select('#', none()) == 0)
            assert(select('#', passthrough(1, nil, 3)) == 3)

            local t = { three() }
            assert(#t == 3 and t[3] == 3)

            local u = { three(), three() }
            assert(#u == 4 and u[1] == 1 and u[2] == 1 and u[4] == 3)

            -- Deep enough that a frame is reused many times over.
            local function sum(n) if n == 0 then return 0 end return n + sum(n - 1) end
            assert(sum(200) == 20100)

            -- Results crossing a tail call and a metamethod call.
            local function tail(x) return one() end
            assert(tail(5) == 1)

            local o = setmetatable({}, { __index = function(_, k) return k .. "!" end })
            local k, l = o.x, o.y
            assert(k == "x!" and l == "y!")

            return true
        "#
        )?,
        true
    );
    Ok(())
}

#[test]
fn a_returned_frame_leaves_nothing_reachable_to_the_caller() -> Result<(), ExternError> {
    // A caller reads a register it has written; a stale value left by the frame that returned must
    // never be one of those.
    assert_eq!(
        eval::<i64>(
            r#"
            local function make() return 10, 20, 30 end
            local total = 0
            for _ = 1, 100 do
                local a, b = make()
                local c = (a or 0) + (b or 0)
                total = total + c
            end
            return total
        "#
        )?,
        3000
    );
    Ok(())
}

#[test]
fn draining_a_stack_from_the_back_removes_the_whole_range() -> Result<(), ExternError> {
    // `Stack::drain` yields from either end, and what came off the back used to be left behind:
    // the callback's arguments reappeared among its return values.
    let mut lua = Lua::core();
    let executor = lua.enter(|ctx| {
        let callback = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let backwards: Vec<Value> = stack.drain(..).rev().collect();
            stack.into_back(ctx, Variadic(backwards));
            Ok(CallbackReturn::Return)
        });
        ctx.stash(Executor::start(ctx, callback.into(), (1i64, 2i64, 3i64)))
    });
    assert_eq!(
        lua.execute::<Variadic<Vec<i64>>>(&executor)?.0,
        vec![3, 2, 1]
    );
    Ok(())
}

#[test]
fn partially_draining_a_stack_from_the_back_removes_the_whole_range() -> Result<(), ExternError> {
    // A single value taken off the back and then dropped: the rest of the range goes too, as
    // `Vec::drain` does.
    let mut lua = Lua::core();
    let executor = lua.enter(|ctx| {
        let callback = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let last = {
                let mut drain = stack.drain(..);
                drain.next_back()
            };
            stack.into_back(ctx, last.unwrap_or_default());
            Ok(CallbackReturn::Return)
        });
        ctx.stash(Executor::start(ctx, callback.into(), (1i64, 2i64, 3i64)))
    });
    assert_eq!(lua.execute::<Variadic<Vec<i64>>>(&executor)?.0, vec![3]);
    Ok(())
}

#[test]
fn ordinary_numeric_for_loops_still_run() -> Result<(), ExternError> {
    assert_eq!(
        eval::<i64>(
            r#"
            local n = 0
            for _ = 1, 10 do n = n + 1 end
            for _ = 10, 1, -2 do n = n + 1 end
            for _ = 1.0, 2.0, 0.25 do n = n + 1 end
            for _ = 1, 0 do n = n + 100 end
            return n
        "#
        )?,
        20
    );
    Ok(())
}
