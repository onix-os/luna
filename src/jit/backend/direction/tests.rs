use super::*;
use crate::{
    jit::{
        abi::{self, Slot},
        backend::{self, Failure},
        ir::Snapshot,
        model, resources, work, JitConfig,
    },
    opcode::Operation,
    types::{RegisterIndex, VarCount},
};
use cranelift_codegen::ir::{AbiParam, Signature};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

fn fixture(shared: bool, reversed: bool) -> Function {
    let mut function = Function::new();
    function.signature = Signature::new(cranelift_codegen::isa::CallConv::SystemV);
    function
        .signature
        .params
        .extend([types::I8, types::I64, types::I64].map(AbiParam::new));
    function.signature.returns.push(AbiParam::new(types::I8));
    let mut context = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut function, &mut context);
    let block = builder.create_block();
    builder.append_block_params_for_function_params(block);
    builder.switch_to_block(block);
    let args = builder.block_params(block).to_vec();
    let ge = builder
        .ins()
        .icmp(IntCC::SignedGreaterThanOrEqual, args[1], args[2]);
    let le = builder
        .ins()
        .icmp(IntCC::SignedLessThanOrEqual, args[1], args[2]);
    let selected = if reversed {
        builder.ins().select(args[0], le, ge)
    } else {
        builder.ins().select(args[0], ge, le)
    };
    let result = if shared {
        builder.ins().band(selected, ge)
    } else {
        selected
    };
    builder.ins().return_(&[result]);
    builder.seal_all_blocks();
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    let isa = cranelift_codegen::isa::lookup("x86_64-unknown-linux-gnu".parse().unwrap())
        .unwrap()
        .finish(flags)
        .unwrap();
    builder.finalize(isa.frontend_config());
    function
}

#[test]
fn direction_rewrite_is_idempotent_and_refuses_shared_or_reversed_comparisons() {
    let mut function = fixture(false, false);
    assert_eq!(rewrite(&mut function), 1);
    assert_eq!(rewrite(&mut function), 0);
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    cranelift_codegen::verify_function(&function, &flags).unwrap();
    assert_eq!(function.layout.blocks().count(), 4);
    for (shared, reversed) in [(true, false), (false, true)] {
        let mut function = fixture(shared, reversed);
        let before = function.clone();
        assert_eq!(rewrite(&mut function), 0);
        assert_eq!(function, before);
    }
}

#[test]
fn direction_rewrite_refuses_comparisons_passed_to_successors() {
    let mut function = fixture(false, false);
    let source = function.layout.entry_block().unwrap();
    let terminal = function.layout.last_inst(source).unwrap();
    let selected = function.dfg.inst_args(terminal)[0];
    let comparison = function
        .layout
        .block_insts(source)
        .find(|&inst| matches!(function.dfg.insts[inst], InstructionData::IntCompare { .. }))
        .unwrap();
    let comparison = function.dfg.first_result(comparison);
    let target = function.dfg.make_block();
    let output = function.dfg.append_block_param(target, types::I8);
    function.dfg.append_block_param(target, types::I8);
    let mut cursor = FuncCursor::new(&mut function);
    cursor.goto_inst(terminal);
    cursor.remove_inst();
    cursor.goto_bottom(source);
    cursor
        .ins()
        .jump(target, &[selected.into(), comparison.into()]);
    cursor.insert_block(target);
    cursor.ins().return_(&[output]);
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    cranelift_codegen::verify_function(&function, &flags).unwrap();
    let before = function.clone();
    assert_eq!(rewrite(&mut function), 0);
    assert_eq!(function, before);
}

#[cfg(not(miri))]
#[test]
fn native_loop_direction_matches_control_and_model_at_all_boundaries() {
    let snapshot = Snapshot {
        prepared_keys: None,
        operations: resources::owned(&[
            Operation::NumericForLoop {
                base: RegisterIndex(0),
                jump: 1,
            },
            Operation::Return {
                start: RegisterIndex(0),
                count: VarCount::constant(0),
            },
            Operation::Return {
                start: RegisterIndex(3),
                count: VarCount::constant(1),
            },
        ]),
        constants: resources::owned(&[]),
        registers: 4,
        upvalues: 0,
        prototypes: 0,
    };
    snapshot.verify().unwrap();
    let memory = resources::MappingCounter::new(resources::Ledger::new(usize::MAX));
    let control = backend::compile(&snapshot, memory.clone(), 8 * 1024 * 1024).unwrap();
    let candidate = backend::compile_in(
        &snapshot,
        memory.clone(),
        8 * 1024 * 1024,
        resources::BudgetAllocator(resources::Ledger::new(32 * 1024 * 1024)),
        work::Limits::from(&JitConfig::default()),
        Failure::BranchLoopDirection,
    )
    .unwrap();
    let mut inputs = vec![
        Slot {
            tag: abi::NIL,
            bits: 0,
        },
        Slot {
            tag: abi::REFERENCE,
            bits: 0,
        },
        Slot {
            tag: abi::BOOLEAN,
            bits: 1,
        },
    ];
    inputs.extend(
        [
            i64::MIN,
            i64::MIN + 1,
            -2,
            -1,
            0,
            1,
            2,
            i64::MAX - 1,
            i64::MAX,
        ]
        .map(|value| Slot {
            tag: abi::INTEGER,
            bits: value as u64,
        }),
    );
    inputs.extend(
        [
            f64::NEG_INFINITY,
            -9223372036854775808.0,
            -1.5,
            -0.0,
            0.0,
            1.5,
            9223372036854775808.0,
            f64::INFINITY,
            f64::NAN,
        ]
        .map(|value| Slot {
            tag: abi::NUMBER,
            bits: value.to_bits(),
        }),
    );
    let mut invocations = 0;
    for &index in &inputs {
        for &limit in &inputs {
            for &step in &inputs {
                for pc in [0, 1, 2, 3, usize::MAX] {
                    for budget in [0, 1, 2, 64, u32::MAX] {
                        let initial = [
                            index,
                            limit,
                            step,
                            Slot {
                                tag: abi::REFERENCE,
                                bits: 123,
                            },
                        ];
                        let mut a = initial;
                        let mut b = initial;
                        let mut expected = initial;
                        let a_exit = control.invoke(&mut a, pc, budget);
                        let b_exit = candidate.invoke(&mut b, pc, budget);
                        let expected_exit = model::run(&snapshot, &mut expected, pc, budget);
                        let exit = |e: abi::Exit| (e.pc, e.instructions, e.reason);
                        let actual_exit = exit(b_exit);
                        assert_eq!(exit(a_exit), actual_exit);
                        assert_eq!(exit(expected_exit), actual_exit);
                        assert_eq!(a.map(|s| (s.tag, s.bits)), b.map(|s| (s.tag, s.bits)));
                        for (actual, expected) in b.into_iter().zip(expected) {
                            assert_eq!(actual.tag, expected.tag);
                            if actual.tag != abi::NUMBER || !f64::from_bits(actual.bits).is_nan() {
                                assert_eq!(actual.bits, expected.bits);
                            } else {
                                assert!(f64::from_bits(expected.bits).is_nan());
                            }
                        }
                        invocations += 1;
                    }
                }
            }
        }
    }
    assert_eq!(invocations, 231525);
    drop((control, candidate));
    assert_eq!(memory.load(std::sync::atomic::Ordering::Relaxed), 0);
}
