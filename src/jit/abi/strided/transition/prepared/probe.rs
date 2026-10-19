use super::*;
use std::hint::black_box;

#[allow(dead_code)]
mod workloads {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/jit_support/workloads.rs"
    ));
}

#[inline(never)]
fn profile_calls<const N: usize>(
    frame: &mut helpers::Frame<'_, '_, '_, '_>,
    entry: Transition,
    args: [u32; 4],
    strided: bool,
) -> u32 {
    let mut result = 0;
    for _ in 0..100_000 {
        result += if strided {
            strided_invoke(black_box(frame), black_box(entry), black_box(args))
        } else {
            packed_invoke::<N>(black_box(frame), black_box(entry), black_box(args))
        };
    }
    black_box(result)
}

fn run<const N: usize>(shape: &str, strided: bool, bytes: &[u8]) {
    let code = super::super::native::compile(if strided { PREPARED } else { packed_helper });
    crate::Lua::empty().enter(|ctx| {
        let (closure, width, args) = source(ctx, bytes);
        assert_eq!(width, N);
        let called = std::rc::Rc::new(std::cell::Cell::new(0));
        let tracker = called.clone();
        let value = if shape == "callback_lookup" {
            Value::Function(crate::Callback::from_fn(&ctx, move |_, _, _| {
                tracker.set(tracker.get() + 1);
                Ok(crate::CallbackReturn::Return)
            }).into())
        } else { Value::Integer(42) };
        ctx.globals().set(ctx, if shape == "callback_lookup" { "host_increment" } else { "target" }, value).unwrap();
        let mut canonical = [Value::Integer(7); N];
        let dest = (args[0] >> 16) as usize;
        assert!(dest != 1);
        let mut pc = 0;
        let mut fuel = crate::Fuel::with(47);
        LuaRegisters::with_test_frame_state(ctx, &mut pc, &mut canonical, &mut fuel, false, |mut registers| {
            let mut frame = helpers::Frame {
                read_hints: None,
                ctx, closure, registers: &mut registers, count: helpers::Counts::default(),
                slot_count: N, panic: None, projection: None,
            };
            let result = profile_calls::<N>(&mut frame, code.entry, args, strided);
            assert_eq!(result, 100_000 * (abi::HELPER_COMPLETED | if N > 1 { READY } else { 0 }));
            assert_eq!(counts(&frame), [100_000, 100_000, 0, 100_000, 0, 100_000, 0, 0]);
            assert!(frame.panic.is_none());
        });
        assert_eq!(pc, args[3] as usize + 1);
        assert_eq!(fuel.remaining(), 47);
        assert_eq!(called.get(), 0);
        for (index, actual) in canonical.into_iter().enumerate() {
            let expected = if index == dest { value } else if index == 1 { Value::Integer(300_007) } else { Value::Integer(7) };
            abi::tests::assert_identical(actual, expected);
        }
        println!("shape={shape} strided={strided} calls=100000 completed=100000 callback_invocations=0 validated=true width={width} pc={pc} packed={} hash_low={} hash_high={}", args[0], args[1], args[2]);
    });
}

#[test]
#[ignore = "prepared transport component profile, not full Lua execution"]
fn profile_prepared_transport() {
    let strided = match std::env::var("LUNA_STRIDED").unwrap().as_str() {
        "0" => false,
        "1" => true,
        _ => panic!("LUNA_STRIDED must be 0 or 1"),
    };
    let shape = std::env::var("LUNA_STRIDED_SHAPE").unwrap();
    match shape.as_str() {
        "scalar_lookup" => run::<1>(&shape, strided, b"return target"),
        "callback_lookup" => {
            let source = workloads::WORKLOADS
                .iter()
                .find(|w| w.name == "rust_callbacks")
                .unwrap();
            assert_eq!(source.expected, 12507500);
            run::<7>(&shape, strided, source.source);
        }
        _ => panic!("unknown prepared shape"),
    }
}
