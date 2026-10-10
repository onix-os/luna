use cranelift_codegen::{
    cursor::{Cursor, FuncCursor},
    ir::{BlockArg, FuncRef, Function, InstructionData, Opcode, Signature},
};

use super::*;
use crate::jit::abi::payload::runtime::Bridge;
#[cfg(not(miri))]
use crate::jit::abi::payload::Payload;

mod reads;
mod writes;

enum Rewrite {
    Read(Inst, u32),
    Write {
        tag_store: Inst,
        bits_store: Inst,
        index: u32,
        tag: IrValue,
        bits: IrValue,
    },
    Helper(Inst, usize, FuncRef),
}

fn invalid() -> JitError {
    JitError::Compilation("invalid payload transport grammar".into())
}

pub(super) fn expansion(
    source: &Snapshot,
    limits: super::super::work::Limits,
) -> Result<super::super::work::Expansion, JitError> {
    let mut bound = super::super::work::Expansion::admit(source, limits)?;
    for operation in &source.operations {
        if let Operation::LoadNil { count, .. } = operation {
            bound.instructions = bound
                .instructions
                .checked_add(32 * usize::from(*count))
                .ok_or(JitError::ResourceLimit("payload expansion overflow"))?;
            bound.blocks = bound
                .blocks
                .checked_add(7 * usize::from(*count))
                .ok_or(JitError::ResourceLimit("payload expansion overflow"))?;
        }
    }
    if bound.instructions > limits.instructions {
        return Err(JitError::ResourceLimit("IR instructions"));
    }
    if bound.blocks > limits.blocks {
        return Err(JitError::ResourceLimit("IR blocks"));
    }
    Ok(bound)
}

#[test]
fn payload_nil_fanout_expansion_respects_original_host_limits() {
    crate::Lua::empty().enter(|ctx| {
        let closure =
            crate::Closure::load(ctx, None, b"local a,b,c,d,e,f,g return 'anchored'").unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 2 * 1024 * 1024).unwrap();
        let limits = super::super::work::Limits::from(&super::super::JitConfig::default());
        let original = super::super::work::Expansion::admit(&source, limits).unwrap();
        let actual = expansion(&source, limits).unwrap();
        assert_eq!(actual.instructions, original.instructions + 32 * 7);
        assert_eq!(actual.blocks, original.blocks + 7 * 7);
        for (instructions, blocks, accepted) in [
            (actual.instructions - 1, actual.blocks, false),
            (actual.instructions, actual.blocks - 1, false),
            (actual.instructions, actual.blocks, true),
        ] {
            let result = expansion(
                &source,
                super::super::work::Limits {
                    instructions,
                    blocks,
                    ..limits
                },
            );
            if accepted {
                assert_eq!(result.unwrap(), actual);
            } else {
                assert!(matches!(result, Err(JitError::ResourceLimit(_))));
            }
        }
    });
}

fn scan(
    function: &Function,
    slots: IrValue,
    host: IrValue,
    registers: usize,
    helpers: &[(u32, FuncRef)],
    mut visit: impl FnMut(Rewrite) -> Result<(), JitError>,
) -> Result<(), JitError> {
    if registers > 256 {
        return Err(invalid());
    }
    for block in function.layout.blocks() {
        let mut instructions = function.layout.block_insts(block);
        while let Some(inst) = instructions.next() {
            let data = function.dfg.insts[inst];
            if matches!(data.opcode(), Opcode::CallIndirect | Opcode::ReturnCall | Opcode::ReturnCallIndirect)
                || function.dfg.inst_args(inst).iter().any(|&value| value != slots && function.dfg.resolve_aliases(value) == slots)
                || data.branch_destination(&function.dfg.jump_tables, &function.dfg.exception_tables).iter().any(|branch| branch.args(&function.dfg.value_lists).any(|arg| matches!(arg, BlockArg::Value(value) if function.dfg.resolve_aliases(value) == slots)))
            { return Err(invalid()); }
            match data {
                InstructionData::Call {
                    opcode: Opcode::Call,
                    func_ref,
                    ..
                } => {
                    let (kind, _) = helpers
                        .iter()
                        .find(|(_, reference)| *reference == func_ref)
                        .ok_or_else(invalid)?;
                    let args = function.dfg.inst_args(inst);
                    if !(1..=10).contains(kind)
                        || args.len() != 6
                        || args[0] != host
                        || args[1] != slots
                    {
                        return Err(invalid());
                    }
                    visit(Rewrite::Helper(inst, (*kind - 1) as usize, func_ref))?;
                }
                InstructionData::Load {
                    opcode: Opcode::Load,
                    arg,
                    offset,
                    flags,
                } if arg == slots => {
                    let offset = i32::from(offset);
                    if offset < 0
                        || offset % 8 != 0
                        || offset as usize / 16 >= registers
                        || !matches!(
                            (
                                offset % 16,
                                function.dfg.value_type(function.dfg.first_result(inst))
                            ),
                            (_, types::I64) | (8, types::F64)
                        )
                        || function.dfg.mem_flags[flags] != MemFlagsData::new()
                    {
                        return Err(invalid());
                    }
                    if offset % 16 == 8 {
                        visit(Rewrite::Read(inst, offset as u32 / 16))?;
                    }
                }
                InstructionData::Store {
                    opcode: Opcode::Store,
                    args,
                    offset,
                    flags,
                } if args[1] == slots => {
                    let offset = i32::from(offset);
                    let bits_store = instructions.next().ok_or_else(invalid)?;
                    let InstructionData::Store {
                        opcode: Opcode::Store,
                        args: second,
                        offset: second_offset,
                        flags: second_flags,
                    } = function.dfg.insts[bits_store]
                    else {
                        return Err(invalid());
                    };
                    if offset < 0
                        || offset % 16 != 0
                        || offset as usize / 16 >= registers
                        || second[1] != slots
                        || i32::from(second_offset) != offset + 8
                        || function.dfg.value_type(args[0]) != types::I64
                        || !matches!(function.dfg.value_type(second[0]), types::I64 | types::F64)
                        || function.dfg.mem_flags[flags] != MemFlagsData::new()
                        || function.dfg.mem_flags[second_flags] != MemFlagsData::new()
                    {
                        return Err(invalid());
                    }
                    visit(Rewrite::Write {
                        tag_store: inst,
                        bits_store,
                        index: offset as u32 / 16,
                        tag: args[0],
                        bits: second[0],
                    })?;
                }
                _ if function.dfg.inst_args(inst).contains(&slots) => return Err(invalid()),
                _ => {}
            }
        }
    }
    Ok(())
}

fn shape<const N: usize>(
    mut instructions: impl Iterator<Item = Inst>,
) -> Result<[Inst; N], JitError> {
    let result = super::super::arrays::try_array(|_| instructions.next().ok_or_else(invalid))?;
    if instructions.next().is_some() {
        return Err(invalid());
    }
    Ok(result)
}

fn size(function: &Function) -> (usize, usize) {
    (
        function
            .layout
            .blocks()
            .map(|block| function.layout.block_insts(block).count())
            .sum(),
        function.layout.blocks().count(),
    )
}

struct Plan {
    reads: usize,
    writes: usize,
    instructions: usize,
    blocks: usize,
}

fn plan(
    function: &Function,
    slots: IrValue,
    host: IrValue,
    registers: usize,
    helpers: &[(u32, FuncRef)],
    expansion: super::super::work::Expansion,
) -> Result<Plan, JitError> {
    let (original_instructions, original_blocks) = size(function);
    let mut plan = Plan {
        reads: 0,
        writes: 0,
        instructions: original_instructions,
        blocks: original_blocks,
    };
    let add = |left: usize, right: usize| {
        left.checked_add(right)
            .ok_or(JitError::ResourceLimit("payload expansion overflow"))
    };
    scan(function, slots, host, registers, helpers, |rewrite| {
        let (instructions, blocks) = match rewrite {
            Rewrite::Read(inst, _) => {
                plan.reads = add(plan.reads, 1)?;
                (
                    20 + usize::from(
                        function.dfg.value_type(function.dfg.first_result(inst)) == types::F64,
                    ),
                    5,
                )
            }
            Rewrite::Write { bits, .. } => {
                plan.writes = add(plan.writes, 1)?;
                (
                    30 + usize::from(function.dfg.value_type(bits) == types::F64),
                    7,
                )
            }
            Rewrite::Helper(..) => (1, 0),
        };
        plan.instructions = add(plan.instructions, instructions)?;
        plan.blocks = add(plan.blocks, blocks)?;
        Ok(())
    })?;
    expansion.verify_actual(
        add(original_instructions, plan.instructions)?,
        add(original_blocks, plan.blocks)?,
    )?;
    Ok(plan)
}

pub(super) fn lower(
    function: &mut Function,
    slots: IrValue,
    host: IrValue,
    registers: usize,
    helpers: &[(u32, FuncRef)],
    allocator: BudgetAllocator,
    expansion: super::super::work::Expansion,
) -> Result<(), JitError> {
    let plan = plan(function, slots, host, registers, helpers, expansion)?;
    let refused = || JitError::ResourceLimit("payload rewrite records");
    let mut reads = BudgetVec::new_in(allocator.clone());
    reads.try_reserve_exact(plan.reads).map_err(|_| refused())?;
    let mut writes = BudgetVec::new_in(allocator);
    writes
        .try_reserve_exact(plan.writes)
        .map_err(|_| refused())?;
    let mut candidate = function.clone();
    let mut write = Signature::new(function.signature.call_conv);
    write
        .params
        .try_reserve_exact(5)
        .map_err(|_| JitError::ResourceLimit("payload signature"))?;
    write
        .params
        .extend([types::I64, types::I64, types::I32, types::I64, types::I64].map(AbiParam::new));
    let write = candidate.import_signature(write);
    scan(function, slots, host, registers, helpers, |rewrite| {
        match rewrite {
            Rewrite::Read(inst, index) => {
                reads.push(reads::emit(&mut candidate, inst, slots, index));
            }
            Rewrite::Write {
                tag_store,
                bits_store,
                index,
                tag,
                bits,
            } => {
                writes.push(writes::emit(
                    &mut candidate,
                    tag_store,
                    bits_store,
                    writes::Input {
                        slots,
                        host,
                        index,
                        tag,
                        bits,
                        signature: write,
                    },
                ));
            }
            Rewrite::Helper(inst, index, reference) => {
                let args: [IrValue; 6] = candidate
                    .dfg
                    .inst_args(inst)
                    .try_into()
                    .map_err(|_| invalid())?;
                let signature = candidate.dfg.ext_funcs[reference].signature;
                let mut cursor = FuncCursor::new(&mut candidate);
                cursor.goto_inst(inst);
                let target = cursor.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    host,
                    (std::mem::offset_of!(Bridge, helpers)
                        + index * std::mem::size_of::<abi::HelperEntry>())
                        as i32,
                );
                cursor
                    .func
                    .replace(inst)
                    .call_indirect(signature, target, &args);
            }
        }
        Ok(())
    })?;
    if size(&candidate) != (plan.instructions, plan.blocks) {
        return Err(JitError::Compilation(
            "payload expansion differs from plan".into(),
        ));
    }
    for read in &reads {
        reads::verify(&candidate, read, slots)?;
    }
    for write in &writes {
        writes::verify(&candidate, write)?;
    }
    *function = candidate;
    Ok(())
}

fn grammar_fixture(fault: u8) -> (Function, IrValue, IrValue) {
    let mut function = Function::new();
    function.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    function
        .signature
        .params
        .extend([types::I64, types::I64, types::I32, types::I64, types::I64].map(AbiParam::new));
    let mut frontend = FunctionBuilderContext::new();
    let (slots, host);
    {
        let mut b = FunctionBuilder::new(&mut function, &mut frontend);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        slots = b.block_params(entry)[0];
        host = b.block_params(entry)[4];
        let offset = match fault {
            1 => 7,
            2 => 24,
            _ => 8,
        };
        let ty = if fault == 3 { types::I32 } else { types::I64 };
        let bits = b.ins().load(ty, MemFlagsData::new(), slots, offset);
        let bits = if fault == 3 {
            b.ins().uextend(types::I64, bits)
        } else {
            bits
        };
        let tag = b.ins().iconst(types::I64, abi::INTEGER as i64);
        if fault != 8 {
            b.ins().store(
                MemFlagsData::new(),
                tag,
                slots,
                if fault == 4 { 16 } else { 0 },
            );
        }
        if fault == 5 {
            b.ins().iconst(types::I64, 9);
        }
        if fault != 9 {
            b.ins().store(
                MemFlagsData::new(),
                bits,
                slots,
                if fault == 6 { 16 } else { 8 },
            );
        }
        if fault == 7 {
            let offset = b.ins().iconst(types::I64, 8);
            b.ins().iadd(slots, offset);
        }
        if fault == 10 {
            let next = b.create_block();
            b.append_block_param(next, types::I64);
            b.ins().jump(next, &[slots.into()]);
            b.switch_to_block(next);
        }
        b.ins().return_(&[]);
        b.seal_all_blocks();
        let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
            .unwrap()
            .finish(settings::Flags::new(settings::builder()))
            .unwrap();
        b.finalize(isa.frontend_config());
    }
    (function, slots, host)
}

#[test]
fn transport_grammar_refuses_malformed_accesses_without_mutating_source() {
    let flags = settings::Flags::new(settings::builder());
    for fault in 0..=10 {
        let (mut function, slots, host) = grammar_fixture(fault);
        cranelift_codegen::verify_function(&function, &flags).unwrap();
        let original = function.clone();
        let allocator = BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024));
        let result = lower(
            &mut function,
            slots,
            host,
            1,
            &[],
            allocator.clone(),
            super::super::work::Expansion {
                instructions: 4096,
                blocks: 4096,
            },
        );
        assert_eq!(allocator.0.current(), 0);
        if fault == 0 {
            result.unwrap();
            cranelift_codegen::verify_function(&function, &flags).unwrap();
        } else {
            assert!(result.is_err(), "fault {fault}");
            assert_eq!(function, original);
        }
    }
}

#[test]
fn payload_workspace_quota_refusals_leave_source_and_ledger_unchanged() {
    let (source, slots, host) = grammar_fixture(0);
    let expansion = super::super::work::Expansion {
        instructions: 4096,
        blocks: 4096,
    };
    let required = std::mem::size_of::<reads::Read>() + std::mem::size_of::<writes::Write>();
    for (limit, allocations, accepted) in [
        (0, usize::MAX, false),
        (required - 1, usize::MAX, false),
        (required, 0, false),
        (required, 1, false),
        (required, 2, true),
    ] {
        let allocator = BudgetAllocator(super::super::resources::Ledger::new(limit));
        allocator.0.fail_after(allocations);
        let mut function = source.clone();
        let result = lower(
            &mut function,
            slots,
            host,
            1,
            &[],
            allocator.clone(),
            expansion,
        );
        assert_eq!(allocator.0.current(), 0);
        if accepted {
            result.unwrap();
            assert_eq!(allocator.0.peak(), required);
            cranelift_codegen::verify_function(
                &function,
                &settings::Flags::new(settings::builder()),
            )
            .unwrap();
        } else {
            assert!(matches!(
                result,
                Err(JitError::ResourceLimit("payload rewrite records"))
            ));
            assert_eq!(function, source);
        }
    }
}

#[test]
fn payload_expansion_is_admitted_before_workspace_or_source_changes() {
    let (source, slots, host) = grammar_fixture(0);
    let generous = super::super::work::Expansion {
        instructions: 4096,
        blocks: 4096,
    };
    let plan = plan(&source, slots, host, 1, &[], generous).unwrap();
    let (instructions, blocks) = size(&source);
    for (instructions, blocks, accepted) in [
        (
            instructions + plan.instructions - 1,
            blocks + plan.blocks,
            false,
        ),
        (
            instructions + plan.instructions,
            blocks + plan.blocks - 1,
            false,
        ),
        (instructions + plan.instructions, blocks + plan.blocks, true),
    ] {
        let allocator = BudgetAllocator(super::super::resources::Ledger::new(4096));
        let mut function = source.clone();
        let expansion = super::super::work::Expansion {
            instructions,
            blocks,
        };
        let result = lower(
            &mut function,
            slots,
            host,
            1,
            &[],
            allocator.clone(),
            expansion,
        );
        assert_eq!(allocator.0.current(), 0);
        if accepted {
            result.unwrap();
            assert_eq!(size(&function), (plan.instructions, plan.blocks));
        } else {
            assert!(
                matches!(result, Err(JitError::Compilation(message)) if message == "IR expansion exceeds admitted bound")
            );
            assert_eq!(allocator.0.peak(), 0);
            assert_eq!(function, source);
        }
    }
}

#[cfg(not(miri))]
struct Compiled(Code);

#[test]
fn incompatible_payload_selections_refuse_before_allocating_native_memory() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return 42").unwrap();
        let snapshot = Snapshot::new(&closure.prototype(), 4096, 2 * 1024 * 1024).unwrap();
        let baseline = snapshot.operations.allocator().0.current();
        let total = MappingCounter::new(super::super::resources::Ledger::new(8 * 1024 * 1024));
        let metadata = BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024));
        for failure in [Failure::RequirePayload, Failure::RequirePayloadLoop] {
            for mode in 0..6 {
                #[cfg(miri)]
                if mode == 5 {
                    continue;
                }
                let selection = Selection {
                    projected: mode == 0,
                    leaf: mode == 1,
                    cell_kernel: mode == 2,
                    integer_activation: mode == 3,
                    #[cfg(not(miri))]
                    scoped_helpers: mode == 5,
                    failure,
                };
                let result = compile_selected_rooted(
                    &snapshot,
                    total.clone(),
                    1024 * 1024,
                    metadata.clone(),
                    super::super::work::Limits::from(&super::super::JitConfig::default()),
                    selection,
                    mode == 4,
                );
                assert!(matches!(result, Err(JitError::Compilation(message))
                    if message == "payload transport requires ordinary lowering"));
                assert_eq!(total.load(Ordering::Relaxed), 0);
                assert_eq!(metadata.0.current(), 0);
                assert_eq!(snapshot.operations.allocator().0.current(), baseline);
            }
        }
    });
}

#[cfg(not(miri))]
impl Code {
    pub(in crate::jit) fn invoke_payload(
        &self,
        frame: &mut helpers::Frame<'_, '_, '_, '_>,
        pc: usize,
        budget: u32,
    ) -> Exit {
        assert!(self.payload);
        assert_eq!(frame.slot_count, self.registers);
        type Entry = unsafe extern "C" fn(*mut Payload, u64, u32, *mut Exit, *mut Bridge);
        let entry: Entry = unsafe { std::mem::transmute(self.entry) };
        let mut exit = Exit::default();
        crate::jit::abi::payload::runtime::with_bridge(frame, |slots, host| unsafe {
            entry(slots, pc as u64, budget.min(64), &mut exit, host);
        });
        exit
    }
}

#[cfg(not(miri))]
fn run<'gc>(
    code: &Code,
    payload: Option<&Compiled>,
    (ctx, closure): (crate::Context<'gc>, crate::Closure<'gc>),
    values: &mut [crate::Value<'gc>],
    pc: usize,
    budget: u32,
) -> (Exit, [u64; 8]) {
    let mut helper_pc = pc;
    crate::thread::LuaRegisters::with_test_frame(ctx, &mut helper_pc, values, |mut registers| {
        let mut scratch: Vec<_> = registers
            .stack_frame
            .iter()
            .copied()
            .map(Slot::from_value)
            .collect();
        let mut frame = helpers::Frame {
            ctx,
            closure,
            registers: &mut registers,
            count: helpers::Counts::default(),
            slot_count: code.registers,
            panic: None,
            projection: None,
        };
        let exit = if let Some(payload) = payload {
            payload.0.invoke_payload(&mut frame, pc, budget)
        } else {
            let mut host = abi::Host {
                data: std::ptr::from_mut(&mut frame).cast(),
                projection: std::ptr::null_mut(),
            };
            let exit = unsafe { code.invoke_host(&mut scratch, pc, budget, &mut host) };
            for (slot, value) in scratch
                .into_iter()
                .zip(frame.registers.stack_frame.iter_mut())
            {
                slot.write_back(value);
            }
            exit
        };
        assert!(frame.panic.is_none());
        (
            exit,
            [
                frame.count.calls,
                frame.count.completed,
                frame.count.declined,
                frame.count.table_reads,
                frame.count.table_writes,
                frame.count.upvalue_reads,
                frame.count.upvalue_writes,
                frame.count.allocations,
            ],
        )
    })
}

#[cfg(not(miri))]
#[test]
fn ordinary_lua_lowering_preserves_each_exit_and_canonical_register() {
    for (source, result) in [
        (
            "local x = 1; for i = 1, 20 do x = x + i end; return x",
            211.0,
        ),
        (
            "local x = 0.5; for i = 1, 20 do x = x + 1.5 end; return x",
            30.5,
        ),
        ("local t = {}; t[1] = 7; local x = t[1] + 2; return x", 9.0),
        (
            "local x = true; if x then x = -7 else x = 9 end; return x",
            -7.0,
        ),
    ] {
        crate::Lua::empty().enter(|ctx| {
            let closure = crate::Closure::load(ctx, None, source.as_bytes()).unwrap();
            let snapshot = Snapshot::new(&closure.prototype(), 4096, 2 * 1024 * 1024).unwrap();
            let total = MappingCounter::new(super::super::resources::Ledger::new(8 * 1024 * 1024));
            let control = compile(&snapshot, total.clone(), 1024 * 1024).unwrap();
            let candidate = Compiled(
                compile_in(
                    &snapshot,
                    total.clone(),
                    1024 * 1024,
                    BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024)),
                    super::super::work::Limits::from(&super::super::JitConfig::default()),
                    if source.contains("for i") {
                        Failure::RequirePayloadLoop
                    } else {
                        Failure::RequirePayload
                    },
                )
                .unwrap(),
            );
            let rejected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                candidate.0.invoke(
                    &mut vec![Slot::from_value(crate::Value::Nil); snapshot.registers],
                    0,
                    64,
                );
            }));
            assert!(rejected.is_err());
            for initial_budget in [0, 1, 63, 64, 65, u32::MAX] {
                let mut control_values = vec![crate::Value::Nil; snapshot.registers];
                let mut candidate_values = control_values.clone();
                let mut pc = 0;
                let mut ended = false;
                let mut native = 0;
                for turn in 0..256 {
                    let budget = if turn == 0 {
                        initial_budget
                    } else {
                        [0, 1, 3, 64][turn % 4]
                    };
                    let (expected, expected_counts) = run(
                        &control,
                        None,
                        (ctx, closure),
                        &mut control_values,
                        pc,
                        budget,
                    );
                    let (actual, actual_counts) = run(
                        &candidate.0,
                        Some(&candidate),
                        (ctx, closure),
                        &mut candidate_values,
                        pc,
                        budget,
                    );
                    assert_eq!(
                        (actual.pc, actual.instructions, actual.reason),
                        (expected.pc, expected.instructions, expected.reason),
                        "{source}, turn {turn}"
                    );
                    assert_eq!(actual_counts, expected_counts);
                    for (&actual, &expected) in candidate_values.iter().zip(&control_values) {
                        let actual = Slot::from_value(actual);
                        let expected = Slot::from_value(expected);
                        assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
                    }
                    pc = actual.pc as usize;
                    native += actual.instructions;
                    if actual.reason == ExitKind::Interpreter as u32 {
                        ended = true;
                        break;
                    }
                }
                assert!(ended, "{source}");
                assert!(native > 0);
                let Operation::Return { start, .. } = snapshot.operations[pc] else {
                    panic!("early fallback at {pc}: {source}");
                };
                let actual = candidate_values[usize::from(start.0)];
                match actual {
                    crate::Value::Integer(value) => assert_eq!(value as f64, result),
                    crate::Value::Number(value) => assert_eq!(value, result),
                    _ => panic!("unexpected result: {actual:?}"),
                }
            }
            drop((control, candidate));
            assert_eq!(total.load(Ordering::Relaxed), 0);
        });
    }
}

#[cfg(not(miri))]
#[test]
fn optimized_payload_loops_match_every_entry_budget_and_scalar_guard() {
    for (source, floating) in [
        ("local s=0 for i=1,20 do s=s+i end return s", false),
        ("local s=0 for i=20,1,-1 do s=s-i end return s", false),
        ("local s=0.0 for i=1,20 do s=s+0.5 end return s", true),
        ("local s=0.0 for i=20,1,-1 do s=i-s*0.5 end return s", true),
    ] {
        crate::Lua::empty().enter(|ctx| {
            let closure = crate::Closure::load(ctx, None, source.as_bytes()).unwrap();
            let snapshot = Snapshot::new(&closure.prototype(), 4096, 2 * 1024 * 1024).unwrap();
            let total = MappingCounter::new(super::super::resources::Ledger::new(8 * 1024 * 1024));
            let metadata = BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024));
            let work = super::super::work::Limits::from(&super::super::JitConfig::default());
            let control = compile_in(&snapshot, total.clone(), 1024 * 1024, metadata.clone(), work, Failure::RequireIntegerLoop).unwrap();
            let candidate = Compiled(compile_in(&snapshot, total.clone(), 1024 * 1024, metadata.clone(), work, Failure::RequirePayloadLoop).unwrap());
            let base = snapshot.operations.iter().find_map(|op| match op {
                Operation::NumericForLoop { base, .. } => Some(usize::from(base.0)),
                _ => None,
            }).unwrap();
            let accumulator = snapshot.operations.iter().find_map(|op| match op {
                Operation::Add { dest, .. } | Operation::Sub { dest, .. } => Some(usize::from(dest.0)),
                _ => None,
            }).unwrap();
            for pc in (0..=snapshot.operations.len()).chain([usize::MAX]) {
                for budget in [0, 1, 2, 3, 63, 64, 65, u32::MAX] {
                    for invalid in [None, Some(base), Some(base + 1), Some(base + 2), Some(base + 3), Some(accumulator)] {
                        for value in [
                            crate::Value::Nil, crate::Value::Boolean(false), crate::Value::Boolean(true),
                            crate::Value::Integer(i64::MIN), crate::Value::Integer(i64::MAX),
                            crate::Value::Number(-0.0), crate::Value::Number(f64::INFINITY),
                            crate::Value::Number(f64::from_bits(0x7ff8000000000042)),
                        ] {
                            let initial = if floating { crate::Value::Number(0.5) } else { crate::Value::Integer(1) };
                            let mut expected_values = vec![initial; snapshot.registers];
                            expected_values[base..base + 4].fill(crate::Value::Integer(1));
                            if let Some(index) = invalid { expected_values[index] = value; }
                            let mut actual_values = expected_values.clone();
                            let (expected, expected_counts) = run(&control, None, (ctx, closure), &mut expected_values, pc, budget);
                            let (actual, actual_counts) = run(&candidate.0, Some(&candidate), (ctx, closure), &mut actual_values, pc, budget);
                            assert_eq!((actual.pc, actual.instructions, actual.reason), (expected.pc, expected.instructions, expected.reason), "{source}: pc={pc} budget={budget} invalid={invalid:?} value={value:?}");
                            assert_eq!(actual_counts, expected_counts);
                            for (actual, expected) in actual_values.into_iter().zip(expected_values) {
                                let actual = Slot::from_value(actual);
                                let expected = Slot::from_value(expected);
                                assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
                            }
                        }
                    }
                }
            }
            drop((control, candidate));
            assert_eq!(total.load(Ordering::Relaxed), 0);
            assert_eq!(metadata.0.current(), 0);
        });
    }
}
