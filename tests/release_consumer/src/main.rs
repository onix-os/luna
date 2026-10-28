use luna::{
    async_sequence, Callback, CallbackReturn, Closure, Executor, FromValue, IntoValue, Lua,
    SequenceReturn,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll, Wake, Waker},
};

#[derive(luna::FromValue, luna::IntoValue, Debug, PartialEq)]
struct Point {
    x: i64,
    y: i64,
}

struct Counter(AtomicUsize);
impl Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct Delayed {
    remaining: usize,
}
impl Future for Delayed {
    type Output = i64;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<i64> {
        if self.remaining == 0 {
            Poll::Ready(42)
        } else {
            self.remaining -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}
fn block_on<F: Future>(future: F) -> (F::Output, usize) {
    let counter = Arc::new(Counter(AtomicUsize::new(0)));
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    for _ in 0..10000 {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return (value, counter.0.load(Ordering::SeqCst));
        }
    }
    panic!("async probe exceeded poll limit");
}

fn run(name: &str, script: &str, expected: i64, auto: bool, asynchronous: bool) {
    let mut lua = Lua::core();
    #[cfg(feature = "jit")]
    {
        assert_eq!(lua.jit_config().mode, luna::JitMode::Off);
        assert!(lua.jit_capabilities().supported_target);
        if auto {
            lua.set_jit_config(luna::JitConfig {
                mode: luna::JitMode::Auto,
                ..Default::default()
            })
            .unwrap();
        }
    }
    #[cfg(not(feature = "jit"))]
    assert!(!auto);
    let executor = lua
        .try_enter(|ctx| {
            let point = Point { x: 3, y: 4 }.into_value(ctx);
            assert_eq!(Point::from_value(ctx, point).unwrap(), Point { x: 3, y: 4 });
            let original = serde_json::json!({"name":"release", "values":[1,2,3], "enabled":true});
            let value = luna_util::serde::to_value(ctx, &original).unwrap();
            let decoded: serde_json::Value = luna_util::serde::from_value(ctx, value).unwrap();
            assert_eq!(decoded, original);
            let serialized =
                serde_json::to_value(luna_util::serde::SerializeValue::new(ctx, value)).unwrap();
            assert_eq!(serialized, original);
            ctx.set_global(
                "host_increment",
                Callback::from_fn(&ctx, |ctx, _, mut stack| {
                    let value: i64 = stack.consume(ctx)?;
                    stack.replace(ctx, value.wrapping_add(1));
                    Ok(CallbackReturn::Return)
                }),
            );
            ctx.set_global(
                "wait",
                Callback::from_fn(&ctx, |ctx, _, _| {
                    Ok(CallbackReturn::Sequence(async_sequence(
                        &ctx,
                        |_, mut seq| async move {
                            let answer = seq.await_future(Delayed { remaining: 2 }).await;
                            seq.enter(|ctx, _, _, mut stack| stack.replace(ctx, answer));
                            Ok(SequenceReturn::Return)
                        },
                    )))
                }),
            );
            let closure = Closure::load(ctx, Some(name), script.as_bytes())?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();
    #[cfg(feature = "jit")]
    let prepared = lua.prepare_jit().unwrap();
    #[cfg(not(feature = "jit"))]
    let prepared = 0usize;
    let (result, wakes) = if asynchronous {
        let (result, wakes) = block_on(lua.execute_async::<i64>(&executor));
        (result.unwrap(), wakes)
    } else {
        (lua.execute::<i64>(&executor).unwrap(), 0)
    };
    assert_eq!(result, expected, "{name}");
    assert_eq!(wakes, if asynchronous { 2 } else { 0 });
    #[cfg(feature = "jit")]
    let (native, code, failures) = {
        let stats = lua.jit_stats();
        (
            stats.native_instructions,
            stats.code_bytes,
            stats.compilation_failures,
        )
    };
    #[cfg(not(feature = "jit"))]
    let (native, code, failures) = (0u64, 0usize, 0u64);
    assert_eq!(failures, 0);
    if auto {
        assert!(prepared > 0 && native > 0 && code > 0, "{name}");
    } else {
        assert_eq!((prepared, native, code), (0, 0, 0));
    }
    println!("case={name} result={result} prepared={prepared} native={native} code={code} failures={failures} wakes={wakes}");
}

fn main() {
    let mode = std::env::args().nth(1).expect("mode argument");
    assert!(mode == "off" || mode == "auto");
    let auto = mode == "auto";
    for (name, script, result) in [
        ("integer", "local s=0 for i=1,1000 do s=s+i end return s", 500500),
        ("array", "local a={} for i=1,128 do a[i]=i end local s=0 for i=1,128 do s=s+a[i] end return s", 8256),
        ("callback", "local s=0 for i=1,128 do s=s+host_increment(i) end return s", 8384),
        ("upvalue", "local n=0 local function add(x) n=n+x end for i=1,128 do add(i) end return n", 8256),
        ("polymorphic", "local mt={} mt.__add=function(a,b) return setmetatable({n=a.n+b.n},mt) end local s=setmetatable({n=0},mt) local one=setmetatable({n=1},mt) for i=1,32 do s=s+one end return s.n", 32),
        ("coroutine", "local co=coroutine.create(function() for i=1,3 do coroutine.yield(i) end return 4 end) local s=0 for i=1,4 do local ok,v=coroutine.resume(co) assert(ok) s=s+v end return s", 10),
    ] { run(name, script, result, auto, false); }
    run(
        "async",
        "local x=wait() local s=0 for i=1,1000 do s=s+i end return s+x",
        500542,
        auto,
        true,
    );
    println!(
        "release_probe=1 jit={} mode={mode} cases=7 derive=1 serde=1 async_wakes=2",
        u8::from(cfg!(feature = "jit"))
    );
}
