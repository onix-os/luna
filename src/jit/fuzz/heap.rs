use std::{cell::RefCell, rc::Rc};

use crate::{
    Callback, CallbackReturn, Closure, Context, Executor, ExecutorMode, Fuel, Function, JitConfig,
    JitMode, JitStats, Lua, StashedExecutor, Table, UserData, Value,
};

use super::Random;

type Journal = Rc<RefCell<Vec<(i64, i64)>>>;

#[derive(Default)]
pub(super) struct Counts {
    pub cases: usize,
    pub slices: usize,
    pub yields: usize,
    pub callbacks: usize,
    pub retirements: usize,
    pub host_reads: usize,
    pub userdata_observations: usize,
    pub instructions: u64,
    pub reads: u64,
    pub writes: u64,
    pub allocations: u64,
    pub upvalue_reads: u64,
    pub upvalue_writes: u64,
    pub declines: u64,
}

impl Counts {
    pub fn add(&mut self, other: Self) {
        self.cases += other.cases;
        self.slices += other.slices;
        self.yields += other.yields;
        self.callbacks += other.callbacks;
        self.retirements += other.retirements;
        self.host_reads += other.host_reads;
        self.userdata_observations += other.userdata_observations;
        self.instructions += other.instructions;
        self.reads += other.reads;
        self.writes += other.writes;
        self.allocations += other.allocations;
        self.upvalue_reads += other.upvalue_reads;
        self.upvalue_writes += other.upvalue_writes;
        self.declines += other.declines;
    }
}

fn program(random: &mut Random, family: usize) -> String {
    let iterations = 5 + random.index(12);
    let stride = 2 + random.index(3);
    let salt = 1 + random.index(31);
    let body = match family {
        0 => {
            r#"
            local alias=shared
            local function make()
                local cells={}
                return function(i)
                    cells[i]={value=i+salt}
                    return cells[i]
                end
            end
            local closed=make()
            for i=1,iterations do
                local item=closed(i)
                alias[i]=item
                item.self=item
                total=total+update(item.value)+tick(item,i)
                assert(alias[i]==item and item.self==item)
                if i%stride==0 then coroutine.yield(i,total,true) end
            end
        "#
        }
        1 => {
            r#"
            local backing={value=0}
            local proxy=setmetatable({}, {
                __index=function(_,k) return backing[k] end,
                __newindex=function(_,k,v)
                    backing[k]=v
                    tick(backing,-v)
                end
            })
            for i=1,iterations do
                proxy.value=i+salt
                total=total+update(proxy.value)+tick(backing,i)
                assert(proxy.value==backing.value)
                if i%stride==0 then coroutine.yield(i,total,true) end
            end
        "#
        }
        2 => {
            r#"
            local weak=setmetatable({}, {__mode='v'})
            local keys=setmetatable({}, {__mode='k'})
            local live={value=salt}
            weak[1]=live
            for i=1,iterations do
                do
                    local dead={value=i}
                    weak[2]=dead
                    keys[dead]=i
                end
                collectgarbage('collect')
                coroutine.yield(i,total,true)
                assert(weak[1]==live and weak[2]==nil and next(keys)==nil)
                total=total+update(weak[1].value)+tick(live,i)
            end
        "#
        }
        3 => {
            r#"
            local events={value=0}
            local mt={__close=function(t,err)
                events.value=events.value+t.value
                tick(events,err and -t.value or t.value)
            end}
            for i=1,iterations do
                local ok=pcall(function()
                    local outer <close> = setmetatable({value=i},mt)
                    local inner <close> = setmetatable({value=i+salt},mt)
                    local t={value=i}
                    tick(t,i)
                    t[nil]=i
                end)
                assert(not ok)
                do local normal <close> = setmetatable({value=i},mt) end
                total=total+update(events.value)
                if i%stride==0 then coroutine.yield(i,total,true) end
            end
        "#
        }
        4 => {
            r#"
            local key={}
            local mode={__mode='k'}
            local cache=setmetatable({},mode)
            local function store(v) cache[key]={value=v} end
            local function read() return cache[key].value end
            for i=1,iterations do
                store(i+salt)
                collectgarbage('collect')
                collectgarbage('collect')
                assert(read()==i+salt,'ephemeron lost its value')
                mode.__mode=i%2==0 and 'kv' or 'v'
                collectgarbage('collect')
                collectgarbage('collect')
                coroutine.yield(i,total,true)
                assert(read()==i+salt,'mode changed without reattachment')
                total=total+update(read())
                setmetatable(cache,mode)
                collectgarbage('collect')
                collectgarbage('collect')
                coroutine.yield(i,total,true)
                assert(cache[key]==nil,'reattached weak mode retained dead value')
                mode.__mode='k'
                setmetatable(cache,mode)
                tick({value=i+salt},i)
            end
        "#
        }
        5 => {
            r#"
            local saved local ran=0
            local function release(v)
                local object=setmetatable({value=v}, {
                    __gc=function(self)
                        ran=ran+1
                        tick(self,-self.value)
                        self.value=salt+ran
                        saved=self
                    end
                })
            end
            release(salt)
            collectgarbage('collect')
            coroutine.yield(0,total,true)
            collectgarbage('collect')
            assert(saved and ran==1,'finalizer did not resurrect exactly once')
            for i=1,iterations do
                total=total+nested(function()
                    local item={value=saved.value+i}
                    tick(item,i)
                    return update(item.value)
                end)
                if i%stride==0 then coroutine.yield(i,total,true) end
            end
            saved=nil
            collectgarbage('collect')
            coroutine.yield(iterations,total,true)
            collectgarbage('collect')
            assert(ran==1,'resurrected object finalized twice')
        "#
        }
        _ => unreachable!(),
    };
    format!(
        "local iterations={iterations} local stride={stride} local salt={salt}\n\
         local total=0 local n=0\n\
         local function update(v) local item=shared.host v=v+item.slice shared.last=item shared.reads=shared.reads+1 n=n+v shared.value=n shared.count=shared.count+1 return n end\n\
         {body}\nreturn total,n,shared.count==iterations and shared.reads==iterations"
    )
}

fn state(native: bool, script: &str) -> (Lua, StashedExecutor, Journal) {
    let mut lua = Lua::core();
    if native {
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            hot_threshold: 1,
            max_queue_entries: 64,
            ..Default::default()
        })
        .unwrap();
    }
    let journal = Journal::default();
    let callback_journal = journal.clone();
    let executor = lua.enter(|ctx| {
        let tick = Callback::from_fn(&ctx, move |ctx, _, mut stack| {
            let (table, kind): (Table, i64) = stack.consume(ctx)?;
            let before: i64 = table.get(ctx, "value")?;
            callback_journal.borrow_mut().push((kind, before));
            table.set_field(ctx, "value", before + kind);
            stack.replace(ctx, before);
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("tick", tick);
        let nested = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let function: Function = stack.consume(ctx)?;
            let executor = Executor::start(ctx, function, ());
            let mut finished = false;
            for _ in 0..64 {
                if executor.step(ctx, &mut Fuel::with(128)).unwrap() {
                    finished = true;
                    break;
                }
            }
            assert!(finished, "bounded nested callback exceeded its slice limit");
            let value = executor.take_result::<i64>(ctx).unwrap()?;
            stack.replace(ctx, value);
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("nested", nested);
        ctx.set_global(
            "warn",
            Callback::from_fn(&ctx, |_, _, _| {
                panic!("unexpected heap campaign finalizer warning")
            }),
        );
        let shared = Table::new(&ctx);
        shared.set_field(ctx, "value", 0);
        shared.set_field(ctx, "count", 0);
        shared.set_field(ctx, "reads", 0);
        ctx.set_global("shared", shared);
        let closure = Closure::load(ctx, Some("heap-fuzz"), script.as_bytes()).unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    mutate(&mut lua, 0);
    lua.prepare_jit().unwrap();
    (lua, executor, journal)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Observation {
    value: i64,
    count: i64,
    reads: usize,
    host: Option<(bool, i64, i64)>,
    last: Option<(bool, i64, i64)>,
}

fn object<'gc>(ctx: Context<'gc>, value: Value<'gc>) -> Option<(bool, i64, i64)> {
    match value {
        Value::Nil => None,
        Value::Table(table) => Some((
            false,
            table.get(ctx, "id").unwrap(),
            table.get(ctx, "slice").unwrap(),
        )),
        Value::UserData(data) => {
            let proxy = match data.metatable().unwrap().get_value(ctx, "__index") {
                Value::Table(table) => table,
                _ => panic!("userdata proxy lost its table"),
            };
            Some((
                true,
                *data.downcast_static::<i64>().unwrap(),
                proxy.get(ctx, "slice").unwrap(),
            ))
        }
        _ => panic!("unexpected host object kind"),
    }
}

fn observe(lua: &mut Lua) -> Observation {
    lua.enter(|ctx| match ctx.get_global_value("shared") {
        Value::Table(table) => Observation {
            value: table.get(ctx, "value").unwrap(),
            count: table.get(ctx, "count").unwrap(),
            reads: table.get(ctx, "reads").unwrap(),
            host: object(ctx, table.get_value(ctx, "host")),
            last: object(ctx, table.get_value(ctx, "last")),
        },
        Value::Nil => Observation::default(),
        _ => panic!("shared global lost its table identity"),
    })
}

fn mutate(lua: &mut Lua, slice: usize) {
    lua.enter(|ctx| {
        if let Value::Table(table) = ctx.get_global_value("shared") {
            for key in ["host", "last"] {
                let proxy = match table.get_value(ctx, key) {
                    Value::Table(object) => Some(object),
                    Value::UserData(data) => {
                        match data.metatable().unwrap().get_value(ctx, "__index") {
                            Value::Table(object) => Some(object),
                            _ => panic!("userdata proxy lost its table"),
                        }
                    }
                    Value::Nil => None,
                    _ => panic!("unexpected retained host object kind"),
                };
                if let Some(proxy) = proxy {
                    proxy.set_field(ctx, "slice", (slice + 1000) as i64);
                }
            }
            let object = Table::new(&ctx);
            object.set_field(ctx, "id", slice as i64);
            object.set_field(ctx, "slice", (slice % 17 + 1) as i64);
            if slice.is_multiple_of(2) {
                table.set_field(ctx, "host", object);
            } else {
                let data = UserData::new_static(&ctx, slice as i64);
                let mt = Table::new(&ctx);
                mt.set_field(ctx, "__index", object);
                data.set_metatable(ctx, Some(mt));
                table.set_field(ctx, "host", data);
            }
        }
    });
}

fn step(lua: &mut Lua, executor: &StashedExecutor, budget: i32) -> (bool, ExecutorMode, i32) {
    lua.enter(|ctx| {
        let executor = ctx.fetch(executor);
        let mut fuel = Fuel::with(budget);
        let finished = executor.step(ctx, &mut fuel).unwrap();
        (finished, executor.mode(), fuel.remaining())
    })
}

fn result(lua: &mut Lua, executor: &StashedExecutor) -> ((i64, i64, bool), ExecutorMode) {
    lua.enter(|ctx| {
        let executor = ctx.fetch(executor);
        let value = executor
            .take_result::<(i64, i64, bool)>(ctx)
            .unwrap()
            .unwrap();
        (value, executor.mode())
    })
}

fn assert_native(stats: JitStats) {
    assert!(stats.native_entries > 0 && stats.native_instructions > 0);
    assert!(stats.native_allocations > 0);
    assert!(stats.native_table_reads > 0 && stats.native_table_writes > 0);
    assert!(stats.native_upvalue_reads > 0 && stats.native_upvalue_writes > 0);
}

pub(super) fn run(random: &mut Random, seed: u64, case: usize) -> Counts {
    let family = case % 6;
    eprintln!("heap seed={seed} case={case} family={family}");
    let script = program(random, family);
    let retire_at = 1 + random.index(3);
    let (mut reference, left, left_journal) = state(false, &script);
    let (mut native, right, right_journal) = state(true, &script);
    let mut counts = Counts::default();
    let mut finished = false;
    for slice in 0..4096 {
        let budget = [0, 1, 7, 63, 64, 128][random.index(6)];
        let expected = step(&mut reference, &left, budget);
        let actual = step(&mut native, &right, budget);
        assert_eq!(
            actual, expected,
            "seed={seed} case={case} slice={slice} script={script}"
        );
        assert_eq!(
            *right_journal.borrow(),
            *left_journal.borrow(),
            "seed={seed} case={case} slice={slice}: callback effects"
        );
        let actual_state = observe(&mut native);
        assert_eq!(actual_state, observe(&mut reference));
        counts.userdata_observations += usize::from(actual_state.last.is_some_and(|item| item.0));
        counts.slices += 1;
        for lua in [&mut reference, &mut native] {
            mutate(lua, slice);
            lua.gc_collect();
            lua.run_finalizers();
        }
        assert_eq!(
            *right_journal.borrow(),
            *left_journal.borrow(),
            "seed={seed} case={case} slice={slice}: post-GC callback effects"
        );
        assert_eq!(observe(&mut native), observe(&mut reference));
        if slice == retire_at {
            native.clear_jit_cache();
            assert_eq!(native.jit_stats().code_bytes, 0);
            assert!(native.prepare_jit().unwrap() > 0);
            counts.retirements += 1;
        }
        if actual.1 == ExecutorMode::Result {
            let expected = result(&mut reference, &left);
            let actual = result(&mut native, &right);
            assert_eq!(actual, expected, "seed={seed} case={case}: result/yield");
            assert!(actual.0 .2);
            match actual.1 {
                ExecutorMode::Suspended => {
                    counts.yields += 1;
                    reference.enter(|ctx| ctx.fetch(&left).resume(ctx, ()).unwrap());
                    native.enter(|ctx| ctx.fetch(&right).resume(ctx, ()).unwrap());
                }
                ExecutorMode::Stopped => {
                    finished = true;
                    break;
                }
                _ => panic!("unexpected post-result mode"),
            }
        }
    }
    assert!(finished, "seed={seed} case={case}: bounded slice limit");
    assert!(counts.yields > 0);
    assert_eq!(counts.retirements, 1);
    assert_eq!(reference.jit_stats().native_entries, 0);
    let stats = native.jit_stats();
    assert_native(stats);
    if family == 1 || family == 3 {
        assert!(stats.helper_declines > 0);
    }
    counts.cases = 1;
    counts.callbacks = right_journal.borrow().len();
    counts.host_reads = observe(&mut native).reads;
    assert!(counts.host_reads > 0);
    counts.instructions = stats.native_instructions;
    counts.reads = stats.native_table_reads;
    counts.writes = stats.native_table_writes;
    counts.allocations = stats.native_allocations;
    counts.upvalue_reads = stats.native_upvalue_reads;
    counts.upvalue_writes = stats.native_upvalue_writes;
    counts.declines = stats.helper_declines;
    drop(left);
    drop(right);
    for lua in [&mut reference, &mut native] {
        lua.clear_jit_cache();
        lua.gc_collect();
        lua.run_finalizers();
        let stats = lua.jit_stats();
        assert_eq!(stats.code_bytes, 0);
        assert_eq!(stats.snapshot_bytes, 0);
        assert_eq!(stats.queued_requests, 0);
    }
    counts
}

#[test]
fn families_execute_native_helpers_and_match_every_slice() {
    for seed in [0, 1, u64::MAX] {
        let mut random = Random(seed);
        let mut observed = 0;
        for case in 0..6 {
            let counts = run(&mut random, seed, case);
            assert!(counts.callbacks > 0);
            observed += counts.userdata_observations;
        }
        assert!(observed > 0);
    }
}
