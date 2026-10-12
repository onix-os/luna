use super::*;
use crate::{jit::work, Fuel, JitConfig, Value as LuaValue};
use cranelift_codegen::{ir::InstructionData, settings};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Module};

type Entry = unsafe extern "C" fn(*mut State);

struct Code(Option<JITModule>);

impl Drop for Code {
    fn drop(&mut self) {
        unsafe { self.0.take().unwrap().free_memory() };
    }
}

fn with_entry(plan: &Plan<'_>, capture: Option<usize>, test: impl FnOnce(Entry)) {
    let mut code = Code(Some(JITModule::new(
        JITBuilder::new(default_libcall_names()).unwrap(),
    )));
    let module = code.0.as_mut().unwrap();
    let mut context = module.make_context();
    let config = module.isa().frontend_config();
    let convention = module.isa().default_call_conv();
    context.func = plan.function(capture, config, convention).unwrap();
    plan.verify_function(&context.func, capture, config, convention)
        .unwrap();
    cranelift_codegen::verify_function(&context.func, module.isa()).unwrap();
    assert!(context.func.dfg.ext_funcs.is_empty());
    let id = module
        .declare_anonymous_function(&context.func.signature)
        .unwrap();
    module.define_function(id, &mut context).unwrap();
    assert!(context.compiled_code().unwrap().buffer.relocs().is_empty());
    module.finalize_definitions().unwrap();
    let entry =
        unsafe { std::mem::transmute::<*const u8, Entry>(module.get_finalized_function(id)) };
    test(entry);
}

fn initial(plan: &Plan<'_>, pc: usize, budget: u32, fuel: i32) -> State {
    let integer = |value| abi::Slot::from_value(LuaValue::Integer(value));
    let mut state = State {
        slots: std::array::from_fn(|index| integer(index as i64 + 30)),
        upper: integer(7),
        root: 1,
        pc: pc as u64,
        fuel,
        interrupted: 0,
        budget,
        instructions: u32::MAX,
        calls: u32::MAX,
        reason: u32::MAX,
    };
    let function = plan.pair.function;
    for operation in &plan.caller.operations[plan.start..plan.call] {
        if let Operation::Move { dest, source } = *operation {
            if dest == function {
                state.slots[usize::from(source.0)] = abi::Slot {
                    tag: abi::REFERENCE,
                    bits: state.root,
                };
            }
        }
    }
    state.slots[usize::from(function.0)] = abi::Slot {
        tag: abi::REFERENCE,
        bits: state.root,
    };
    for (offset, value) in [0, 4, 1, 0].into_iter().enumerate() {
        state.slots[plan.base + offset] = integer(value);
    }
    state
}

fn reference(plan: &Plan<'_>, capture: Option<usize>, state: &mut State) {
    state.instructions = 0;
    state.calls = 0;
    if state.budget > 64 || state.root >= plan.caller.registers as u64 {
        state.reason = INVALID;
        return;
    }
    if state.interrupted != 0 || state.fuel <= 0 {
        state.reason = FUEL;
        return;
    }
    if !(plan.start as u64..=plan.end as u64).contains(&state.pc) {
        state.reason = INVALID;
        return;
    }
    let integer = |value| abi::Slot::from_value(LuaValue::Integer(value));
    loop {
        let pc = state.pc as usize;
        let cost = if pc == plan.call { 5 } else { 1 };
        if state.budget < cost {
            state.reason = BUDGET;
            return;
        }
        match plan.caller.operations[pc] {
            Operation::Move { dest, source } => {
                state.slots[usize::from(dest.0)] = state.slots[usize::from(source.0)];
                state.pc += 1;
                state.fuel -= 1;
            }
            Operation::Call { .. } => {
                let function = usize::from(plan.pair.function.0);
                let arguments = usize::from(plan.pair.arguments);
                let slot = state.slots[function];
                let captured = capture
                    .map(|index| state.slots[index])
                    .unwrap_or(state.upper);
                let pattern = plan.pair.pattern;
                let right = match pattern.right {
                    leaf::Operand::Register(index) if index == pattern.read => captured,
                    leaf::Operand::Register(index) => {
                        state.slots[function + 1 + usize::from(index.0)]
                    }
                    leaf::Operand::Constant(index) => plan.callee.constants[usize::from(index)],
                };
                if slot.tag != abi::REFERENCE
                    || slot.bits != state.root
                    || captured.tag != abi::INTEGER
                    || right.tag != abi::INTEGER
                {
                    state.reason = GUARD;
                    return;
                }
                let mut fuel = Fuel::with(state.fuel);
                fuel.consume(4);
                fuel.consume(arguments as i32);
                fuel.consume(4);
                if !fuel.should_continue() {
                    state.reason = FUEL;
                    return;
                }
                let left = captured.bits as i64;
                let right = right.bits as i64;
                let updated = match pattern.arithmetic {
                    leaf::Arithmetic::Add => left.wrapping_add(right),
                    leaf::Arithmetic::Sub => left.wrapping_sub(right),
                    leaf::Arithmetic::Mul => left.wrapping_mul(right),
                };
                let mut physical = state.slots[..plan.caller.registers].to_vec();
                physical.copy_within(function + 1..function + 1 + arguments, function);
                physical.resize(
                    function + plan.callee.registers,
                    abi::Slot::from_value(LuaValue::Nil),
                );
                physical[function + arguments..].fill(abi::Slot::from_value(LuaValue::Nil));
                physical[function + usize::from(pattern.read.0)] = integer(left);
                physical[function + usize::from(pattern.result.0)] = integer(updated);
                if let Some(index) = capture {
                    physical[index] = integer(updated);
                } else {
                    state.upper = integer(updated);
                }
                physical.resize(plan.caller.registers, abi::Slot::from_value(LuaValue::Nil));
                state.slots[..plan.caller.registers].copy_from_slice(&physical);
                fuel.consume(4);
                fuel.consume(3);
                fuel.consume(4);
                state.fuel = fuel.remaining();
                state.calls += 1;
                state.pc += 1;
            }
            Operation::NumericForLoop { .. } => {
                let values = &state.slots[plan.base..plan.base + 3];
                if values.iter().any(|value| value.tag != abi::INTEGER) {
                    state.reason = GUARD;
                    return;
                }
                let index = values[0].bits as i64;
                let limit = values[1].bits as i64;
                let step = values[2].bits as i64;
                let (next, overflow) = index.overflowing_add(step);
                state.slots[plan.base] = integer(next);
                state.pc += 1;
                state.fuel -= 1;
                if !overflow
                    && if step < 0 {
                        next >= limit
                    } else {
                        next <= limit
                    }
                {
                    state.slots[plan.base + 3] = integer(next);
                    state.pc = plan.start as u64;
                }
            }
            _ => unreachable!(),
        }
        state.budget -= cost;
        state.instructions += cost;
        if state.pc == (plan.end + 1) as u64 {
            state.reason = FINISHED;
            return;
        }
        if pc == plan.call && state.fuel <= 0 {
            state.reason = FUEL;
            return;
        }
    }
}

fn compare(plan: &Plan<'_>, capture: Option<usize>, entry: Entry, state: State) -> State {
    let mut expected = state.clone();
    reference(plan, capture, &mut expected);
    let mut actual = state;
    unsafe { entry(&mut actual) };
    assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    actual
}

#[test]
fn native_loops_match_full_slot_and_fuel_model_at_every_budget() {
    let mut calls = 0;
    let mut finished = 0;
    for source in [
        &b"local x=7 local function f(v) x=x+v end for i=1,4 do f(i) end return x"[..],
        &b"local x=7 local function f(v) x=x-v end for i=31,-10,-3 do f(i) end return x"[..],
        &b"local x=3 local function f() x=x*2 end for i=1,17 do f() end return x"[..],
    ] {
        super::super::tests::sources(source, |caller, callee, call| {
            let plan = Plan::new(
                &caller,
                &callee,
                call,
                work::Limits::from(&JitConfig::default()),
            )
            .unwrap();
            for capture in [None, Some(0)] {
                with_entry(&plan, capture, |entry| {
                    for pc in plan.start..=plan.end {
                        for budget in 0..=64 {
                            for fuel in [i32::MIN, -1, 0, 1, 8, 9, 10, 11, 19, 20, 21, 64, i32::MAX]
                            {
                                let result = compare(
                                    &plan,
                                    capture,
                                    entry,
                                    initial(&plan, pc, budget, fuel),
                                );
                                calls += result.calls;
                                finished += u32::from(result.reason == FINISHED);
                            }
                        }
                    }
                });
            }
        });
    }
    assert_eq!((calls, finished), (13248, 634));
}

#[test]
fn native_loop_guards_aliases_and_overflow_preserve_exit_state() {
    super::super::tests::sources(super::super::tests::SOURCE, |caller, callee, call| {
        let plan = Plan::new(
            &caller,
            &callee,
            call,
            work::Limits::from(&JitConfig::default()),
        )
        .unwrap();
        for capture in [
            None,
            Some(0),
            Some(plan.base),
            Some(plan.base + 1),
            Some(plan.base + 2),
        ] {
            with_entry(&plan, capture, |entry| {
                for fault in 0..14 {
                    let mut state = initial(&plan, plan.call, 64, i32::MAX);
                    match fault {
                        0 => state.interrupted = 1,
                        1 => state.budget = 65,
                        2 => state.root = caller.registers as u64,
                        3 => state.pc = u64::MAX,
                        4 => state.slots[usize::from(plan.pair.function.0)].tag = abi::INTEGER,
                        5 => state.slots[usize::from(plan.pair.function.0)].bits = 0,
                        6 => state.slots[usize::from(plan.pair.function.0) + 1].tag = abi::NUMBER,
                        7 => {
                            if let Some(index) = capture {
                                state.slots[index].tag = abi::NUMBER
                            } else {
                                state.upper.tag = abi::NUMBER
                            }
                        }
                        8..=10 => {
                            state.pc = plan.end as u64;
                            state.slots[plan.base + fault - 8].tag = abi::NUMBER;
                        }
                        11 => {
                            state.slots[plan.base].bits = i64::MAX as u64;
                            state.pc = plan.end as u64;
                        }
                        12 => {
                            state.slots[plan.base].bits = i64::MIN as u64;
                            state.slots[plan.base + 2].bits = (-1i64) as u64;
                            state.pc = plan.end as u64;
                        }
                        13 => {
                            state.slots[plan.base + 2].bits = 0;
                            state.pc = plan.end as u64;
                        }
                        _ => unreachable!(),
                    }
                    compare(&plan, capture, entry, state);
                }
                for (index, limit, step) in [
                    (31, -10, -3),
                    (-9, 9, 3),
                    (0, 0, 0),
                    (i64::MAX - 1, i64::MAX, 1),
                    (i64::MIN + 1, i64::MIN, -1),
                ] {
                    let mut state = initial(&plan, plan.end, 64, i32::MAX);
                    for (offset, value) in [index, limit, step].into_iter().enumerate() {
                        state.slots[plan.base + offset].bits = value as u64;
                    }
                    compare(&plan, capture, entry, state);
                }
            });
        }
    });
}

#[test]
fn native_target_template_rejects_modified_guards_and_writes() {
    super::super::tests::sources(super::super::tests::SOURCE, |caller, callee, call| {
        let plan = Plan::new(
            &caller,
            &callee,
            call,
            work::Limits::from(&JitConfig::default()),
        )
        .unwrap();
        for target in [std::env::consts::ARCH] {
            let isa = cranelift_codegen::isa::lookup_by_name(target)
                .unwrap()
                .finish(settings::Flags::new(settings::builder()))
                .unwrap();
            let config = isa.frontend_config();
            let convention = isa.default_call_conv();
            let original = plan.function(Some(0), config, convention).unwrap();
            cranelift_codegen::verify_function(&original, isa.as_ref()).unwrap();
            assert!(plan
                .function(Some(usize::from(plan.pair.function.0)), config, convention)
                .is_err());
            let mut mutations = 0;
            for block in original.layout.blocks() {
                for inst in original.layout.block_insts(block) {
                    let mut changed = original.clone();
                    let mutated = match &mut changed.dfg.insts[inst] {
                        InstructionData::UnaryImm { imm, .. } => {
                            *imm = (i64::from(*imm) ^ 1).into();
                            true
                        }
                        InstructionData::Brif { blocks, .. } => {
                            blocks.swap(0, 1);
                            true
                        }
                        InstructionData::Load { offset, .. }
                        | InstructionData::Store { offset, .. } => {
                            *offset = (i32::from(*offset) + 1).into();
                            true
                        }
                        _ => false,
                    };
                    if mutated {
                        assert!(plan
                            .verify_function(&changed, Some(0), config, convention)
                            .is_err());
                        mutations += 1;
                    }
                }
            }
            assert!(mutations > 100);
            assert!(plan
                .verify_function(&original, None, config, convention)
                .is_err());
        }
    });
}

#[test]
fn generated_regions_match_physical_vm_operations_and_call_transitions() {
    use crate::{
        jit::{ir::Snapshot, projection::Origin},
        thread::activation::with_test_thread,
        Closure, Function as LuaFunction, Lua,
    };

    let mut compared = 0;
    let mut calls = 0;
    for source in [
        &b"local x=7 local function f(v) x=x+v end for i=1,4 do f(i) end return x"[..],
        &b"local x=7 local function f(v) x=x-v end for i=31,-10,-3 do f(i) end return x"[..],
        &b"local x=3 local function f() x=x*2 end for i=1,17 do f() end return x"[..],
        &b"local x=9223372036854775807 local function f(v) x=x+v end for i=1,4 do f(i) end return x"[..],
        &b"local x=7 local function f(v) x=x+v end for i=1,4 do x=i f(i) end return x"[..],
        &b"local x=7 local function f(v) x=x+v end for i=1,4 do local g=f g(i) end return x"[..],
    ] {
        Lua::empty().enter(|ctx| {
            let closure = Closure::load(ctx, None, source).unwrap();
            let caller = Snapshot::new(&closure.prototype(), 4096, 1 << 20).unwrap();
            let callee = Snapshot::new(&closure.prototype().prototypes[0], 4096, 1 << 20).unwrap();
            let call = caller.operations.iter().position(|op| matches!(op, Operation::Call { .. })).unwrap();
            let plan = Plan::new(&caller, &callee, call, work::Limits::from(&JitConfig::default())).unwrap();
            with_entry(&plan, Some(0), |entry| {
                for start in plan.start..=plan.end {
                    for budget in 0..=64 {
                        for fuel in [-1, 0, 8, 9, 10, 11, 20, 64, 1000] {
                            with_test_thread(ctx, closure, &mut Fuel::with(10000), |host| {
                                let mut setup = 0;
                                while host.with_registers(|_, r| *r.pc) != start {
                                    host.run(ctx, 1, 1, 0).result.unwrap();
                                    setup += 1;
                                    assert!(setup < 32);
                                }
                                host.test_fuel(Fuel::with(fuel));
                                let (roots, mut state) = host.with_registers(|_, registers| {
                                    let mut roots = vec![LuaValue::Nil; caller.registers];
                                    let mut state = initial(&plan, start, budget, fuel);
                                    assert!(abi::roots::capture(&mut roots, &mut state.slots[..caller.registers], registers.stack_frame));
                                    let (index, function) = roots.iter().enumerate().find_map(|(index, value)| match value {
                                        LuaValue::Function(LuaFunction::Closure(value)) if std::ptr::eq(&*value.prototype(), &*closure.prototype().prototypes[0]) => Some((index, *value)),
                                        _ => None,
                                    }).unwrap();
                                    assert!(matches!(registers.projection_origin(function.upvalues()[0].get()), Some(Origin::Register(0, _))));
                                    state.root = if start == plan.call { state.slots[usize::from(plan.pair.function.0)].bits } else { index as u64 };
                                    (roots, state)
                                });
                                unsafe { entry(&mut state) };
                                calls += state.calls;
                                let mut remaining = state.instructions;
                                while remaining != 0 {
                                    let pc = host.with_registers(|_, r| *r.pc);
                                    if matches!(caller.operations[pc], Operation::Call { .. }) {
                                        assert!(remaining >= 5);
                                        host.run(ctx, 2, 64, 4).result.unwrap();
                                        remaining -= 5;
                                    } else {
                                        host.run(ctx, 1, 1, 0).result.unwrap();
                                        remaining -= 1;
                                    }
                                }
                                let mut values = vec![LuaValue::Nil; caller.registers];
                                assert!(abi::roots::materialize(&roots, &state.slots[..caller.registers], &mut values));
                                host.with_registers(|_, registers| {
                                    assert_eq!(state.pc as usize, *registers.pc);
                                    for (actual, expected) in values.iter().zip(registers.stack_frame.iter()) {
                                        match (*actual, *expected) {
                                            (LuaValue::Nil, LuaValue::Nil) => {},
                                            (LuaValue::Integer(actual), LuaValue::Integer(expected)) => assert_eq!(actual, expected),
                                            (LuaValue::Function(actual), LuaValue::Function(expected)) => assert_eq!(actual, expected),
                                            _ => panic!("unexpected values {actual:?} {expected:?}"),
                                        }
                                    }
                                });
                                assert_eq!(state.fuel, host.fuel().remaining());
                                compared += 1;
                            });
                        }
                    }
                }
            });
        });
    }
    assert!(
        compared > 12000 && calls > 1000,
        "compared={compared} calls={calls}"
    );
}
