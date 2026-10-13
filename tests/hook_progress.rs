use std::{cell::Cell, rc::Rc};

use luna::{Callback, CallbackReturn, Closure, Executor, Fuel, Lua};

#[test]
fn count_one_hook_advances_each_instruction() {
    for budget in [1, 64, 65536] {
        let mut lua = Lua::empty();
        let calls = Rc::new(Cell::new(0));
        let executor = lua.enter(|ctx| {
            let calls = calls.clone();
            let hook = Callback::from_fn(&ctx, move |_, _, _| {
                calls.set(calls.get() + 1);
                Ok(CallbackReturn::Return)
            });
            ctx.set_debug_hook(hook.into(), false, 1);
            let closure =
                Closure::load(ctx, None, b"local n=0 for i=1,7 do n=n+i end return n").unwrap();
            ctx.stash(Executor::start(ctx, closure.into(), ()))
        });
        let mut completed = false;
        for _ in 0..1000 {
            completed = lua.enter(|ctx| {
                ctx.fetch(&executor)
                    .step(ctx, &mut Fuel::with(budget))
                    .unwrap()
            });
            lua.gc_collect();
            if completed {
                break;
            }
        }
        assert!(
            completed,
            "count-one hook stalled: budget={budget} calls={}",
            calls.get()
        );
        let result = lua.enter(|ctx| {
            ctx.fetch(&executor)
                .take_result::<i64>(ctx)
                .unwrap()
                .unwrap()
        });
        assert_eq!(result, 28);
        assert!(calls.get() > 7);
    }
}
