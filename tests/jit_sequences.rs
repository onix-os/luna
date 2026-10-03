#![cfg(all(
    feature = "jit",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use std::{cell::Cell, pin::Pin, rc::Rc};

use luna::{
    BoxSequence, Callback, CallbackReturn, Closure, Context, Error, Execution, Executor,
    ExecutorMode, Fuel, Function, JitConfig, JitMode, Lua, Sequence, SequencePoll, Stack, Table,
    Value, Variadic,
};
use ottavino_gc_arena::Collect;

#[derive(Collect)]
#[collect(no_drop)]
struct Continuation<'gc> {
    function: Function<'gc>,
    table: Table<'gc>,
    marker: Table<'gc>,
    fails: bool,
    #[collect(require_static)]
    phase: Rc<Cell<u8>>,
}

impl<'gc> Sequence<'gc> for Continuation<'gc> {
    fn poll(
        self: Pin<&mut Self>,
        ctx: Context<'gc>,
        mut exec: Execution<'gc, '_>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        match self.phase.get() {
            0 => {
                assert_eq!(self.table.get::<_, i64>(ctx, "n")?, 100);
                self.table.set(ctx, "host", 1)?;
                stack.replace(ctx, (7, None::<i64>));
                self.phase.set(1);
                exec.fuel().interrupt();
                Ok(SequencePoll::Pending)
            }
            1 => {
                stack.push_back(3.into());
                self.phase.set(2);
                Ok(SequencePoll::Call {
                    function: self.function,
                    bottom: 2,
                })
            }
            2 => {
                assert!(!self.fails);
                let values = stack.consume::<Variadic<Vec<Option<i64>>>>(ctx)?.0;
                assert_eq!(values, [Some(7), None, Some(11), None, Some(13), None]);
                assert_eq!(self.table.get::<_, i64>(ctx, "n")?, 200);
                self.phase.set(3);
                exec.fuel().interrupt();
                Ok(SequencePoll::Pending)
            }
            3 => {
                stack.replace(ctx, (77, None::<i64>, 23, None::<i64>));
                self.phase.set(4);
                Ok(SequencePoll::Yield {
                    to_thread: None,
                    bottom: 2,
                })
            }
            4 => {
                let values = stack.consume::<Variadic<Vec<Option<i64>>>>(ctx)?.0;
                assert_eq!(values, [Some(77), None, Some(29), None]);
                stack.replace(ctx, (self.table, Value::Nil, self.marker, Value::Nil));
                self.phase.set(5);
                Ok(SequencePoll::Return)
            }
            phase => panic!("unexpected sequence phase {phase}"),
        }
    }

    fn error(
        self: Pin<&mut Self>,
        ctx: Context<'gc>,
        mut exec: Execution<'gc, '_>,
        error: Error<'gc>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        assert!(self.fails);
        assert_eq!(self.phase.get(), 2);
        match error {
            Error::Lua(error) => match error.0 {
                Value::Table(marker) => assert_eq!(marker, self.marker),
                value => panic!("unexpected Lua error value: {value:?}"),
            },
            error => panic!("unexpected sequence error: {error}"),
        }
        let values = stack.consume::<Variadic<Vec<Option<i64>>>>(ctx)?.0;
        assert_eq!(values, [Some(7), None]);
        assert_eq!(self.table.get::<_, i64>(ctx, "n")?, 200);
        self.phase.set(3);
        exec.fuel().interrupt();
        Ok(SequencePoll::Pending)
    }
}

type Slice = (bool, ExecutorMode, i32, bool, u8);

fn run(native: bool, fails: bool, budget: i32) -> Vec<Slice> {
    let mut lua = Lua::core();
    lua.set_jit_config(JitConfig {
        mode: if native { JitMode::Auto } else { JitMode::Off },
        hot_threshold: u32::MAX,
        ..Default::default()
    })
    .unwrap();
    let phase = Rc::new(Cell::new(0));
    lua.enter(|ctx| {
        let phase = phase.clone();
        ctx.set_global(
            "host_sequence",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let (function, table, marker): (Function, Table, Table) = stack.consume(ctx)?;
                Ok(CallbackReturn::Sequence(BoxSequence::new(
                    &ctx,
                    Continuation {
                        function,
                        table,
                        marker,
                        fails,
                        phase: phase.clone(),
                    },
                )))
            }),
        );
    });
    let executor = lua.enter(|ctx| {
        let closure = Closure::load(
            ctx,
            Some("native-sequence"),
            &br#"
                local fails=...
                local marker={}
                local t={n=0}
                local function callee(x)
                    assert(x==3)
                    for i=1,100 do t.n=t.n+1 end
                    if fails then error(marker) end
                    return 11,nil,13,nil
                end
                for i=1,100 do t.n=t.n+1 end
                local r=table.pack(host_sequence(callee,t,marker))
                assert(r.n==4 and r[1]==t and r[2]==nil and r[3]==marker and r[4]==nil)
                assert(t.host==1 and t.n==200)
                for i=1,100 do t.n=t.n+1 end
                return t.n,r.n,nil
            "#[..],
        )
        .unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), fails))
    });
    if native {
        while lua.prepare_jit().unwrap() != 0 {}
    }
    let installed = lua.jit_stats().installed_regions;
    let mut seen = [false; 3];
    let mut trace = Vec::new();
    let mut yielded = false;
    for _ in 0..2000 {
        let slice = lua.enter(|ctx| {
            let executor = ctx.fetch(&executor);
            let mut fuel = Fuel::with(budget);
            (
                executor.step(ctx, &mut fuel).unwrap(),
                executor.mode(),
                fuel.remaining(),
                fuel.is_interrupted(),
                phase.get(),
            )
        });
        trace.push(slice);
        lua.gc_collect();
        let stats = lua.jit_stats();
        assert_eq!(stats.installed_regions, installed);
        assert_eq!(stats.queued_requests, 0);
        assert_eq!(stats.native_panic_exits, 0);
        let checkpoint = match phase.get() {
            1 => Some((0, 101)),
            3 => Some((1, 201)),
            4 => Some((2, 201)),
            _ => None,
        };
        if let Some((index, writes)) = checkpoint {
            seen[index] = true;
            assert_eq!(stats.native_table_writes, if native { writes } else { 0 });
        }
        if !slice.0 {
            continue;
        }
        let values = lua
            .try_enter(|ctx| {
                Ok(ctx
                    .fetch(&executor)
                    .take_result::<Variadic<Vec<Option<i64>>>>(ctx)??
                    .0)
            })
            .unwrap();
        let mode = lua.enter(|ctx| ctx.fetch(&executor).mode());
        if phase.get() == 4 {
            assert!(!yielded);
            assert_eq!(values, [Some(23), None]);
            assert_eq!(mode, ExecutorMode::Suspended);
            yielded = true;
            lua.gc_collect();
            lua.enter(|ctx| ctx.fetch(&executor).resume(ctx, (29, None::<i64>)))
                .unwrap();
        } else {
            assert_eq!(phase.get(), 5);
            assert!(yielded && seen.into_iter().all(|value| value));
            assert_eq!(values, [Some(300), Some(4), None]);
            assert_eq!(mode, ExecutorMode::Stopped);
            assert_eq!(stats.native_table_writes, if native { 301 } else { 0 });
            assert_eq!(stats.native_instructions > 0, native);
            return trace;
        }
    }
    panic!("sequence did not complete");
}

#[test]
fn rust_sequences_preserve_native_calls_errors_pending_yield_and_resume() {
    for fails in [false, true] {
        for budget in [-1, 0, 1, 64, 65536] {
            assert_eq!(
                run(false, fails, budget),
                run(true, fails, budget),
                "fails={fails}, fuel={budget}"
            );
        }
    }
}
