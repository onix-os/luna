use super::*;
use crate::jit::{helpers, resources::Ledger, JitConfig};
use crate::{thread::LuaRegisters, Callback, CallbackReturn, Closure, Constant, Lua, Table, Value};

const SOURCE: &[u8] = b"local sum=0 for i=1,5000 do sum=sum+host_increment(i) end return sum";

fn same<'gc>(a: Value<'gc>, b: Value<'gc>) {
    match (a, b) {
        (Value::Nil, Value::Nil) => {}
        (Value::Boolean(a), Value::Boolean(b)) => assert_eq!(a, b),
        (Value::Integer(a), Value::Integer(b)) => assert_eq!(a, b),
        (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
        (Value::Function(a), Value::Function(b)) => assert_eq!(a, b),
        (Value::String(a), Value::String(b)) => {
            assert!(std::ptr::eq(a.as_bytes().as_ptr(), b.as_bytes().as_ptr()))
        }
        (Value::Table(a), Value::Table(b)) => assert_eq!(a, b),
        (Value::Thread(a), Value::Thread(b)) => assert_eq!(a, b),
        (Value::UserData(a), Value::UserData(b)) => assert_eq!(a, b),
        _ => panic!("native read value mismatch: {a:?} {b:?}"),
    }
}

fn execute<'gc>(
    ctx: crate::Context<'gc>,
    closure: Closure<'gc>,
    ordinary: &Code,
    read: Option<&ReadCode>,
    pc: usize,
    budget: u32,
    initial: &[Value<'gc>],
) -> (
    abi::Exit,
    std::vec::Vec<Value<'gc>>,
    std::vec::Vec<(u64, u64)>,
    [u64; 8],
) {
    let mut values = initial.to_vec();
    let mut cursor = pc;
    let (exit, slots, counts) =
        LuaRegisters::with_test_frame(ctx, &mut cursor, &mut values, |mut registers| {
            let mut slots: std::vec::Vec<_> =
                initial.iter().copied().map(Slot::from_value).collect();
            let mut frame = helpers::Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: helpers::Counts::default(),
                slot_count: initial.len(),
                panic: None,
                projection: None,
                read_hints: Some(&ordinary.entries),
            };
            let exit = if let Some(read) = read {
                unsafe {
                    read.plan
                        .invoke_fresh(read.entry, &mut frame, &mut slots, pc, budget)
                }
                .unwrap()
                .unwrap()
            } else {
                let mut host = abi::Host {
                    data: (&mut frame as *mut helpers::Frame<'_, '_, '_, '_>).cast(),
                    projection: std::ptr::null_mut(),
                };
                unsafe { ordinary.invoke_host(&mut slots, pc, budget, &mut host) }
            };
            assert!(frame.panic.is_none());
            *frame.registers.pc = exit.pc as usize;
            for (slot, value) in slots.iter().zip(frame.registers.stack_frame.iter_mut()) {
                slot.write_back(value);
            }
            let c = &frame.count;
            (
                exit,
                slots.iter().map(|s| (s.tag, s.bits)).collect(),
                [
                    c.calls,
                    c.completed,
                    c.declined,
                    c.table_reads,
                    c.table_writes,
                    c.upvalue_reads,
                    c.upvalue_writes,
                    c.allocations,
                ],
            )
        });
    (exit, values, slots, counts)
}

#[test]
fn actual_six_argument_code_matches_ordinary_at_every_pc_and_budget() {
    Lua::empty().enter(|ctx| {
        let env = Table::new(&ctx);
        env.set_field(
            ctx,
            "host_increment",
            Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return)),
        );
        let closure = Closure::load_with_env(ctx, None, SOURCE, env).unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
        let maps = MappingCounter::new(Ledger::new(usize::MAX));
        let ledger = Ledger::new(8 << 20);
        let ordinary = super::super::compile_in(
            &source,
            maps.clone(),
            8 << 20,
            BudgetAllocator(ledger.clone()),
            (&JitConfig::default()).into(),
            Failure::None,
        )
        .unwrap();
        let read = compile(
            &source,
            maps.clone(),
            8 << 20,
            BudgetAllocator(ledger.clone()),
            (&JitConfig::default()).into(),
        )
        .unwrap();
        assert_eq!(read.registers, source.registers);
        let Constant::String(key) =
            closure.prototype().constants[((read.plan.binding()[1]) & 255) as usize]
        else {
            panic!()
        };
        env.get_canonical_string(
            ctx,
            key,
            ordinary.entries.hint(read.plan.helper_pc() as usize),
        );
        let mut initial = vec![Value::Integer(0); source.registers];
        initial[2] = Value::Integer(3);
        initial[3] = Value::Integer(1);
        initial[4] = Value::Integer(1);
        let mut executed = 0;
        for pc in 0..=source.operations.len() + 1 {
            for budget in (0..=65).chain([u32::MAX]) {
                let expected = execute(ctx, closure, &ordinary, None, pc, budget, &initial);
                let actual = execute(ctx, closure, &ordinary, Some(&read), pc, budget, &initial);
                assert_eq!(
                    (actual.0.pc, actual.0.instructions, actual.0.reason),
                    (expected.0.pc, expected.0.instructions, expected.0.reason),
                    "pc={pc} budget={budget}"
                );
                assert_eq!(actual.2, expected.2, "pc={pc} budget={budget}");
                for (&a, &b) in actual.1.iter().zip(&expected.1) {
                    same(a, b);
                }
                assert_eq!(actual.3[0] + actual.3[3], expected.3[0]);
                assert_eq!(actual.3[1] + actual.3[3], expected.3[1]);
                assert_eq!(&actual.3[2..], &expected.3[2..]);
                executed += actual.3[3];
            }
        }
        assert!(executed > 100);
        drop((ordinary, read));
        assert_eq!(maps.load(Ordering::Relaxed), 0);
        assert_eq!(maps.requested(), 0);
        assert_eq!(ledger.current(), 0);
    });
}

#[test]
fn ordinary_constructor_refuses_the_distinct_entry_and_releases_ownership() {
    Lua::empty().enter(|ctx| {
        let closure=Closure::load(ctx,None,SOURCE).unwrap();
        let source=Snapshot::new(&closure.prototype(),4096,8<<20).unwrap();
        let maps=MappingCounter::new(Ledger::new(usize::MAX));
        let ledger=Ledger::new(8<<20);
        let result=super::super::compile_in(&source,maps.clone(),8<<20,BudgetAllocator(ledger.clone()),(&JitConfig::default()).into(),Failure::RequireReadonlyLookup);
        assert!(matches!(result,Err(JitError::Compilation(message)) if message=="non-ordinary native entry ABI"));
        assert_eq!(maps.load(Ordering::Relaxed),0);
        assert_eq!(maps.requested(),0);
        assert_eq!(ledger.current(),0);
    });
}

struct Fixture {
    function: Function,
    rewrite: Rewrite,
    witness: witness::Witness,
    old_store: Inst,
}

fn original_fixture() -> (Function, Inst, Inst, IrValue, IrValue) {
    use cranelift_codegen::ir::{ExtFuncData, ExternalName, Signature, UserExternalName};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
    let mut function = Function::new();
    function.signature.params.reserve_exact(6);
    function
        .signature
        .params
        .extend([types::I64, types::I64, types::I32, types::I64, types::I64].map(AbiParam::new));
    let mut signature = Signature::new(function.signature.call_conv);
    signature.params.push(AbiParam::new(types::I64));
    signature.returns.push(AbiParam::new(types::I32));
    let signature = function.import_signature(signature);
    let name = function.declare_imported_user_function(UserExternalName::new(0, 0));
    let helper = function.import_function(ExtFuncData {
        name: ExternalName::user(name),
        signature,
        colocated: false,
        patchable: false,
    });
    let mut context = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut function, &mut context);
    let root = builder.create_block();
    builder.append_block_params_for_function_params(root);
    builder.switch_to_block(root);
    let slots = builder.block_params(root)[0];
    let pc = builder.block_params(root)[1];
    let exit = builder.block_params(root)[3];
    let old_store = builder.ins().store(MemFlagsData::trusted(), pc, exit, 0);
    let call = builder.ins().call(helper, &[slots]);
    let result = builder.inst_results(call)[0];
    builder
        .ins()
        .store(MemFlagsData::trusted(), result, exit, 8);
    builder.ins().return_(&[]);
    builder.seal_all_blocks();
    drop(builder);
    (function, call, old_store, slots, exit)
}

fn fixture(source: &Snapshot) -> Fixture {
    let (mut function, call, old_store, slots, exit) = original_fixture();
    let witness =
        witness::Witness::new(&function, BudgetAllocator(Ledger::new(usize::MAX))).unwrap();
    let plan = Plan::recognize(source).unwrap();
    let rewrite = rewrite(&mut function, plan, call, slots, exit).unwrap();
    verify(&function, source, plan, &rewrite, &witness).unwrap();
    Fixture {
        function,
        rewrite,
        witness,
        old_store,
    }
}

fn corrupt(function: &mut Function, inst: Inst, rewrite: &Rewrite) {
    if function.dfg.insts[inst].opcode() == Opcode::Return {
        let zero = function.dfg.block_params(rewrite.root)[1];
        function
            .replace(inst)
            .store(MemFlagsData::trusted(), zero, rewrite.view, 32);
        return;
    }
    match &mut function.dfg.insts[inst] {
        InstructionData::UnaryImm { imm, .. } => *imm = (i64::from(*imm) ^ 1).into(),
        InstructionData::Load { offset, .. } | InstructionData::Store { offset, .. } => {
            *offset = (i32::from(*offset) + 8).into()
        }
        InstructionData::IntCompare { cond, .. } => {
            *cond = if *cond == IntCC::Equal {
                IntCC::NotEqual
            } else {
                IntCC::Equal
            }
        }
        InstructionData::Binary { opcode, .. } => *opcode = Opcode::Bor,
        InstructionData::Brif { blocks, .. } => blocks.swap(0, 1),
        other => panic!("uncovered lookup instruction: {other:?}"),
    }
}

#[test]
fn independent_checker_rejects_each_guard_and_effect_instruction_mutation() {
    Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, SOURCE).unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
        let initial = fixture(&source);
        let mut nodes: std::vec::Vec<_> = [
            initial.rewrite.root,
            initial.rewrite.check,
            initial.rewrite.refused,
        ]
        .into_iter()
        .flat_map(|block| initial.function.layout.block_insts(block))
        .collect();
        nodes.extend(initial.rewrite.body);
        nodes.push(initial.rewrite.call);
        assert_eq!(nodes.len(), 33);
        for inst in nodes {
            let mut f = fixture(&source);
            corrupt(&mut f.function, inst, &f.rewrite);
            assert!(
                verify(
                    &f.function,
                    &source,
                    Plan::recognize(&source).unwrap(),
                    &f.rewrite,
                    &f.witness
                )
                .is_err(),
                "accepted mutation at {inst:?}"
            );
        }
    });
}

#[test]
fn preservation_checker_refuses_extra_effects_abi_imports_and_parameter_rebinding() {
    Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, SOURCE).unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
        for case in 0..11 {
            let mut f = fixture(&source);
            match case {
                0..=3 => {
                    let block = [
                        f.rewrite.root,
                        f.rewrite.check,
                        f.rewrite.refused,
                        f.rewrite.original,
                    ][case];
                    let last = f.function.layout.last_inst(block).unwrap();
                    let value = f.function.dfg.block_params(f.rewrite.root)[1];
                    FuncCursor::new(&mut f.function).at_inst(last).ins().store(
                        MemFlagsData::trusted(),
                        value,
                        f.rewrite.view,
                        32,
                    );
                }
                4 => corrupt(&mut f.function, f.old_store, &f.rewrite),
                5 => f.function.signature.params.swap(0, 2),
                6 => {
                    let reference = f.function.dfg.ext_funcs.keys().next().unwrap();
                    f.function.dfg.ext_funcs[reference].colocated = true;
                }
                7 => {
                    let reference = f.function.dfg.signatures.keys().next().unwrap();
                    f.function.dfg.signatures[reference].params[0] = AbiParam::new(types::I32);
                }
                8 => {
                    f.function.declare_imported_user_function(
                        cranelift_codegen::ir::UserExternalName::new(1, 1),
                    );
                }
                9 => {
                    let inst = f.function.layout.last_inst(f.rewrite.check).unwrap();
                    let old = f.function.dfg.block_params(f.rewrite.root)[0];
                    let peer = f.function.dfg.block_params(f.rewrite.root)[4];
                    let dfg = &mut f.function.dfg;
                    let InstructionData::Brif { blocks, .. } = &mut dfg.insts[inst] else {
                        panic!()
                    };
                    blocks[0].update_args(&mut dfg.value_lists, |arg| {
                        if arg == BlockArg::Value(old) {
                            BlockArg::Value(peer)
                        } else {
                            arg
                        }
                    });
                }
                10 => {
                    f.function.layout.remove_inst(f.rewrite.body[5]);
                }
                _ => unreachable!(),
            }
            assert!(
                verify(
                    &f.function,
                    &source,
                    Plan::recognize(&source).unwrap(),
                    &f.rewrite,
                    &f.witness
                )
                .is_err(),
                "accepted preservation mutation {case}"
            );
        }
    });
}

#[test]
fn preservation_workspace_refusals_release_every_partial_allocation() {
    let (function, _, _, _, _) = original_fixture();
    let ledger = Ledger::new(usize::MAX);
    let witness = witness::Witness::new(&function, BudgetAllocator(ledger.clone())).unwrap();
    let peak = ledger.peak();
    assert!(peak > 1024);
    drop(witness);
    assert_eq!(ledger.current(), 0);
    for limit in [0, 1, 63, 127, 255, 511, 1023, peak - 1, peak] {
        let ledger = Ledger::new(limit);
        let result = witness::Witness::new(&function, BudgetAllocator(ledger.clone()));
        assert_eq!(result.is_ok(), limit == peak, "limit={limit} peak={peak}");
        drop(result);
        assert_eq!(ledger.current(), 0);
        assert_eq!(ledger.refusals(), usize::from(limit < peak));
    }
}

#[test]
fn actual_code_declines_bad_views_without_touching_slots_or_using_host() {
    use crate::jit::readonly_lookup::binding::View;
    Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, SOURCE).unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
        let code = compile(
            &source,
            MappingCounter::new(Ledger::new(usize::MAX)),
            8 << 20,
            BudgetAllocator(Ledger::new(8 << 20)),
            (&JitConfig::default()).into(),
        )
        .unwrap();
        for case in 0..6 {
            for pc in [0, code.plan.helper_pc() as u64, u64::MAX] {
                for budget in [0, 1, 64, u32::MAX] {
                    let mut slots = vec![
                        Slot {
                            tag: abi::INTEGER,
                            bits: 91
                        };
                        source.registers
                    ];
                    let mut view = View {
                        binding: code.plan.binding(),
                        slot: Slot::from_value(Value::Integer(7)),
                        executed: 0,
                    };
                    match case {
                        0 => {}
                        1 => view.binding[0] ^= 1,
                        2 => view.binding[1] ^= 1,
                        3 => view.executed = 1,
                        4 => view.slot.tag = abi::REFERENCE + 1,
                        5 => view.slot.tag = u64::MAX,
                        _ => unreachable!(),
                    }
                    let before = (view.binding, view.slot.tag, view.slot.bits, view.executed);
                    let mut exit = abi::Exit {
                        pc: 99,
                        instructions: 99,
                        reason: 99,
                    };
                    let pointer = if case == 0 {
                        std::ptr::null_mut()
                    } else {
                        &mut view
                    };
                    unsafe {
                        (code.entry)(
                            slots.as_mut_ptr(),
                            pc,
                            budget,
                            &mut exit,
                            std::ptr::null_mut(),
                            pointer,
                        )
                    };
                    assert_eq!(
                        (exit.pc, exit.instructions, exit.reason),
                        (pc, 0, crate::jit::exits::Kind::Interpreter as u32)
                    );
                    assert!(slots.iter().all(|s| s.tag == abi::INTEGER && s.bits == 91));
                    assert_eq!(
                        (view.binding, view.slot.tag, view.slot.bits, view.executed),
                        before
                    );
                }
            }
        }
    });
}

#[test]
fn actual_code_preserves_fresh_scalar_bits_and_reference_identity() {
    Lua::empty().enter(|ctx| {
        let env = Table::new(&ctx);
        let closure = Closure::load_with_env(ctx, None, SOURCE, env).unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
        let ordinary = super::super::compile_in(
            &source,
            MappingCounter::new(Ledger::new(usize::MAX)),
            8 << 20,
            BudgetAllocator(Ledger::new(8 << 20)),
            (&JitConfig::default()).into(),
            Failure::None,
        )
        .unwrap();
        let read = compile(
            &source,
            MappingCounter::new(Ledger::new(usize::MAX)),
            8 << 20,
            BudgetAllocator(Ledger::new(8 << 20)),
            (&JitConfig::default()).into(),
        )
        .unwrap();
        let Constant::String(key) =
            closure.prototype().constants[(read.plan.binding()[1] & 255) as usize]
        else {
            panic!()
        };
        let values = [
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8000000000055)),
            ctx.intern(b"value").into(),
            Table::new(&ctx).into(),
            closure.into(),
            Callback::from_fn(&ctx, |_, _, _| panic!("lookup invoked callback")).into(),
            crate::Thread::new(ctx).into(),
            crate::UserData::new_static(&ctx, ()).into(),
        ];
        for value in values {
            env.set(ctx, key, value).unwrap();
            env.get_canonical_string(
                ctx,
                key,
                ordinary.entries.hint(read.plan.helper_pc() as usize),
            );
            let initial = vec![Value::Integer(-7); source.registers];
            for budget in [0, 1, 2, 64] {
                let pc = read.plan.helper_pc() as usize;
                let expected = execute(ctx, closure, &ordinary, None, pc, budget, &initial);
                let actual = execute(ctx, closure, &ordinary, Some(&read), pc, budget, &initial);
                assert_eq!(
                    (actual.0.pc, actual.0.instructions, actual.0.reason),
                    (expected.0.pc, expected.0.instructions, expected.0.reason)
                );
                assert_eq!(actual.2, expected.2);
                for (&a, &b) in actual.1.iter().zip(&expected.1) {
                    same(a, b)
                }
                same(
                    actual.1[usize::from(read.plan.destination())],
                    if budget == 0 {
                        Value::Integer(-7)
                    } else {
                        value
                    },
                );
            }
        }
    });
}

#[test]
fn distinct_image_resource_refusals_leave_live_ordinary_code_owned() {
    Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, SOURCE).unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
        let maps = MappingCounter::new(Ledger::new(usize::MAX));
        let ledger = Ledger::new(8 << 20);
        let ordinary = super::super::compile_in(
            &source,
            maps.clone(),
            8 << 20,
            BudgetAllocator(ledger.clone()),
            (&JitConfig::default()).into(),
            Failure::None,
        )
        .unwrap();
        let before = (
            maps.load(Ordering::Relaxed),
            maps.requested(),
            ledger.current(),
        );
        let limits: crate::jit::work::Limits = (&JitConfig::default()).into();
        let bound = crate::jit::work::Expansion::admit(&source, limits).unwrap();
        for case in 0..5 {
            let mut work = limits;
            let mut mapping_limit = 8 << 20;
            let metadata = if case == 1 {
                Ledger::new(0)
            } else {
                ledger.clone()
            };
            match case {
                0 => mapping_limit = 0,
                1 => {}
                2 => work.instructions = bound.instructions + 63,
                3 => work.blocks = bound.blocks + 2,
                4 => work.relocations = 0,
                _ => unreachable!(),
            }
            let result = compile(
                &source,
                maps.clone(),
                mapping_limit,
                BudgetAllocator(metadata.clone()),
                work,
            );
            assert!(
                matches!(result, Err(JitError::ResourceLimit(_))),
                "resource case {case}"
            );
            assert_eq!(
                (
                    maps.load(Ordering::Relaxed),
                    maps.requested(),
                    ledger.current()
                ),
                before
            );
            if case == 1 {
                assert_eq!(metadata.current(), 0)
            }
        }
        let mut slots = vec![Slot::from_value(Value::Integer(3)); source.registers];
        let exit = ordinary.invoke(&mut slots, usize::MAX, 64);
        assert_eq!((exit.pc, exit.instructions), (usize::MAX as u64, 0));
        drop(ordinary);
        assert_eq!(
            (
                maps.load(Ordering::Relaxed),
                maps.requested(),
                ledger.current()
            ),
            (0, 0, 0)
        );
    });
}
