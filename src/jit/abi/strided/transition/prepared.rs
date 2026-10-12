use super::*;
use crate::{
    jit::ir::Snapshot,
    opcode::{Operation, RCIndex},
    thread::LuaRegisters,
    Closure, Context,
};

const PREPARED: Helper = helper_selected::<{ abi::HELPER_GET_UP_TABLE }, true>;

struct Packed<'gc, 'a, 'b, 'p> {
    frame: *mut helpers::Frame<'gc, 'a, 'b, 'p>,
    slots: *mut abi::Slot,
    len: usize,
}

unsafe extern "C" fn packed_helper(
    header: *mut Header,
    data: *mut (),
    a: u32,
    b: u32,
    c: u32,
    pc: u32,
) -> u32 {
    let packed = unsafe { &mut *data.cast::<Packed<'_, '_, '_, '_>>() };
    let mut host = abi::Host {
        data: packed.frame.cast(),
        projection: ptr::null_mut(),
    };
    let status = unsafe { (helpers::PREHASHED_ENTRY)(&mut host, packed.slots, a, b, c, pc) };
    if packed.len <= 1 || unsafe { (*packed.slots.add(1)).tag } != abi::INTEGER {
        unsafe { header.write(Header::retired()) };
    } else if unsafe { (*header).base.is_null() } {
        unsafe {
            header.write(Header {
                base: packed
                    .slots
                    .cast::<u8>()
                    .add(mem::offset_of!(abi::Slot, bits)),
                stride: mem::size_of::<abi::Slot>(),
                len: packed.len,
            })
        };
    }
    status
}

unsafe extern "C" fn rust_control<const STRIDED: bool>(
    header: *mut Header,
    data: *mut (),
    a: u32,
    b: u32,
    c: u32,
    pc: u32,
) -> u32 {
    unsafe { rust_entry::<1, 1>(header, 1) };
    let helper = if STRIDED { PREPARED } else { packed_helper };
    let result = unsafe { helper(header, data, a, b, c, pc) };
    if result != abi::HELPER_COMPLETED {
        return result;
    }
    result | (unsafe { rust_entry::<1, 1>(header, 2) } * READY)
}

#[inline(never)]
fn packed_invoke<const N: usize>(
    frame: &mut helpers::Frame<'_, '_, '_, '_>,
    entry: Transition,
    args: [u32; 4],
) -> u32 {
    assert_eq!(frame.slot_count, N);
    let mut scratch = [mem::MaybeUninit::<abi::Slot>::uninit(); N];
    for (slot, value) in scratch
        .iter_mut()
        .zip(frame.registers.stack_frame[..N].iter().copied())
    {
        slot.write(abi::Slot::from_value(value));
    }
    let slots = scratch.as_mut_ptr().cast::<abi::Slot>();
    let mut header = if N > 1 && unsafe { (*slots.add(1)).tag } == abi::INTEGER {
        Header {
            base: unsafe { slots.cast::<u8>().add(mem::offset_of!(abi::Slot, bits)) },
            stride: mem::size_of::<abi::Slot>(),
            len: N,
        }
    } else {
        Header::retired()
    };
    let mut packed = Packed {
        frame: ptr::from_mut(frame),
        slots,
        len: N,
    };
    let [a, b, c, pc] = args;
    let result = unsafe { entry(&mut header, ptr::from_mut(&mut packed).cast(), a, b, c, pc) };
    for (index, value) in frame.registers.stack_frame[..N].iter_mut().enumerate() {
        unsafe { slots.add(index).read() }.write_back(value);
    }
    result
}

#[inline(never)]
fn strided_invoke(
    frame: &mut helpers::Frame<'_, '_, '_, '_>,
    entry: Transition,
    args: [u32; 4],
) -> u32 {
    let [a, b, c, pc] = args;
    with_session(frame, Mask::new(&[1]).unwrap(), |header, data| unsafe {
        entry(header, data, a, b, c, pc)
    })
}

fn source<'gc>(ctx: Context<'gc>, bytes: &[u8]) -> (Closure<'gc>, usize, [u32; 4]) {
    let closure = Closure::load(ctx, None, bytes).unwrap();
    let snapshot = Snapshot::new(&closure.prototype(), 4096, 1 << 20).unwrap();
    snapshot.verify().unwrap();
    let (pc, operation) = snapshot
        .operations
        .iter()
        .enumerate()
        .find(|(_, op)| {
            matches!(
                op,
                Operation::GetUpTable {
                    key: RCIndex::Constant(_),
                    ..
                }
            )
        })
        .unwrap();
    let Operation::GetUpTable {
        dest,
        table,
        key: RCIndex::Constant(index),
    } = *operation
    else {
        unreachable!()
    };
    let packed = (u32::from(dest.0) << 16) | (u32::from(table.0) << 8) | u32::from(index.0);
    let hash = snapshot.prepared_keys.as_ref().unwrap().hash(index.0);
    (
        closure,
        snapshot.registers,
        [packed, hash as u32, (hash >> 32) as u32, pc as u32],
    )
}

#[derive(Debug, PartialEq)]
struct Report {
    result: u32,
    pc: usize,
    fuel: i32,
    counts: [u64; 8],
    panicked: bool,
}

fn counts(frame: &helpers::Frame<'_, '_, '_, '_>) -> [u64; 8] {
    let c = &frame.count;
    [
        c.calls,
        c.completed,
        c.declined,
        c.table_reads,
        c.table_writes,
        c.upvalue_reads,
        c.upvalue_writes,
        c.allocations,
    ]
}

fn compare(native: bool) {
    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    let code = native.then(|| [packed_helper, PREPARED].map(super::native::compile));
    let entries: [Transition; 2] = [rust_control::<false>, rust_control::<true>];
    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    let entries = code
        .as_ref()
        .map_or(entries, |code| code.each_ref().map(|code| code.entry));
    #[cfg(miri)]
    assert!(!native);
    crate::Lua::empty().enter(|ctx| {
        let (closure, _, args) = source(ctx, b"return target");
        let table = ctx.globals();
        let values = [
            Value::Integer(i64::MAX),
            Value::Integer(i64::MIN),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8_1234_5678_9abc)),
            Value::Nil,
            Value::Boolean(true),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(
                crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return)).into(),
            ),
        ];
        let mut comparisons = 0;
        for value in values {
            for scenario in 0..12 {
                for pc in [0, 65536, u32::MAX] {
                    let mut args = args;
                    args[0] = (args[0] & 0xffff) | (1 << 16);
                    args[3] = pc;
                    match scenario {
                        3 => args[0] |= 1 << 24,
                        4 => args[0] = (args[0] & !255) | 255,
                        5 => args[0] = (args[0] & !(255 << 8)) | (255 << 8),
                        6 => args[0] = (args[0] & 0xffff) | (255 << 16),
                        8 => args[2] ^= 1 << 31,
                        _ => {}
                    }
                    let reports = entries
                        .into_iter()
                        .enumerate()
                        .map(|(mode, entry)| {
                            table.set_metatable(ctx, None);
                            table
                                .set(
                                    ctx,
                                    "target",
                                    if scenario == 2 { Value::Nil } else { value },
                                )
                                .unwrap();
                            if scenario == 2 {
                                let mt = crate::Table::new(&ctx);
                                mt.set(ctx, "__index", crate::Table::new(&ctx)).unwrap();
                                table.set_metatable(ctx, Some(mt));
                            }
                            let mut canonical = [
                                Value::Table(table),
                                Value::Integer(73),
                                Value::Nil,
                                Value::Boolean(true),
                            ];
                            if scenario == 9 {
                                canonical[0] = Value::Boolean(false);
                            }
                            if scenario == 10 {
                                canonical[1] = Value::Nil;
                            }
                            let mut current_pc = 17;
                            let mut fuel = crate::Fuel::with(47);
                            let report = LuaRegisters::with_test_frame_state(
                                ctx,
                                &mut current_pc,
                                &mut canonical,
                                &mut fuel,
                                false,
                                |mut registers| {
                                    let cell = if matches!(scenario, 1 | 9) {
                                        registers
                                            .open_test_upvalue(&ctx, crate::types::RegisterIndex(0))
                                    } else if scenario == 11 {
                                        registers
                                            .open_test_upvalue(&ctx, crate::types::RegisterIndex(1))
                                    } else {
                                        crate::closure::UpValue::new(
                                            &ctx,
                                            crate::closure::UpValueState::Closed(Value::Table(
                                                table,
                                            )),
                                        )
                                    };
                                    closure.set_upvalue(&ctx, 0, cell);
                                    let mut frame = helpers::Frame {
                                        ctx,
                                        closure,
                                        registers: &mut registers,
                                        count: helpers::Counts::default(),
                                        slot_count: 4,
                                        panic: None,
                                        projection: None,
                                    };
                                    let inner = table.into_inner();
                                    let borrow = (scenario == 7).then(|| inner.borrow_mut(&ctx));
                                    let result = if mode == 0 {
                                        packed_invoke::<4>(&mut frame, entry, args)
                                    } else {
                                        strided_invoke(&mut frame, entry, args)
                                    };
                                    drop(borrow);
                                    (
                                        result,
                                        *frame.registers.pc,
                                        counts(&frame),
                                        frame.panic.is_some(),
                                    )
                                },
                            );
                            (
                                Report {
                                    result: report.0,
                                    pc: report.1,
                                    counts: report.2,
                                    panicked: report.3,
                                    fuel: fuel.remaining(),
                                },
                                canonical,
                            )
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(
                        reports[0].0, reports[1].0,
                        "scenario={scenario} pc={pc} value={value:?}"
                    );
                    for (a, b) in reports[0].1.into_iter().zip(reports[1].1) {
                        abi::tests::assert_identical(a, b);
                    }
                    let expected = match scenario {
                        2 | 3 | 4 | 9 | 11 => abi::HELPER_DECLINED,
                        5 | 6 | 7 => abi::HELPER_PANICKED,
                        _ => abi::HELPER_COMPLETED,
                    };
                    assert_eq!(reports[1].0.result & !READY, expected);
                    comparisons += 1;
                }
            }
        }
        assert_eq!(comparisons, 288);
    });
}

#[test]
fn strided_prepared_matches_proxy_guards_values_aliases_and_panics() {
    compare(false);
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn strided_prepared_native_matches_proxy_guards_values_aliases_and_panics() {
    compare(true);
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod probe;
