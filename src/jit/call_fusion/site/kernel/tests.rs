use super::*;
use crate::{
    jit::{
        backend::call_fusion as native,
        resources::{Ledger, MappingCounter},
    },
    Closure, JitConfig, Lua, Value,
};
use std::sync::atomic::Ordering;

const SOURCE: &[u8] =
    b"local amount=7 local sum=11 local function add(v) sum=sum+v end add(amount) return sum";

fn input(source: &[u8]) -> Input {
    Lua::empty().enter(|ctx| {
        let caller = Closure::load(ctx, None, source).unwrap();
        Input::new(
            &caller.prototype(),
            0,
            260,
            BudgetAllocator(Ledger::new(16 << 20)),
        )
        .unwrap()
        .unwrap()
    })
}

fn pairs(slots: &[abi::Slot]) -> Vec<(u64, u64)> {
    slots.iter().map(|s| (s.tag, s.bits)).collect()
}

fn slots(site: Site, sum: i64, argument: i64) -> Vec<abi::Slot> {
    let mut slots = vec![abi::Slot::from_value(Value::Integer(-91)); usize::from(site.registers)];
    if let Argument::Register(index) = site.argument {
        slots[usize::from(index)] = abi::Slot::from_value(Value::Integer(argument));
    }
    slots[usize::from(site.leaf.parent)] = abi::Slot::from_value(Value::Integer(sum));
    slots[usize::from(site.holder)] = abi::Slot::canonical();
    slots
}

fn limits() -> work::Limits {
    work::Limits::from(&JitConfig::default())
}

fn compare(input: &Input) {
    let plan = Plan::new(input).unwrap();
    let metadata = BudgetAllocator(Ledger::new(16 << 20));
    let mappings = MappingCounter::new(Ledger::new(16 << 20));
    let baseline = plan.allocator().0.current();
    let code =
        native::compile(&plan, mappings.clone(), 2 << 20, metadata.clone(), limits()).unwrap();
    assert!(code.bytes > 0);
    assert!(mappings.load(Ordering::Relaxed) > 0);
    for (sum, argument) in [
        (11, 7),
        (0, -1),
        (i64::MAX, 1),
        (i64::MIN, -1),
        (i64::MAX, i64::MIN),
    ] {
        for budget in [7, 8, 64, u32::MAX] {
            let mut actual = slots(plan.site, sum, argument);
            let mut expected = actual.clone();
            assert!(plan.site.materialize(&mut expected).is_some());
            let mut view = plan.view();
            let exit = code
                .invoke_scratch(
                    &mut actual,
                    u64::from(plan.site.region_pc),
                    budget,
                    Some(&mut view),
                )
                .unwrap();
            assert_eq!(pairs(&actual), pairs(&expected));
            assert_eq!(
                (exit.pc, exit.instructions, exit.reason, view.completed),
                (
                    u64::from(plan.site.return_pc),
                    5,
                    Kind::Interpreter as u32,
                    1
                )
            );
        }
    }
    drop(code);
    assert_eq!(plan.allocator().0.current(), baseline);
    assert_eq!(
        (
            metadata.0.current(),
            mappings.load(Ordering::Relaxed),
            mappings.requested()
        ),
        (0, 0, 0)
    );
}

#[test]
fn native_region_matches_all_slots_for_constants_registers_aliases_and_wraparound() {
    for source in [
        SOURCE,
        b"local sum=11 local function add(v) sum=sum+v end add(7) return sum".as_slice(),
        b"local sum=11 local function add(v) sum=sum+v end add(sum) return sum".as_slice(),
        b"local sum=0 local function add(v) sum=sum+v end for i=1,100 do add(i) end return sum"
            .as_slice(),
    ] {
        compare(&input(source));
    }
}

#[test]
fn native_region_preserves_callee_aliases_and_wide_caller_tails() {
    use crate::{
        compiler,
        opcode::{OpCode, Operation, RCIndex},
        types::{RegisterIndex, UpValueIndex, VarCount},
        FunctionPrototype,
    };
    for (width, read, result) in [
        (2, 1, 0),
        (2, 1, 1),
        (3, 1, 2),
        (3, 2, 1),
        (4, 3, 0),
        (8, 7, 6),
        (256, 255, 254),
    ] {
        for parent_width in [5, 12, 256] {
            let mut interner = compiler::interning::BasicInterner::default();
            let parsed = compiler::parse_chunk(SOURCE, &mut interner).unwrap();
            let mut compiled = compiler::compile_chunk(&parsed, &mut interner).unwrap();
            compiled.stack_size = parent_width;
            let child = &mut compiled.prototypes[0];
            child.stack_size = width;
            child.opcodes = vec![
                OpCode::encode(Operation::GetUpValue {
                    dest: RegisterIndex(read),
                    source: UpValueIndex(0),
                }),
                OpCode::encode(Operation::Add {
                    dest: RegisterIndex(result),
                    left: RCIndex::Register(RegisterIndex(read)),
                    right: RCIndex::Register(RegisterIndex(0)),
                }),
                OpCode::encode(Operation::SetUpValue {
                    dest: UpValueIndex(0),
                    source: RegisterIndex(result),
                }),
                OpCode::encode(Operation::Return {
                    start: RegisterIndex(0),
                    count: VarCount::constant(0),
                }),
            ];
            let input = Lua::empty().enter(|ctx| {
                let compiled = compiled.map_strings(|s| ctx.intern(s.as_ref()));
                let proto = FunctionPrototype::from_compiled(
                    &ctx,
                    ctx.intern(b"fusion-native-layout"),
                    &compiled,
                    false,
                );
                let caller = Closure::new(&ctx, proto, Some(ctx.globals())).unwrap();
                Input::new(
                    &caller.prototype(),
                    0,
                    260,
                    BudgetAllocator(Ledger::new(16 << 20)),
                )
                .unwrap()
                .unwrap()
            });
            compare(&input);
        }
    }
}

#[test]
fn native_guards_refuse_before_any_slot_or_marker_write() {
    let input = input(SOURCE);
    let plan = Plan::new(&input).unwrap();
    let metadata = BudgetAllocator(Ledger::new(16 << 20));
    let mappings = MappingCounter::new(Ledger::new(16 << 20));
    let code = native::compile(&plan, mappings, 2 << 20, metadata, limits()).unwrap();
    for case in 0..19 {
        let mut actual = slots(plan.site, 11, 7);
        let mut pc = u64::from(plan.site.region_pc);
        let mut budget = 64;
        let mut view = plan.view();
        match case {
            0..=3 => view.binding[case] ^= 1,
            4 => view.completed = 1,
            5 => pc = pc - 1,
            6 => pc += 1,
            7 => pc = u64::MAX,
            8..=14 => budget = (case - 8) as u32,
            15 => actual[usize::from(plan.site.holder)].tag = abi::INTEGER,
            16 => actual[usize::from(plan.site.holder)].bits = 1,
            17 => actual[usize::from(plan.site.leaf.parent)].tag = abi::NUMBER,
            18 => {
                let Argument::Register(index) = plan.site.argument else {
                    panic!()
                };
                actual[usize::from(index)].tag = abi::NUMBER;
            }
            _ => unreachable!(),
        }
        let before = pairs(&actual);
        let original_view = (view.binding, view.completed);
        let exit = code
            .invoke_scratch(&mut actual, pc, budget, Some(&mut view))
            .unwrap();
        assert_eq!(
            (exit.pc, exit.instructions, exit.reason),
            (pc, 0, Kind::Guard as u32),
            "case={case}"
        );
        assert_eq!(pairs(&actual), before, "case={case}");
        assert_eq!((view.binding, view.completed), original_view);
    }
    let mut short = slots(plan.site, 11, 7);
    short.pop();
    let before = pairs(&short);
    assert!(code
        .invoke_scratch(&mut short, 0, 64, Some(&mut plan.view()))
        .is_err());
    assert_eq!(pairs(&short), before);
    let mut actual = slots(plan.site, 11, 7);
    let before = pairs(&actual);
    let exit = code
        .invoke_scratch(&mut actual, u64::from(plan.site.region_pc), 64, None)
        .unwrap();
    assert_eq!(
        (exit.pc, exit.instructions, exit.reason),
        (u64::from(plan.site.region_pc), 0, Kind::Guard as u32)
    );
    assert_eq!(pairs(&actual), before);
}

#[test]
fn native_template_and_resource_refusals_reclaim_every_charge() {
    let input = input(SOURCE);
    let plan = Plan::new(&input).unwrap();
    let metadata = BudgetAllocator(Ledger::new(16 << 20));
    let mappings = MappingCounter::new(Ledger::new(16 << 20));
    let baseline = plan.allocator().0.current();
    let workspace = plan.allocator();
    let previous_limit = workspace.0.limit();
    workspace.0.set_limit(baseline);
    assert!(native::compile(&plan, mappings.clone(), 2 << 20, metadata.clone(), limits()).is_err());
    workspace.0.set_limit(previous_limit);
    assert_eq!(workspace.0.current(), baseline);
    for case in 0..10 {
        let mut limits = limits();
        let mut code_limit = 2 << 20;
        match case {
            0 => limits.instructions = 0,
            1 => limits.blocks = 0,
            2 => code_limit = 0,
            _ => metadata.0.fail_after(case - 3),
        }
        let result = native::compile(
            &plan,
            mappings.clone(),
            code_limit,
            metadata.clone(),
            limits,
        );
        if case < 3 {
            assert!(result.is_err());
        }
        drop(result);
        metadata.0.fail_after(usize::MAX);
        assert_eq!(plan.allocator().0.current(), baseline, "case={case}");
        assert_eq!(
            (
                metadata.0.current(),
                mappings.load(Ordering::Relaxed),
                mappings.requested()
            ),
            (0, 0, 0),
            "case={case}"
        );
    }
    let isa = cranelift_native::builder().unwrap();
    let isa = isa
        .finish(cranelift_codegen::settings::Flags::new(
            cranelift_codegen::settings::builder(),
        ))
        .unwrap();
    let target = isa.frontend_config();
    let convention = isa.default_call_conv();
    let function = plan.program(target, convention);
    assert_eq!(function.signature.params.len(), 6);
    assert!(function.dfg.ext_funcs.is_empty());
    plan.verify(&function, target, convention).unwrap();
    let mut corrupt = function.clone();
    corrupt.signature.params.pop();
    assert!(plan.verify(&corrupt, target, convention).is_err());
    let mut corrupt = function.clone();
    corrupt.signature.returns.push(AbiParam::new(types::I64));
    assert!(plan.verify(&corrupt, target, convention).is_err());
    let mut corrupt = function.clone();
    corrupt.signature.params[2].value_type = types::I64;
    assert!(plan.verify(&corrupt, target, convention).is_err());
    let mut corrupt = function;
    let debit = corrupt.layout.blocks().flat_map(|b| corrupt.layout.block_insts(b))
        .find(|&inst| matches!(corrupt.dfg.insts[inst], cranelift_codegen::ir::InstructionData::UnaryImm { opcode: cranelift_codegen::ir::Opcode::Iconst, imm } if imm.bits() == 5)).unwrap();
    let cranelift_codegen::ir::InstructionData::UnaryImm { imm, .. } =
        &mut corrupt.dfg.insts[debit]
    else {
        unreachable!()
    };
    *imm = 7.into();
    assert!(plan.verify(&corrupt, target, convention).is_err());
}

#[test]
fn native_image_outlives_its_source_input_without_retaining_gc_objects() {
    let metadata = BudgetAllocator(Ledger::new(16 << 20));
    let mappings = MappingCounter::new(Ledger::new(16 << 20));
    let (code, site, mut view, source_ledger) = {
        let input = input(SOURCE);
        let plan = Plan::new(&input).unwrap();
        let code =
            native::compile(&plan, mappings.clone(), 2 << 20, metadata.clone(), limits()).unwrap();
        (code, plan.site, plan.view(), plan.allocator().0.clone())
    };
    assert_eq!(source_ledger.current(), 0);
    let mut values = slots(site, i64::MAX, 1);
    let mut expected = values.clone();
    assert!(site.materialize(&mut expected).is_some());
    let exit = code
        .invoke_scratch(&mut values, u64::from(site.region_pc), 64, Some(&mut view))
        .unwrap();
    assert_eq!(exit.instructions, 5);
    assert_eq!(pairs(&values), pairs(&expected));
    drop(code);
    assert_eq!(
        (
            metadata.0.current(),
            mappings.load(Ordering::Relaxed),
            mappings.requested()
        ),
        (0, 0, 0)
    );
}
