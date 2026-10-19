use super::super::native::KernelEntry;
use super::*;
use crate::{
    jit::{backend, helpers, resources::MappingCounter},
    table::RawTable,
    thread::LuaRegisters,
    Closure, Lua,
};
use cranelift_codegen::{
    ir::Opcode,
    settings::{self, Configurable},
};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Linkage, Module};

struct Code(Option<JITModule>);

impl Drop for Code {
    fn drop(&mut self) {
        unsafe { self.0.take().unwrap().free_memory() };
    }
}

fn with_kernel(source: &Snapshot, plan: Plan, body: impl FnOnce(KernelEntry)) {
    let mut flags = settings::builder();
    flags.set("opt_level", "speed").unwrap();
    flags.set("use_colocated_libcalls", "false").unwrap();
    let isa = cranelift_native::builder()
        .unwrap()
        .finish(settings::Flags::new(flags))
        .unwrap();
    let mut owner = Code(Some(JITModule::new(JITBuilder::with_isa(
        isa,
        default_libcall_names(),
    ))));
    let module = owner.0.as_mut().unwrap();
    let mut context = module.make_context();
    context.func = program(source, plan, module.isa()).unwrap();
    for block in context.func.layout.blocks() {
        for inst in context.func.layout.block_insts(block) {
            assert!(!matches!(
                context.func.dfg.insts[inst].opcode(),
                Opcode::Call
                    | Opcode::CallIndirect
                    | Opcode::ReturnCall
                    | Opcode::ReturnCallIndirect
            ));
        }
    }
    let id = module
        .declare_function("array_kernel", Linkage::Local, &context.func.signature)
        .unwrap();
    module.define_function(id, &mut context).unwrap();
    module.finalize_definitions().unwrap();
    let entry =
        unsafe { std::mem::transmute::<*const u8, KernelEntry>(module.get_finalized_function(id)) };
    body(entry);
}

fn array<'gc>(ctx: Context<'gc>, values: &[Value<'gc>]) -> Table<'gc> {
    let mut raw = RawTable::with_capacity(&ctx, values.len(), 0);
    raw.array_mut()[..values.len()].copy_from_slice(values);
    Table::from_parts(&ctx, raw, None)
}

fn same(left: &[Slot], right: &[Slot]) {
    assert_eq!(
        left.iter().map(|s| (s.tag, s.bits)).collect::<Vec<_>>(),
        right.iter().map(|s| (s.tag, s.bits)).collect::<Vec<_>>()
    );
}

#[test]
fn complete_native_loop_slices_match_the_existing_backend_at_every_pc_and_budget() {
    for text in [
        "local t={} for i=1,100 do t[i]=i end return t",
        "local t={} local sum=0 for i=1,100 do sum=sum+t[i] end return sum",
        "local t={} for i=1,100 do t[i]=(t[i]+3)*2-1 end return t",
        "local t={} for i=1,100 do t[i]=false end return t",
        "local t={} for i=1,100 do t[i]=t[i]+0.5 end return t",
        "local t={} for i=1,100 do local value=i t[i]=value end return t",
        "local t={} for i=1,100 do local value=2147483647 t[i]=value end return t",
    ] {
        Lua::empty().enter(|ctx| {
            let closure = Closure::load(ctx, None, text.as_bytes()).unwrap();
            let source = Snapshot::new(&closure.prototype(), 4096, 1024 * 1024).unwrap();
            let plan = (0..source.operations.len()).find_map(|end| Plan::new(&source, end)).unwrap();
            let generic = backend::compile(&source, MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX)), 1024 * 1024).unwrap();
            with_kernel(&source, plan, |entry| {
                for (index, limit, step) in [(1, 32, 1), (32, 1, -3), (17, 30, 0), (63, 100, 2)] {
                    for pc in plan.start..=plan.end {
                        for budget in (0..=65).chain([u32::MAX]) {
                            let initial: Vec<_> = (1..=128).map(|key| Value::Integer(key * 7)).collect();
                            let native_table = array(ctx, &initial);
                            let control_table = array(ctx, &initial);
                            let mut canonical = vec![Value::Integer(3); source.registers];
                            canonical[usize::from(plan.table)] = Value::Table(control_table);
                            for (offset, value) in [index, limit, step, index].into_iter().enumerate() {
                                canonical[usize::from(plan.base) + offset] = Value::Integer(value);
                            }
                            let mut native_slots: Vec<_> = canonical.iter().copied().map(Slot::from_value).collect();
                            let mut control_slots = native_slots.clone();
                            let mut counts = Counts::default();
                            let exit = with_window::<_, 64>(ctx, native_table, 1, 64, plan.access, &mut counts, |window| {
                                window.with_native(|session| unsafe { session.invoke_kernel(entry, &mut native_slots, pc as u64, budget) })
                            }).unwrap();
                            assert!(exit.instructions <= budget.min(64));
                            assert!(exit.pc >= plan.start as u64 && exit.pc <= plan.end as u64 + 1);
                            let mut frame_pc = pc;
                            LuaRegisters::with_test_frame(ctx, &mut frame_pc, &mut canonical, |mut registers| {
                                let mut frame = helpers::Frame { ctx, closure, registers: &mut registers, count: helpers::Counts::default(), slot_count: source.registers, panic: None, projection: None };
                                let mut host = abi::Host { data: (&mut frame as *mut helpers::Frame<'_, '_, '_, '_>).cast(), projection: std::ptr::null_mut() };
                                let expected = unsafe { generic.invoke_host(&mut control_slots, pc, exit.instructions, &mut host) };
                                assert_eq!((exit.pc, exit.instructions), (expected.pc, expected.instructions), "{text} pc={pc} budget={budget} control={index}/{limit}/{step}");
                                assert_eq!(u64::from(counts.reads), frame.count.table_reads);
                                assert_eq!(u64::from(counts.writes), frame.count.table_writes);
                                assert!(frame.panic.is_none());
                            });
                            same(&native_slots, &control_slots);
                            let native: Vec<_> = (1..=128).map(|key| Slot::from_value(native_table.get_value(ctx, key))).collect();
                            let control: Vec<_> = (1..=128).map(|key| Slot::from_value(control_table.get_value(ctx, key))).collect();
                            same(&native, &control);
                        }
                    }
                }
            });
        });
    }
}

#[test]
fn loop_overflow_and_invalid_entry_pcs_preserve_exact_exit_state() {
    Lua::empty().enter(|ctx| {
        let closure =
            Closure::load(ctx, None, b"local t={} for i=1,9 do t[i]=i end return t").unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 1024 * 1024).unwrap();
        let plan = (0..source.operations.len())
            .find_map(|end| Plan::new(&source, end))
            .unwrap();
        let generic = backend::compile(
            &source,
            MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX)),
            1024 * 1024,
        )
        .unwrap();
        with_kernel(&source, plan, |entry| {
            let table = array(ctx, &[Value::Integer(7); 64]);
            for (index, limit, step) in [
                (i64::MAX, i64::MAX, 1),
                (i64::MIN, i64::MIN, -1),
                (i64::MAX, i64::MIN, i64::MIN),
            ] {
                let mut slots = vec![Slot::from_value(Value::Integer(3)); source.registers];
                for (offset, value) in [index, limit, step, 7].into_iter().enumerate() {
                    slots[usize::from(plan.base) + offset] =
                        Slot::from_value(Value::Integer(value));
                }
                let mut control = slots.clone();
                let mut counts = Counts::default();
                let exit =
                    with_window::<_, 64>(ctx, table, 1, 64, Access::Read, &mut counts, |window| {
                        window.with_native(|session| unsafe {
                            session.invoke_kernel(entry, &mut slots, plan.end as u64, 1)
                        })
                    })
                    .unwrap();
                let expected = generic.invoke(&mut control, plan.end, 1);
                assert_eq!(
                    (exit.pc, exit.instructions),
                    (expected.pc, expected.instructions)
                );
                same(&slots, &control);
                assert_eq!(counts, Counts::default());
            }
            for pc in [u64::MAX, plan.end as u64 + 1] {
                let mut slots = vec![Slot::from_value(Value::Integer(3)); source.registers];
                let original = slots.clone();
                let exit = with_window::<_, 64>(
                    ctx,
                    table,
                    1,
                    64,
                    Access::Read,
                    &mut Counts::default(),
                    |window| {
                        window.with_native(|session| unsafe {
                            session.invoke_kernel(entry, &mut slots, pc, u32::MAX)
                        })
                    },
                )
                .unwrap();
                assert_eq!(
                    (exit.pc, exit.instructions, exit.reason),
                    (pc, 0, Kind::Interpreter as u32)
                );
                same(&slots, &original);
            }
        });
    });
}

#[test]
fn frozen_array_script_resumes_generic_native_code_without_losing_slice_coverage() {
    use super::super::invoke;
    Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, b"local t={} for i=1,5000 do t[i]=i end local sum=0 for i=1,5000 do sum=sum+t[i] end return sum").unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 1024 * 1024).unwrap();
        let plans: Vec<_> = (0..source.operations.len()).filter_map(|end| Plan::new(&source, end)).collect();
        assert_eq!(plans.len(), 2);
        let generic = backend::compile(&source, MappingCounter::new(crate::jit::resources::Ledger::new(usize::MAX)), 1024 * 1024).unwrap();
        with_kernel(&source, plans[0], |fill| with_kernel(&source, plans[1], |sum| {
            for budget in [1, 2, 17, 64, 65, u32::MAX] {
                let run = |selected: bool| {
                    let mut canonical = vec![Value::Nil; source.registers];
                    let mut slots = vec![Slot::from_value(Value::Nil); source.registers];
                    let mut pc = 0;
                    LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                        let mut frame = helpers::Frame { ctx, closure, registers: &mut registers, count: helpers::Counts::default(), slot_count: source.registers, panic: None, projection: None };
                        let mut slices = Vec::new();
                        let mut direct = (0u64, 0u64);
                        loop {
                            let pc = *frame.registers.pc;
                            if let Operation::Return { start, .. } = source.operations[pc] {
                                assert_eq!((slots[usize::from(start.0)].tag, slots[usize::from(start.0)].bits), (abi::INTEGER, 12_502_500));
                                break;
                            }
                            assert!(slices.len() < 50_000);
                            let prefix = if selected {
                                plans.iter().zip([fill, sum]).find_map(|(&plan, entry)| {
                                    unsafe { invoke::invoke(entry, plan, ctx, frame.registers.stack_frame, &mut slots, pc, budget) }
                                })
                            } else { None };
                            let completed = prefix.as_ref().map_or(0, |prefix| prefix.exit.instructions);
                            if let Some(prefix) = prefix {
                                *frame.registers.pc = prefix.exit.pc as usize;
                                direct.0 += u64::from(prefix.counts.reads);
                                direct.1 += u64::from(prefix.counts.writes);
                            }
                            let pc = *frame.registers.pc;
                            let mut host = abi::Host { data: (&mut frame as *mut helpers::Frame<'_, '_, '_, '_>).cast(), projection: std::ptr::null_mut() };
                            let exit = unsafe { generic.invoke_host(&mut slots, pc, budget.min(64) - completed, &mut host) };
                            assert!(frame.panic.is_none());
                            assert!(exit.instructions + completed > 0);
                            *frame.registers.pc = exit.pc as usize;
                            for (slot, value) in slots.iter().copied().zip(frame.registers.stack_frame.iter_mut()) { slot.write_back(value); }
                            slices.push((exit.pc, exit.instructions + completed, exit.reason));
                        }
                        assert_eq!(frame.count.table_reads + direct.0, 5000);
                        assert_eq!(frame.count.table_writes + direct.1, 5000);
                        (slices, frame.count.calls, direct)
                    })
                };
                let (control, helper_calls, _) = run(false);
                let (candidate, reduced_calls, direct) = run(true);
                assert_eq!(candidate, control, "budget={budget}");
                println!("array_kernel budget={budget} slices={} control_helper_calls={helper_calls} candidate_helper_calls={reduced_calls} direct_reads={} direct_writes={}", candidate.len(), direct.0, direct.1);
                assert!(direct.0 > 4000 && direct.1 > 4000, "budget={budget} direct={direct:?}");
                assert!(reduced_calls + 8000 < helper_calls, "budget={budget}");
            }
        }));
    });
}
