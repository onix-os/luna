use std::{cell::RefCell, rc::Rc};

use crate::{
    Callback, CallbackReturn, Closure, Executor, ExecutorMode, Fuel, JitConfig, JitMode, JitStats,
    Lua, StashedExecutor, Table, Value,
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
        _ => unreachable!(),
    };
    format!(
        "local iterations={iterations} local stride={stride} local salt={salt}\n\
         shared={{value=0,count=0}} local total=0 local n=0\n\
         local function update(v) n=n+v shared.value=n shared.count=shared.count+1 return n end\n\
         {body}\nreturn total,n,shared.count==iterations"
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
        let closure = Closure::load(ctx, Some("heap-fuzz"), script.as_bytes()).unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    lua.prepare_jit().unwrap();
    (lua, executor, journal)
}

fn observe(lua: &mut Lua) -> (i64, i64) {
    lua.enter(|ctx| match ctx.get_global_value("shared") {
        Value::Table(table) => (
            table.get(ctx, "value").unwrap(),
            table.get(ctx, "count").unwrap(),
        ),
        Value::Nil => (0, 0),
        _ => panic!("shared global lost its table identity"),
    })
}

fn mutate(lua: &mut Lua, slice: usize) {
    lua.enter(|ctx| {
        if let Value::Table(table) = ctx.get_global_value("shared") {
            let object = Table::new(&ctx);
            object.set_field(ctx, "slice", slice as i64);
            table.set_field(ctx, "host", object);
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
    let family = case % 4;
    let script = program(random, family);
    let retire_at = 3 + random.index(12);
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
        assert_eq!(observe(&mut native), observe(&mut reference));
        counts.slices += 1;
        for lua in [&mut reference, &mut native] {
            mutate(lua, slice);
            lua.gc_collect();
        }
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
        for case in 0..4 {
            let counts = run(&mut random, seed, case);
            assert!(counts.callbacks > 0);
        }
    }
}
