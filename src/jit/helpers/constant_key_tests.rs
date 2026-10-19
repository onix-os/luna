use super::*;
use crate::{
    closure::{UpValue, UpValueState},
    opcode::{Operation, RCIndex},
    types::{ConstantIndex8, RegisterIndex, UpValueIndex},
};

fn symbols(operations: &[crate::opcode::Operation]) -> [(u32, &'static str, abi::HelperEntry); 10] {
    use crate::opcode::{Operation, RCIndex};
    let mut found = false;
    for operation in operations {
        if let Operation::GetUpTable { key, .. } = operation {
            if matches!(key, RCIndex::Register(_)) {
                return SYMBOLS;
            }
            found = true;
        }
    }
    let mut symbols = SYMBOLS;
    if found {
        let symbol = symbols
            .iter_mut()
            .find(|symbol| symbol.0 == abi::HELPER_GET_UP_TABLE)
            .unwrap();
        *symbol = (
            abi::HELPER_GET_UP_TABLE,
            "luna_get_up_constant_v1",
            call_constant,
        );
    }
    symbols
}

unsafe extern "C" fn call_constant(
    host: *mut abi::Host,
    slots: *mut Slot,
    a: u32,
    b: u32,
    c: u32,
    pc: u32,
) -> u32 {
    if host.is_null() {
        return abi::HELPER_DECLINED;
    }
    let data = unsafe { (*host).data };
    let frame = unsafe { &mut *data.cast::<Frame<'_, '_, '_, '_>>() };
    frame.count.calls += 1;
    let result = catch_unwind(AssertUnwindSafe(|| {
        *frame.registers.pc = pc as usize + 1;
        if let Some(projection) = frame.projection.as_deref_mut() {
            projection
                .flush(frame.ctx, frame.registers)
                .expect("invalid pending projection");
        }
        let completed = {
            let slots = unsafe { std::slice::from_raw_parts_mut(slots, frame.slot_count) };
            let table = frame.upvalue(slots, b);
            let key =
                frame.closure.prototype().constants[(c & !abi::CONSTANT_OPERAND) as usize].into();
            let completed = frame.table_read(slots, a, table, key);
            frame.count.upvalue_reads += u64::from(completed);
            completed
        };
        if let Some(projection) = frame.projection.as_deref_mut() {
            projection
                .refresh(frame.registers, frame.closure.upvalues())
                .expect("invalid refreshed projection");
        }
        if !completed {
            *frame.registers.pc = pc as usize;
            return abi::HELPER_DECLINED;
        }
        *frame.registers.pc = pc as usize + 1;
        abi::HELPER_COMPLETED
    }));
    match result {
        Ok(abi::HELPER_COMPLETED) => {
            frame.count.completed += 1;
            abi::HELPER_COMPLETED
        }
        Ok(_) => {
            frame.count.declined += 1;
            abi::HELPER_DECLINED
        }
        Err(payload) => {
            let slots = unsafe { std::slice::from_raw_parts_mut(slots, frame.slot_count) };
            for (slot, dest) in slots
                .iter()
                .copied()
                .zip(frame.registers.stack_frame.iter_mut())
            {
                slot.write_back(dest);
            }
            frame.panic = Some(payload);
            abi::HELPER_PANICKED
        }
    }
}

fn read(key: RCIndex) -> Operation {
    Operation::GetUpTable {
        dest: RegisterIndex(0),
        table: UpValueIndex(0),
        key,
    }
}

#[test]
fn source_selection_requires_nonempty_exclusively_constant_reads() {
    let names = |ops: &[Operation]| symbols(ops).map(|(kind, name, _)| (kind, name));
    let original = SYMBOLS.map(|(kind, name, _)| (kind, name));
    assert_eq!(names(&[]), original);
    assert_eq!(
        names(&[Operation::GetTable {
            dest: RegisterIndex(0),
            table: RegisterIndex(1),
            key: RCIndex::Constant(ConstantIndex8(0)),
        }]),
        original
    );
    for count in 1..=16 {
        let mut ops = vec![read(RCIndex::Constant(ConstantIndex8(0))); count];
        let specialized = names(&ops);
        for (actual, expected) in specialized.into_iter().zip(original) {
            assert_eq!(actual.0, expected.0);
            if actual.0 == abi::HELPER_GET_UP_TABLE {
                assert_eq!(actual.1, "luna_get_up_constant_v1");
            } else {
                assert_eq!(actual, expected);
            }
        }
        for index in 0..count {
            for register in [0, 1, 255] {
                ops[index] = read(RCIndex::Register(RegisterIndex(register)));
                assert_eq!(names(&ops), original);
            }
            ops[index] = read(RCIndex::Constant(ConstantIndex8(255)));
            assert_eq!(names(&ops), specialized);
        }
    }
    let entry = symbols(&[read(RCIndex::Constant(ConstantIndex8(0)))])
        .into_iter()
        .find(|symbol| symbol.0 == abi::HELPER_GET_UP_TABLE)
        .unwrap()
        .2;
    assert_eq!(
        unsafe { entry(std::ptr::null_mut(), std::ptr::null_mut(), 0, 0, 0, 0) },
        abi::HELPER_DECLINED
    );
}

struct Observation<'gc> {
    values: [Value<'gc>; 7],
    slots: [Slot; 7],
    counts: [u64; 8],
    result: u32,
    pc: usize,
    fuel: i32,
    panic: Option<std::any::TypeId>,
}

fn observe<'gc>(
    entry: abi::HelperEntry,
    ctx: Context<'gc>,
    closure: Closure<'gc>,
    receiver: Value<'gc>,
    key: u32,
    scenario: usize,
    dest: u32,
) -> Observation<'gc> {
    let mut values = [
        receiver,
        Value::Integer(1),
        Value::Number(-0.0),
        Value::Nil,
        Value::Function(closure.into()),
        Value::Integer(i64::MAX),
        Value::Nil,
    ];
    let mut slots = values.map(Slot::from_value);
    slots[1] = Slot::from_value(Value::Integer(-47));
    slots[2] = Slot::from_value(Value::Number(f64::from_bits(0x7ff8000000000042)));
    if scenario == 2 {
        slots[0] = Slot::from_value(Value::Integer(42));
    }
    let mut pc = 3;
    let mut fuel = crate::Fuel::with(-1);
    let (result, counts, panic) = LuaRegisters::with_test_frame_state(
        ctx,
        &mut pc,
        &mut values,
        &mut fuel,
        scenario == 3,
        |mut registers| {
            let cell = if scenario == 1 || scenario == 2 {
                registers.open_test_upvalue(&ctx, RegisterIndex(0))
            } else {
                UpValue::new(&ctx, UpValueState::Closed(receiver))
            };
            closure.set_upvalue(&ctx, 0, cell);
            let mut frame = Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: Counts::default(),
                slot_count: if scenario == 4 { 3 } else { 7 },
                panic: None,
                read_hints: None,
                projection: None,
            };
            let mut host = abi::Host {
                data: std::ptr::from_mut(&mut frame).cast(),
                projection: std::ptr::null_mut(),
            };
            let b = if scenario == 5 { u32::MAX } else { 0 };
            let c = if scenario == 6 { u32::MAX } else { key };
            let result = unsafe { entry(&mut host, slots.as_mut_ptr(), dest, b, c, 17) };
            let count = frame.count;
            (
                result,
                [
                    count.calls,
                    count.completed,
                    count.declined,
                    count.table_reads,
                    count.table_writes,
                    count.upvalue_reads,
                    count.upvalue_writes,
                    count.allocations,
                ],
                frame.panic.as_deref().map(std::any::Any::type_id),
            )
        },
    );
    Observation {
        values,
        slots,
        result,
        counts,
        pc,
        fuel: fuel.remaining(),
        panic,
    }
}

#[test]
fn constant_entry_matches_generic_aliases_declines_and_panic_publication() {
    crate::Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"return _ENV.key, _ENV[42], _ENV[1.5]").unwrap();
        assert!(closure.prototype().constants.len() >= 3);
        let entry = symbols(&[read(RCIndex::Constant(ConstantIndex8(0)))])
            .into_iter()
            .find(|symbol| symbol.0 == abi::HELPER_GET_UP_TABLE)
            .unwrap()
            .2;
        let table = Table::new(&ctx);
        let missing = Table::new(&ctx);
        let meta = Table::new(&ctx);
        meta.set_field(ctx, "__index", true);
        missing.set_metatable(ctx, Some(meta));
        let payloads = [
            Value::Nil,
            Value::Boolean(false),
            Value::Integer(i64::MIN),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8000000000042)),
            Value::String(ctx.intern(b"payload")),
            Value::Table(table),
            Value::Function(closure.into()),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 42)),
        ];
        for (index, constant) in closure.prototype().constants.iter().copied().enumerate() {
            let key: Value = constant.into();
            for payload in payloads {
                table.set_readonly(&ctx, false);
                table.set_raw(&ctx, key, payload).unwrap();
                table.set_readonly(&ctx, true);
                for receiver in [
                    Value::Table(table),
                    Value::Table(missing),
                    Value::Nil,
                    Value::Integer(3),
                ] {
                    for scenario in 0..7 {
                        for dest in [0, 3, 6, 7] {
                            let key = abi::CONSTANT_OPERAND | index as u32;
                            let old = observe(
                                call::<{ abi::HELPER_GET_UP_TABLE }>,
                                ctx,
                                closure,
                                receiver,
                                key,
                                scenario,
                                dest,
                            );
                            let new = observe(entry, ctx, closure, receiver, key, scenario, dest);
                            assert_eq!(
                                (new.result, new.counts, new.pc, new.fuel, new.panic),
                                (old.result, old.counts, old.pc, old.fuel, old.panic),
                                "constant {index}, scenario {scenario}, dest {dest}"
                            );
                            for (actual, expected) in new.values.into_iter().zip(old.values) {
                                tests::assert_identical(actual, expected);
                            }
                            for (actual, expected) in new.slots.into_iter().zip(old.slots) {
                                assert_eq!(
                                    (actual.tag, actual.bits),
                                    (expected.tag, expected.bits)
                                );
                            }
                        }
                    }
                }
                tests::assert_identical(table.get_raw(&ctx, key), payload);
            }
        }
    });
}
