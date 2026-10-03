#![cfg(all(
    feature = "jit",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use luna::{Closure, Context, Executor, ExecutorMode, Fuel, JitConfig, JitMode, Lua, Value};

#[derive(Clone, Copy, Debug)]
enum Input {
    Integer(i64),
    Number(f64),
    Text(&'static str),
}

impl Input {
    fn value(self, ctx: Context<'_>) -> Value<'_> {
        match self {
            Self::Integer(value) => Value::Integer(value),
            Self::Number(value) => Value::Number(value),
            Self::Text(value) => ctx.intern(value.as_bytes()).into(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Answer {
    Integer(i64),
    Number(u64),
    Error(String),
}

fn number(value: f64) -> Answer {
    Answer::Number(if value.is_nan() {
        f64::NAN.to_bits()
    } else {
        value.to_bits()
    })
}

fn run(
    operator: &str,
    left: Input,
    right: Input,
    native: bool,
    budget: i32,
) -> (Answer, Vec<(bool, ExecutorMode, i32, bool)>) {
    let mut lua = Lua::core();
    lua.set_jit_config(JitConfig {
        mode: if native { JitMode::Auto } else { JitMode::Off },
        hot_threshold: u32::MAX,
        ..Default::default()
    })
    .unwrap();
    let source = format!("local a,b=... local t={{n=0}} for i=1,100 do t.n=t.n+1 end local ok,r=pcall(function() return a {operator} b end) for i=1,100 do t.n=t.n+1 end if not ok then r=tostring(r) end return ok,r,t.n");
    let executor = lua.enter(|ctx| {
        let closure = Closure::load(ctx, Some("numeric-exits"), source.as_bytes()).unwrap();
        ctx.stash(Executor::start(
            ctx,
            closure.into(),
            (left.value(ctx), right.value(ctx)),
        ))
    });
    if native {
        while lua.prepare_jit().unwrap() != 0 {}
    }
    let installed = lua.jit_stats().installed_regions;
    let mut slices = Vec::new();
    for _ in 0..1000 {
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
        assert_eq!(lua.jit_stats().installed_regions, installed);
        assert_eq!(lua.jit_stats().queued_requests, 0);
        if slice.0 {
            let answer = lua
                .try_enter(|ctx| {
                    let (ok, value, writes) = ctx
                        .fetch(&executor)
                        .take_result::<(bool, Value, i64)>(ctx)??;
                    assert_eq!(writes, 200);
                    Ok(match (ok, value) {
                        (true, Value::Integer(value)) => Answer::Integer(value),
                        (true, Value::Number(value)) => number(value),
                        (false, Value::String(value)) => {
                            Answer::Error(String::from_utf8(value.as_bytes().to_vec()).unwrap())
                        }
                        value => panic!("unexpected numeric result: {value:?}"),
                    })
                })
                .unwrap();
            let stats = lua.jit_stats();
            if native {
                assert!(stats.native_instructions > 0);
                assert_eq!(stats.native_table_writes, 201);
                assert!(stats.native_interpreter_exits > 0);
                assert_eq!(stats.native_panic_exits, 0);
            } else {
                assert_eq!(stats.native_entries, 0);
            }
            return (answer, slices);
        }
    }
    panic!("numeric execution did not finish");
}

#[test]
fn numeric_edges_preserve_native_prefix_suffix_results_and_exact_slices() {
    use Input::{Integer as I, Number as N, Text as S};
    let cases = [
        ("+", I(i64::MAX), I(1), Some(Answer::Integer(i64::MIN))),
        ("-", I(i64::MIN), I(1), Some(Answer::Integer(i64::MAX))),
        ("*", I(i64::MAX), I(2), Some(Answer::Integer(-2))),
        ("//", I(i64::MIN), I(-1), Some(Answer::Integer(i64::MIN))),
        ("%", I(i64::MIN), I(-1), Some(Answer::Integer(0))),
        ("//", I(-7), I(3), Some(Answer::Integer(-3))),
        ("//", I(7), I(-3), Some(Answer::Integer(-3))),
        ("%", I(-7), I(3), Some(Answer::Integer(2))),
        ("%", I(7), I(-3), Some(Answer::Integer(-2))),
        ("//", I(1), I(0), None),
        ("%", I(1), I(0), None),
        ("/", I(1), I(2), Some(number(0.5))),
        ("/", I(1), I(0), Some(number(f64::INFINITY))),
        ("/", I(-1), I(0), Some(number(f64::NEG_INFINITY))),
        ("/", I(1), N(-0.0), Some(number(f64::NEG_INFINITY))),
        ("/", N(-0.0), I(2), Some(number(-0.0))),
        ("/", I(0), I(0), Some(number(f64::NAN))),
        (
            "/",
            N(f64::INFINITY),
            N(f64::INFINITY),
            Some(number(f64::NAN)),
        ),
        ("%", N(5.0), N(f64::INFINITY), Some(number(5.0))),
        ("%", N(-5.0), N(f64::INFINITY), Some(number(f64::INFINITY))),
        ("%", N(5.5), N(-2.0), Some(number(-0.5))),
        ("+", S("20"), I(2), Some(Answer::Integer(22))),
        ("+", S("3.5"), I(1), Some(number(4.5))),
        ("+", S("not a number"), I(1), None),
    ];
    for (operator, left, right, expected) in cases {
        for budget in [-1, 0, 1, 64, 65536] {
            let reference = run(operator, left, right, false, budget);
            let native = run(operator, left, right, true, budget);
            assert_eq!(
                native, reference,
                "{left:?} {operator} {right:?}, fuel={budget}"
            );
            if let Some(expected) = &expected {
                assert_eq!(&native.0, expected, "{left:?} {operator} {right:?}");
            } else {
                assert!(matches!(native.0, Answer::Error(ref message) if !message.is_empty()));
            }
        }
    }
}
