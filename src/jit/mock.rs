use std::{cell::RefCell, rc::Rc};

use crate::{
    Callback, CallbackReturn, Closure, Executor, ExecutorMode, Fuel, Function, FunctionPrototype,
    Lua, Value, Variadic,
};

use super::{abi::Slot, ir::Snapshot, JitMode, Runtime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Before,
    After,
}

pub(super) struct Mock {
    mode: Mode,
    attempts: u64,
    completed: u64,
}

impl Runtime {
    pub(crate) fn mock_snapshot(&self, prototype: &FunctionPrototype<'_>) -> Option<Snapshot> {
        let manager = self.0.borrow();
        manager.mock.as_ref()?;
        assert_eq!(manager.config.mode, JitMode::Off);
        Some(Snapshot::new(prototype, 4096, 2 * 1024 * 1024).unwrap())
    }

    pub(crate) fn run_mock(
        &self,
        snapshot: &Snapshot,
        registers: &mut crate::thread::LuaRegisters<'_, '_>,
    ) -> u32 {
        let mode = {
            let mut manager = self.0.borrow_mut();
            let mock = manager.mock.as_mut().unwrap();
            mock.attempts += 1;
            assert!(mock.attempts < 100_000, "mock failed to make progress");
            mock.mode
        };
        if registers.stack_frame.len() < snapshot.registers {
            return 0;
        }
        let mut slots = [Slot::from_value(Value::Nil); 256];
        let slots = &mut slots[..snapshot.registers];
        for (slot, value) in slots.iter_mut().zip(registers.stack_frame.iter().copied()) {
            *slot = Slot::from_value(value);
        }
        let before = *registers.pc;
        let exit = super::model::run(snapshot, slots, before, u32::from(mode == Mode::After));
        assert!(exit.instructions <= 1);
        if exit.instructions == 0 {
            assert_eq!(exit.pc as usize, before);
            for (slot, value) in slots.iter().zip(registers.stack_frame.iter().copied()) {
                let original = Slot::from_value(value);
                assert_eq!((slot.tag, slot.bits), (original.tag, original.bits));
            }
        }
        for (slot, value) in slots.iter().copied().zip(registers.stack_frame.iter_mut()) {
            slot.write_back(value);
        }
        *registers.pc = exit.pc as usize;
        self.0.borrow_mut().mock.as_mut().unwrap().completed += u64::from(exit.instructions);
        exit.instructions
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Report {
    slices: Vec<(bool, ExecutorMode, i32, bool)>,
    events: Vec<i64>,
    result: Result<Vec<Option<i64>>, String>,
    dispatches: u64,
}

fn run(source: &str, mode: Option<Mode>, budget: i32) -> Report {
    let mut lua = Lua::core();
    let events = Rc::new(RefCell::new(Vec::new()));
    let executor = lua.enter(|ctx| {
        ctx.jit().0.borrow_mut().mock = mode.map(|mode| Mock {
            mode,
            attempts: 0,
            completed: 0,
        });
        let events = events.clone();
        ctx.set_global(
            "mark",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                events.borrow_mut().push(stack.consume::<i64>(ctx)?);
                Ok(CallbackReturn::Return)
            }),
        );
        ctx.set_global(
            "observe",
            Callback::from_fn(&ctx, |ctx, _, mut stack| {
                let function: Function = stack.consume(ctx)?;
                let executor = Executor::start(ctx, function, ());
                for _ in 0..100 {
                    if executor.step(ctx, &mut Fuel::with(64)).unwrap() {
                        let value = executor.take_result::<i64>(ctx).unwrap()?;
                        stack.replace(ctx, value);
                        return Ok(CallbackReturn::Return);
                    }
                }
                panic!("nested mock executor did not complete");
            }),
        );
        let closure = Closure::load(ctx, Some("mock-boundary"), source.as_bytes()).unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    let mut slices = Vec::new();
    for _ in 0..2000 {
        let slice = lua.enter(|ctx| {
            let executor = ctx.fetch(&executor);
            let mut fuel = Fuel::with(budget);
            (
                executor.step(ctx, &mut fuel).unwrap(),
                executor.mode(),
                fuel.remaining(),
                fuel.is_interrupted(),
            )
        });
        slices.push(slice);
        lua.gc_collect();
        let stats = lua.jit_stats();
        assert_eq!(stats.native_entries, 0);
        assert_eq!(stats.native_instructions, 0);
        assert_eq!(stats.compilation_requests, 0);
        assert_eq!(stats.installed_regions, 0);
        assert_eq!((stats.code_bytes, stats.code_requested_bytes), (0, 0));
        if slice.0 {
            let result = lua
                .try_enter(|ctx| {
                    ctx.fetch(&executor)
                        .take_result::<Variadic<Vec<Option<i64>>>>(ctx)?
                })
                .map(|values| values.0)
                .map_err(|error| error.to_string());
            lua.enter(|ctx| {
                let manager = ctx.jit().0.borrow();
                if let Some(mock) = &manager.mock {
                    assert!(mock.attempts > 0);
                    assert_eq!(mock.attempts, stats.total_dispatches);
                    assert_eq!(mock.completed > 0, mock.mode == Mode::After);
                    assert!(mock.completed < mock.attempts);
                }
            });
            return Report {
                slices,
                events: events.borrow().clone(),
                result,
                dispatches: stats.total_dispatches,
            };
        }
    }
    panic!("mock executor did not complete");
}

#[test]
fn before_and_after_scalar_exits_preserve_canonical_execution_without_native_code() {
    let cases = [
        r#"
            local function inner(...)
                local args=table.pack(...)
                local x=args[1]
                local function read() return x end
                for i=1,4 do x=x+i mark(observe(read)) end
                return x,nil,table.unpack(args,1,args.n)
            end
            local r=table.pack(inner(10,nil,30,nil))
            assert(r.n==6 and r[1]==20 and r[3]==10 and r[5]==30)
            assert(r[2]==nil and r[4]==nil and r[6]==nil)
            return r[1],r.n,nil
        "#,
        r#"
            local n=0
            local co=coroutine.create(function()
                local r <close> = setmetatable({},{__close=function() mark(n) end})
                n=n+1
                local a,b=coroutine.yield(n,nil)
                assert(a==7 and b==nil)
                n=n+a
                return n,nil
            end)
            local x=table.pack(coroutine.resume(co))
            assert(x.n==3 and x[1] and x[2]==1 and x[3]==nil)
            local y=table.pack(coroutine.resume(co,7,nil))
            assert(y.n==3 and y[1] and y[2]==8 and y[3]==nil)
            return y[2],y.n,nil
        "#,
        r#"
            local marker={}
            local n=0
            local function work()
                local r <close> = setmetatable({},{__close=function() mark(n) end})
                n=9
                error(marker)
            end
            local ok,err=pcall(work)
            assert(not ok and err==marker)
            return n,nil
        "#,
        r#"
            local x=0
            for i=1,5 do x=x+i end
            mark(x)
            return x//0
        "#,
    ];
    for (index, source) in cases.into_iter().enumerate() {
        for budget in [-1, 0, 1, 64, 65536] {
            let expected = run(source, None, budget);
            let (values, events) = match index {
                0 => (vec![Some(20), Some(6), None], vec![11, 13, 16, 20]),
                1 => (vec![Some(8), Some(3), None], vec![8]),
                2 => (vec![Some(9), None], vec![9]),
                _ => (vec![], vec![15]),
            };
            assert_eq!(expected.events, events);
            if index == 3 {
                assert!(expected
                    .result
                    .as_ref()
                    .unwrap_err()
                    .contains("mock-boundary:5"));
            } else {
                assert_eq!(expected.result, Ok(values));
            }
            for mode in [Mode::Before, Mode::After] {
                assert_eq!(
                    run(source, Some(mode), budget),
                    expected,
                    "case={index}, fuel={budget}, mode={mode:?}"
                );
            }
        }
    }
}
