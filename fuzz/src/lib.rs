use luna::{
    Closure, Context, Executor, FromValue, Fuel, JitConfig, JitMode, Lua, StashedExecutor,
    TypeError, Value, Variadic,
};

const LITERALS: [&str; 13] = [
    "0",
    "1",
    "-1",
    "0x7fffffffffffffff",
    "(-0x7fffffffffffffff-1)",
    "0x8000000000000000",
    "1.5",
    "-0.0",
    "math.huge",
    "(-math.huge)",
    "(0/0)",
    "false",
    "nil",
];

fn byte(data: &[u8], index: usize) -> u8 {
    data.get(index).copied().unwrap_or(0)
}

fn literal(value: u8) -> &'static str {
    LITERALS[usize::from(value) % LITERALS.len()]
}

pub fn scalar(data: &[u8]) -> String {
    let mut source = format!(
        "local a={} local b={} local c={} local q=true for i=1,{} do\n",
        literal(byte(data, 0)),
        literal(byte(data, 1)),
        literal(byte(data, 2)),
        1 + byte(data, 3) % 8
    );
    for chunk in data.get(4..).unwrap_or_default().chunks(3).take(16) {
        let value = literal(byte(chunk, 1));
        let shift = i32::from(byte(chunk, 2)) - 128;
        let body = match byte(chunk, 0) % 20 {
            0 => "a=a+b".into(),
            1 => "b=b-c".into(),
            2 => "c=a*b".into(),
            3 => "a=a/(b+1)".into(),
            4 => "a=a//b".into(),
            5 => "a=a%b".into(),
            6 => format!("c=a^{}", byte(chunk, 2) % 5),
            7 => "a=a&b".into(),
            8 => format!("b=b<<({shift})"),
            9 => format!("c=c>>({shift})"),
            10 => "q=a<b".into(),
            11 => "if a==b then c=b else c=a end".into(),
            12 => "a=-a".into(),
            13 => "b=not b".into(),
            14 => format!("a={value}"),
            15 => "local t=a a=b b=c c=t".into(),
            16 => format!("if a then a=c else b={value} end"),
            17 => "q=a<=c".into(),
            18 => "c=~c".into(),
            _ => "q=not a".into(),
        };
        source.push_str(&format!(
            "local ok=pcall(function() {body} end) if not ok then q=false end\n"
        ));
    }
    source.push_str("end return a,b,c,q");
    source
}

pub fn heap(data: &[u8]) -> String {
    let value = literal(byte(data, 0));
    let key = 1 + byte(data, 1) % 4;
    let loops = 1 + byte(data, 2) % 8;
    let body = match byte(data, 3) % 4 {
        0 => format!("x={value} alias=x return read()"),
        1 => format!("alias={{[{key}]=42}} assert(x==alias) return read()[{key}]"),
        2 => format!("x=false return alias[{key}]"),
        _ => format!("x=false alias[{key}]=42 return 0"),
    };
    let operations = data.get(4..).unwrap_or_default().chunks(2).take(16).map(|chunk| {
        let key = 1 + byte(chunk, 1) % 4;
        match byte(chunk, 0) % 8 {
            0 => format!("t[{key}]=i+{key}"),
            1 => format!("t[{key}]={{value=i}} assert(t[{key}].value==i)"),
            2 => "local a=t assert(a==t) t.self=a assert(t.self==t)".into(),
            3 => format!("closed({key})"),
            4 => "local p=setmetatable({}, {__index=t}) assert(p.self==t.self)".into(),
            5 => "local ok=pcall(function() t[nil]=i end) assert(not ok)".into(),
            6 => "local w=setmetatable({}, {__mode='v'}) w[1]=t assert(w[1]==t)".into(),
            _ => "local c=coroutine.create(function() coroutine.yield(i) return i end) local ok,v=coroutine.resume(c) assert(ok and v==i) ok,v=coroutine.resume(c) assert(ok and v==i)".into(),
        }
    }).collect::<Vec<_>>().join("\n");
    format!(
        r#"
        local t={{}} local n=0
        local function closed(v) n=n+v end
        for i=1,{loops} do {operations} end
        local alias=0
        local anchor=function() return alias end
        local wanted=debug.upvalueid(anchor,1)
        local f
        f=function()
            local x={{[{key}]=0}}
            local read=function() return x end
            local joined=false
            for j=1,10 do
                if debug.getupvalue(f,j) and debug.upvalueid(f,j)==wanted then
                    debug.upvaluejoin(f,j,read,1) joined=true break
                end
            end
            assert(joined)
            {body}
        end
        local ok,result=pcall(f)
        return n,ok,ok and result or -1,t.self==t
    "#
    )
}

#[derive(Debug, PartialEq, Eq)]
enum Primitive {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(u64),
    Nan,
    Bytes(Vec<u8>),
}

impl<'gc> FromValue<'gc> for Primitive {
    fn from_value(_: Context<'gc>, value: Value<'gc>) -> Result<Self, TypeError> {
        Ok(match value {
            Value::Nil => Self::Nil,
            Value::Boolean(value) => Self::Boolean(value),
            Value::Integer(value) => Self::Integer(value),
            Value::Number(value) if value.is_nan() => Self::Nan,
            Value::Number(value) => Self::Number(value.to_bits()),
            Value::String(value) => Self::Bytes(value.as_bytes().to_vec()),
            _ => panic!("nonprimitive generated-program result"),
        })
    }
}

fn state(source: &str, native: bool, debug: bool) -> (Lua, StashedExecutor) {
    let mut lua = Lua::core();
    if debug {
        lua.load_debug();
    }
    if native {
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            hot_threshold: 1,
            ..Default::default()
        })
        .unwrap();
    }
    let executor = lua.enter(|ctx| {
        let closure = Closure::load(ctx, Some("coverage-guided"), source.as_bytes()).unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    if native {
        assert!(lua.prepare_jit().unwrap() > 0);
    }
    (lua, executor)
}

pub fn check(source: &str, data: &[u8], debug: bool) {
    assert!(source.len() <= 16 * 1024);
    let (mut reference, left) = state(source, false, debug);
    let (mut native, right) = state(source, true, debug);
    let retire = usize::from(byte(data, 0) % 4);
    let mut finished = false;
    for slice in 0..1024 {
        let budget = [0, 1, 7, 63, 64, 128][usize::from(byte(data, slice % 16)) % 6];
        let step = |lua: &mut Lua, executor: &StashedExecutor| {
            lua.enter(|ctx| {
                let executor = ctx.fetch(executor);
                let mut fuel = Fuel::with(budget);
                (
                    executor.step(ctx, &mut fuel).unwrap(),
                    executor.mode(),
                    fuel.remaining(),
                )
            })
        };
        let expected = step(&mut reference, &left);
        assert_eq!(
            step(&mut native, &right),
            expected,
            "slice={slice}\n{source}"
        );
        reference.gc_collect();
        native.gc_collect();
        if expected.0 {
            finished = true;
            break;
        }
        if slice == retire {
            native.clear_jit_cache();
            assert_eq!(native.jit_stats().code_bytes, 0);
            assert!(native.prepare_jit().unwrap() > 0);
        }
    }
    assert!(finished, "slice limit exceeded\n{source}");
    let expected: Variadic<Vec<Primitive>> = reference.execute(&left).unwrap();
    let actual: Variadic<Vec<Primitive>> = native.execute(&right).unwrap();
    assert_eq!(actual.0, expected.0, "{source}");
    let stats = native.jit_stats();
    assert!(
        stats.native_instructions > 0,
        "zero native execution\n{source}"
    );
    if debug && byte(data, 3) % 4 < 2 {
        assert!(
            stats.native_upvalue_writes > 0,
            "zero native alias writes\n{source}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_boundaries_and_every_statement_have_paired_native_execution() {
        for operation in 0..20 {
            for value in 0..LITERALS.len() as u8 {
                let data = [value, 1, 6, 0, operation, value, 64];
                check(&scalar(&data), &data, false);
            }
        }
    }

    #[test]
    fn heap_aliases_and_all_heap_statements_have_paired_native_execution() {
        for alias in 0..4 {
            for operation in 0..8 {
                let data = [6, 1, 0, alias, operation, 2];
                check(&heap(&data), &data, true);
            }
        }
    }

    #[test]
    fn input_and_source_expansion_are_bounded() {
        for data in [vec![], vec![0; 4096], vec![255; 4096]] {
            assert!(scalar(&data).len() < 16 * 1024);
            assert!(heap(&data).len() < 16 * 1024);
        }
    }
}
