use crate::jit::resources::MappingCounter;
use crate::{
    opcode::{Operation, RCIndex},
    Constant,
};

use super::{
    abi::{self, Exit, Slot},
    ir::Snapshot,
};

fn scalar(slot: Slot) -> Option<Constant<&'static [u8]>> {
    Some(match slot.tag {
        abi::NIL => Constant::Nil,
        abi::BOOLEAN => Constant::Boolean(slot.bits != 0),
        abi::INTEGER => Constant::Integer(slot.bits as i64),
        abi::NUMBER => Constant::Number(f64::from_bits(slot.bits)),
        _ => return None,
    })
}

fn slot(value: Constant<&[u8]>) -> Slot {
    match value {
        Constant::Nil => Slot {
            tag: abi::NIL,
            bits: 0,
        },
        Constant::Boolean(value) => Slot {
            tag: abi::BOOLEAN,
            bits: u64::from(value),
        },
        Constant::Integer(value) => Slot {
            tag: abi::INTEGER,
            bits: value as u64,
        },
        Constant::Number(value) => Slot {
            tag: abi::NUMBER,
            bits: value.to_bits(),
        },
        Constant::String(_) => unreachable!(),
    }
}

fn numeric(value: Slot) -> bool {
    matches!(value.tag, abi::INTEGER | abi::NUMBER)
}

fn truth(value: Slot) -> bool {
    value.tag != abi::NIL && (value.tag != abi::BOOLEAN || value.bits != 0)
}

pub(super) fn run(snapshot: &Snapshot, slots: &mut [Slot], start: usize, budget: u32) -> Exit {
    let mut pc = start;
    let mut count = 0;
    loop {
        let Some(op) = snapshot.operations.get(pc).copied() else {
            return Exit {
                pc: pc as u64,
                instructions: count,
                reason: 0,
            };
        };
        if count >= budget.min(64) {
            return Exit {
                pc: pc as u64,
                instructions: count,
                reason: 2,
            };
        }
        let before = pc;
        let operand = |index: RCIndex| match index {
            RCIndex::Register(index) => slots[usize::from(index.0)],
            RCIndex::Constant(index) => snapshot.constants[usize::from(index.0)],
        };
        let fail = || Exit {
            pc: before as u64,
            instructions: count,
            reason: 1,
        };
        pc += 1;
        use Operation::*;
        match op {
            LoadConstant { dest, constant } => {
                let value = snapshot.constants[usize::from(constant.0)];
                if value.tag == abi::REFERENCE {
                    return Exit {
                        pc: before as u64,
                        instructions: count,
                        reason: 0,
                    };
                }
                slots[usize::from(dest.0)] = value;
            }
            Move { dest, source } => {
                let value = slots[usize::from(source.0)];
                if value.tag == abi::REFERENCE {
                    return fail();
                }
                slots[usize::from(dest.0)] = value;
            }
            LoadBool {
                dest,
                value,
                skip_next,
            } => {
                slots[usize::from(dest.0)] = slot(Constant::Boolean(value));
                pc += usize::from(skip_next);
            }
            LoadNil { dest, count } => {
                for index in usize::from(dest.0)..usize::from(dest.0) + usize::from(count) {
                    slots[index] = slot(Constant::Nil);
                }
            }
            Jump {
                offset,
                close_upvalues,
            } if close_upvalues.is_none() => {
                pc = pc.checked_add_signed(isize::from(offset)).unwrap()
            }
            Test { value, is_true } => {
                pc += usize::from(truth(slots[usize::from(value.0)]) == is_true);
            }
            Not { dest, source } => {
                slots[usize::from(dest.0)] =
                    slot(Constant::Boolean(!truth(slots[usize::from(source.0)])))
            }
            Add { dest, left, right }
            | Sub { dest, left, right }
            | Mul { dest, left, right }
            | Div { dest, left, right } => {
                let left = operand(left);
                let right = operand(right);
                if !numeric(left) || !numeric(right) {
                    return fail();
                }
                let left = scalar(left).unwrap();
                let right = scalar(right).unwrap();
                let value = match op {
                    Add { .. } => left.add(&right),
                    Sub { .. } => left.subtract(&right),
                    Mul { .. } => left.multiply(&right),
                    _ => left.float_divide(&right),
                }
                .unwrap();
                slots[usize::from(dest.0)] = slot(value);
            }
            Eq {
                skip_if,
                left,
                right,
            }
            | Less {
                skip_if,
                left,
                right,
            }
            | LessEq {
                skip_if,
                left,
                right,
            } => {
                let left = operand(left);
                let right = operand(right);
                if !numeric(left) || !numeric(right) {
                    return fail();
                }
                let left = scalar(left).unwrap();
                let right = scalar(right).unwrap();
                let result = match op {
                    Eq { .. } => left == right,
                    Less { .. } => left.less_than(&right).unwrap(),
                    _ => left.less_equal(&right).unwrap(),
                };
                pc += usize::from(result == skip_if);
            }
            NumericForPrep { base, jump } => {
                let index = slots[usize::from(base.0)];
                let step = slots[usize::from(base.0) + 2];
                if !numeric(index)
                    || !numeric(step)
                    || scalar(step).unwrap().to_number() == Some(0.0)
                {
                    return fail();
                }
                slots[usize::from(base.0)] = slot(
                    scalar(index)
                        .unwrap()
                        .subtract(&scalar(step).unwrap())
                        .unwrap(),
                );
                pc = pc.checked_add_signed(isize::from(jump)).unwrap();
            }
            NumericForLoop { base, jump } => {
                let base = usize::from(base.0);
                let index = slots[base];
                let limit = slots[base + 1];
                let step = slots[base + 2];
                if !numeric(index) || !numeric(limit) || !numeric(step) {
                    return fail();
                }
                let limit = scalar(limit).unwrap();
                let (value, within) = if index.tag == abi::INTEGER && step.tag == abi::INTEGER {
                    let step = step.bits as i64;
                    let (index, overflow) = (index.bits as i64).overflowing_add(step);
                    let within = if let Constant::Integer(limit) = limit {
                        if step < 0 {
                            index >= limit
                        } else {
                            index <= limit
                        }
                    } else if step < 0 {
                        index as f64 >= limit.to_number().unwrap()
                    } else {
                        index as f64 <= limit.to_number().unwrap()
                    };
                    (Constant::Integer(index), !overflow && within)
                } else {
                    let index = scalar(index).unwrap().to_number().unwrap();
                    let step = scalar(step).unwrap().to_number().unwrap();
                    let index = index + step;
                    let limit = limit.to_number().unwrap();
                    (
                        Constant::Number(index),
                        if step < 0.0 {
                            index >= limit
                        } else {
                            index <= limit
                        },
                    )
                };
                slots[base] = slot(value);
                if within {
                    slots[base + 3] = slot(value);
                    pc = pc.checked_add_signed(isize::from(jump)).unwrap();
                }
            }
            _ => {
                return Exit {
                    pc: before as u64,
                    instructions: count,
                    reason: 0,
                }
            }
        }
        count += 1;
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn snapshot(source: &[u8]) -> Snapshot {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let proto = crate::FunctionPrototype::compile(ctx, "model", source).unwrap();
            Snapshot::new(&proto, 4096, 2 * 1024 * 1024).unwrap()
        })
    }

    #[test]
    fn every_entry_and_budget_matches_the_rust_boundary_model() {
        let programs: &[&[u8]] = &[
            b"local s=0 for i=1,100 do s=s+i end return s",
            b"local s=0.0 for i=10.5,1.5,-0.5 do s=s+i*0.5 end return s",
            b"local a,b=... local x=a+b local y=a*b local z=x/0 return z<y and 1 or 2",
            b"local a=... local x=not a if x then return 1 else return 0 end",
        ];
        let values = [
            slot(Constant::Nil),
            slot(Constant::Boolean(false)),
            slot(Constant::Boolean(true)),
            slot(Constant::Integer(0)),
            slot(Constant::Integer(1)),
            slot(Constant::Integer(i64::MAX)),
            slot(Constant::Integer(i64::MIN)),
            slot(Constant::Number(0.0)),
            slot(Constant::Number(-0.0)),
            slot(Constant::Number(0.5)),
            slot(Constant::Number(f64::INFINITY)),
            slot(Constant::Number(f64::NAN)),
            Slot {
                tag: abi::REFERENCE,
                bits: 0,
            },
        ];
        for program in programs {
            let snapshot = snapshot(program);
            let memory = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
            let code =
                super::super::backend::compile(&snapshot, memory.clone(), 8 * 1024 * 1024).unwrap();
            for pc in 0..snapshot.operations.len() {
                for budget in [0, 1, 2, 3, 63, 64, 1000] {
                    for seed in 0..values.len() {
                        let mut expected: Vec<_> = (0..snapshot.registers)
                            .map(|index| values[(seed + index) % values.len()])
                            .collect();
                        let mut actual = expected.clone();
                        let reference = run(&snapshot, &mut expected, pc, budget);
                        let exit = code.invoke(&mut actual, pc, budget);
                        assert_eq!(
                            (exit.pc, exit.instructions, exit.reason),
                            (reference.pc, reference.instructions, reference.reason),
                            "pc={pc} budget={budget} seed={seed}"
                        );
                        for (actual, expected) in actual.iter().zip(&expected) {
                            assert_eq!(actual.tag, expected.tag);
                            if actual.tag == abi::NUMBER && f64::from_bits(actual.bits).is_nan() {
                                assert!(f64::from_bits(expected.bits).is_nan());
                            } else {
                                assert_eq!(actual.bits, expected.bits);
                            }
                        }
                    }
                }
            }
            drop(code);
            assert_eq!(memory.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn mixed_numeric_arithmetic_matches_at_every_entry_budget_and_alias() {
        use crate::types::{ConstantIndex8 as C, RegisterIndex as R, VarCount};
        let values = [
            Constant::Integer(i64::MIN),
            Constant::Integer(i64::MIN + 1),
            Constant::Integer(-9_007_199_254_740_993),
            Constant::Integer(-1),
            Constant::Integer(0),
            Constant::Integer(1),
            Constant::Integer(9_007_199_254_740_993),
            Constant::Integer(i64::MAX),
            Constant::Number(f64::NEG_INFINITY),
            Constant::Number(-9_223_372_036_854_775_808.0),
            Constant::Number(-1.5),
            Constant::Number(-f64::from_bits(1)),
            Constant::Number(-0.0),
            Constant::Number(0.0),
            Constant::Number(f64::from_bits(1)),
            Constant::Number(1.5),
            Constant::Number(9_223_372_036_854_775_808.0),
            Constant::Number(f64::INFINITY),
            Constant::Number(f64::NAN),
            Constant::Number(f64::from_bits(0xfff8_0000_0000_0001)),
            Constant::Nil,
            Constant::Boolean(true),
        ]
        .map(slot);
        for kind in 0..4 {
            for dest in [R(0), R(1), R(2)] {
                for layout in 0..4 {
                    let left = if layout & 1 == 0 {
                        RCIndex::Register(R(0))
                    } else {
                        RCIndex::Constant(C(0))
                    };
                    let right = if layout & 2 == 0 {
                        RCIndex::Register(R(1))
                    } else {
                        RCIndex::Constant(C(1))
                    };
                    let op = match kind {
                        0 => Operation::Add { dest, left, right },
                        1 => Operation::Sub { dest, left, right },
                        2 => Operation::Mul { dest, left, right },
                        _ => Operation::Div { dest, left, right },
                    };
                    let snapshot = Snapshot {
                        operations: super::super::resources::owned(&[
                            op,
                            Operation::Return {
                                start: dest,
                                count: VarCount::constant(1),
                            },
                        ]),
                        constants: super::super::resources::owned(&[
                            slot(Constant::Integer(i64::MAX)),
                            slot(Constant::Number(-0.0)),
                        ]),
                        registers: 3,
                        upvalues: 0,
                        prototypes: 0,
                    };
                    snapshot.verify().unwrap();
                    let memory =
                        MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
                    let code =
                        super::super::backend::compile(&snapshot, memory.clone(), 8 * 1024 * 1024)
                            .unwrap();
                    for a in values {
                        for b in values {
                            for pc in 0..2 {
                                for budget in [0, 1, 2, 64] {
                                    let mut expected = [
                                        a,
                                        b,
                                        Slot {
                                            tag: abi::REFERENCE,
                                            bits: 0x1234,
                                        },
                                    ];
                                    let mut actual = expected;
                                    let reference = run(&snapshot, &mut expected, pc, budget);
                                    let exit = code.invoke(&mut actual, pc, budget);
                                    assert_eq!((exit.pc, exit.instructions, exit.reason), (reference.pc, reference.instructions, reference.reason), "kind={kind} dest={dest:?} layout={layout} pc={pc} budget={budget} a={a:?} b={b:?}");
                                    for (actual, expected) in actual.iter().zip(expected) {
                                        assert_eq!(actual.tag, expected.tag);
                                        if actual.tag == abi::NUMBER
                                            && f64::from_bits(expected.bits).is_nan()
                                        {
                                            assert!(f64::from_bits(actual.bits).is_nan());
                                        } else {
                                            assert_eq!(actual.bits, expected.bits);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    drop(code);
                    assert_eq!(memory.load(Ordering::Relaxed), 0);
                }
            }
        }
    }

    #[test]
    fn mixed_numeric_loop_limits_match_at_every_entry_and_budget() {
        use crate::types::{RegisterIndex as R, VarCount};
        let values = [
            Constant::Integer(i64::MIN),
            Constant::Integer(i64::MIN + 1),
            Constant::Integer(-9_007_199_254_740_993),
            Constant::Integer(-2),
            Constant::Integer(-1),
            Constant::Integer(0),
            Constant::Integer(1),
            Constant::Integer(2),
            Constant::Integer(9_007_199_254_740_993),
            Constant::Integer(i64::MAX - 1),
            Constant::Integer(i64::MAX),
            Constant::Number(f64::NEG_INFINITY),
            Constant::Number(-9_223_372_036_854_775_808.0),
            Constant::Number(-1.5),
            Constant::Number(-0.0),
            Constant::Number(0.0),
            Constant::Number(f64::from_bits(1)),
            Constant::Number(1.5),
            Constant::Number(9_223_372_036_854_775_808.0),
            Constant::Number(f64::INFINITY),
            Constant::Number(f64::NAN),
            Constant::Nil,
            Constant::Boolean(true),
        ]
        .map(slot);
        for base in [0, 4] {
            let snapshot = Snapshot {
                operations: super::super::resources::owned(&[
                    Operation::NumericForLoop {
                        base: R(base),
                        jump: 1,
                    },
                    Operation::Return {
                        start: R(base + 3),
                        count: VarCount::constant(1),
                    },
                    Operation::Return {
                        start: R(base + 3),
                        count: VarCount::constant(1),
                    },
                ]),
                constants: super::super::resources::owned(&[]),
                registers: 8,
                upvalues: 0,
                prototypes: 0,
            };
            snapshot.verify().unwrap();
            let memory = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
            let code =
                super::super::backend::compile(&snapshot, memory.clone(), 8 * 1024 * 1024).unwrap();
            for index in values {
                for limit in values {
                    for step in values {
                        for pc in 0..snapshot.operations.len() {
                            for budget in [0, 1, 2, 64] {
                                let mut expected = [slot(Constant::Integer(123)); 8];
                                expected[usize::from(base)..usize::from(base) + 3]
                                    .copy_from_slice(&[index, limit, step]);
                                let mut actual = expected;
                                let reference = run(&snapshot, &mut expected, pc, budget);
                                let exit = code.invoke(&mut actual, pc, budget);
                                assert_eq!((exit.pc, exit.instructions, exit.reason), (reference.pc, reference.instructions, reference.reason), "base={base} pc={pc} budget={budget} index={index:?} limit={limit:?} step={step:?}");
                                for (actual, expected) in actual.iter().zip(expected) {
                                    assert_eq!(actual.tag, expected.tag);
                                    if actual.tag == abi::NUMBER
                                        && f64::from_bits(expected.bits).is_nan()
                                    {
                                        assert!(f64::from_bits(actual.bits).is_nan());
                                    } else {
                                        assert_eq!(actual.bits, expected.bits);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            drop(code);
            assert_eq!(memory.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn mixed_numeric_comparisons_match_at_every_budget_and_skip_polarity() {
        use crate::types::{ConstantIndex8, RegisterIndex, VarCount};
        let integers = [
            i64::MIN,
            i64::MIN + 1,
            -9_007_199_254_740_993,
            -2,
            -1,
            0,
            1,
            2,
            9_007_199_254_740_991,
            9_007_199_254_740_992,
            9_007_199_254_740_993,
            i64::MAX - 1,
            i64::MAX,
        ];
        let mut floats = vec![
            f64::NEG_INFINITY,
            -9_223_372_036_854_775_808.0,
            -9_007_199_254_740_992.0,
            -2.5,
            -2.0,
            -1.5,
            -1.0,
            -0.5,
            -f64::MIN_POSITIVE,
            -0.0,
            0.0,
            f64::MIN_POSITIVE,
            0.5,
            1.0,
            1.5,
            2.0,
            2.5,
            9_007_199_254_740_992.0,
            9_223_372_036_854_775_808.0,
            f64::INFINITY,
            f64::NAN,
            f64::from_bits(1),
            -f64::from_bits(1),
        ];
        for bits in [
            (-9_223_372_036_854_775_808.0f64).to_bits(),
            9_223_372_036_854_775_808.0f64.to_bits(),
            9_007_199_254_740_992.0f64.to_bits(),
        ] {
            floats.extend([f64::from_bits(bits - 1), f64::from_bits(bits + 1)]);
        }
        let mut seed = 0xd1b5_4a32_d192_ed03u64;
        for _ in 0..128 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            floats.push(f64::from_bits(seed));
        }
        for comparison in 0..3 {
            for skip_if in [false, true] {
                for constant_right in [false, true] {
                    let right = if constant_right {
                        RCIndex::Constant(ConstantIndex8(0))
                    } else {
                        RCIndex::Register(RegisterIndex(1))
                    };
                    let left = RCIndex::Register(RegisterIndex(0));
                    let op = match comparison {
                        0 => Operation::Eq {
                            skip_if,
                            left,
                            right,
                        },
                        1 => Operation::Less {
                            skip_if,
                            left,
                            right,
                        },
                        _ => Operation::LessEq {
                            skip_if,
                            left,
                            right,
                        },
                    };
                    let snapshot = Snapshot {
                        operations: super::super::resources::owned(&[
                            op,
                            Operation::LoadBool {
                                dest: RegisterIndex(2),
                                value: false,
                                skip_next: true,
                            },
                            Operation::LoadBool {
                                dest: RegisterIndex(2),
                                value: true,
                                skip_next: false,
                            },
                            Operation::Return {
                                start: RegisterIndex(2),
                                count: VarCount::constant(1),
                            },
                        ]),
                        constants: super::super::resources::owned(&[slot(Constant::Number(1.5))]),
                        registers: 3,
                        upvalues: 0,
                        prototypes: 0,
                    };
                    snapshot.verify().unwrap();
                    let memory =
                        MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
                    let code =
                        super::super::backend::compile(&snapshot, memory.clone(), 8 * 1024 * 1024)
                            .unwrap();
                    for integer in integers {
                        for &float in &floats {
                            for reversed in [false, true] {
                                if constant_right && (float != 1.5 || reversed) {
                                    continue;
                                }
                                let pair = [
                                    slot(Constant::Integer(integer)),
                                    slot(Constant::Number(float)),
                                ];
                                for budget in [0, 1, 2, 3, 64] {
                                    let mut expected = vec![
                                        pair[usize::from(reversed)],
                                        pair[usize::from(!reversed)],
                                        slot(Constant::Nil),
                                    ];
                                    let mut actual = expected.clone();
                                    let reference = run(&snapshot, &mut expected, 0, budget);
                                    let exit = code.invoke(&mut actual, 0, budget);
                                    assert_eq!((exit.pc, exit.instructions, exit.reason),
                                        (reference.pc, reference.instructions, reference.reason),
                                        "op={op:?} integer={integer} float={float:?} reversed={reversed} budget={budget}");
                                    for (actual, expected) in actual.iter().zip(&expected) {
                                        assert_eq!(
                                            (actual.tag, actual.bits),
                                            (expected.tag, expected.bits)
                                        );
                                    }
                                }
                            }
                        }
                    }
                    drop(code);
                    assert_eq!(memory.load(Ordering::Relaxed), 0);
                }
            }
        }
    }

    #[test]
    fn unknown_entry_declines_before_budget_checks() {
        let snapshot = snapshot(b"return 42");
        let memory = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
        let code =
            super::super::backend::compile(&snapshot, memory.clone(), 8 * 1024 * 1024).unwrap();
        for pc in [
            snapshot.operations.len(),
            snapshot.operations.len() + 1,
            usize::MAX,
        ] {
            for budget in [0, 1, 64, u32::MAX] {
                let mut slots = vec![slot(Constant::Nil); snapshot.registers];
                let reference = run(&snapshot, &mut slots, pc, budget);
                let exit = code.invoke(&mut slots, pc, budget);
                assert_eq!(
                    (reference.pc, reference.instructions, reference.reason),
                    (pc as u64, 0, 0)
                );
                assert_eq!((exit.pc, exit.instructions, exit.reason), (pc as u64, 0, 0));
                assert!(slots
                    .iter()
                    .all(|slot| slot.tag == abi::NIL && slot.bits == 0));
            }
        }
        drop(code);
        assert_eq!(memory.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn retired_entry_remains_executable_until_its_last_lease_drops() {
        for configuration in [
            None,
            Some(super::super::JitConfig::default()),
            Some(super::super::JitConfig {
                mode: super::super::JitMode::Auto,
                max_code_bytes: 1,
                ..super::super::JitConfig::default()
            }),
            Some(super::super::JitConfig {
                mode: super::super::JitMode::Auto,
                max_metadata_bytes: 1,
                ..super::super::JitConfig::default()
            }),
            Some(super::super::JitConfig {
                mode: super::super::JitMode::Auto,
                max_ir_instructions: 1,
                ..super::super::JitConfig::default()
            }),
            Some(super::super::JitConfig {
                mode: super::super::JitMode::Auto,
                max_ir_blocks: 1,
                ..super::super::JitConfig::default()
            }),
        ] {
            let snapshot = snapshot(b"local x=40 return x+2");
            let memory = MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX));
            let mut manager = super::super::Manager::default();
            let metadata = manager.metadata.0.clone();
            let code = super::super::owner::Shared::try_new(
                super::super::backend::compile_in(
                    &snapshot,
                    memory.clone(),
                    8 * 1024 * 1024,
                    manager.metadata.clone(),
                    super::super::work::Limits::from(&super::super::JitConfig::default()),
                    super::super::backend::Failure::None,
                )
                .unwrap(),
                manager.metadata.clone(),
            )
            .unwrap();
            manager.memory = memory.clone();
            manager.config.mode = super::super::JitMode::Auto;
            manager
                .code
                .insert(1, super::super::CachedCode { code, last_used: 0 });
            let runtime = super::super::Runtime::try_new(manager).unwrap();
            let lease = runtime.lookup(1).unwrap();
            if let Some(configuration) = configuration {
                runtime.0.borrow_mut().configure(configuration);
            } else {
                runtime.0.borrow_mut().clear();
            }
            assert!(runtime.0.borrow().code.is_empty());
            assert!(runtime.lookup(1).is_none());
            assert!(memory.load(Ordering::Relaxed) > 0);
            assert!(metadata.current() > 0);
            let mut registers = vec![slot(Constant::Nil); snapshot.registers];
            let exit = lease.code.invoke(&mut registers, 0, 64);
            assert!(exit.instructions > 0);
            let Operation::Return { start, .. } = snapshot.operations[exit.pc as usize] else {
                panic!("missing return")
            };
            assert_eq!(registers[usize::from(start.0)].bits, 42);
            drop(lease);
            assert_eq!(memory.load(Ordering::Relaxed), 0);
            assert_eq!(metadata.current(), 0);
        }
    }

    #[test]
    fn malformed_ir_and_resource_limits_are_rejected() {
        let mut snapshot = snapshot(b"return 42");
        snapshot.operations[0] = Operation::Move {
            dest: crate::types::RegisterIndex(255),
            source: crate::types::RegisterIndex(0),
        };
        assert!(snapshot.verify().is_err());
        snapshot.operations[0] = Operation::Jump {
            offset: i16::MAX,
            close_upvalues: crate::types::Opt254::none(),
        };
        assert!(snapshot.verify().is_err());
    }
}
