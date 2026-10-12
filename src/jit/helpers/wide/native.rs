use super::*;
use crate::{
    jit::ir::Snapshot,
    opcode::{Operation, RCIndex},
    thread::LuaRegisters,
    Closure,
};
use cranelift_codegen::{
    ir::{types, AbiParam, InstBuilder},
    settings::{self, Configurable},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Linkage, Module};
use std::hint::black_box;

type Entry = unsafe extern "C" fn(*mut abi::Host, *mut Slot) -> u32;

struct Code {
    owner: Option<JITModule>,
    entry: Entry,
    bytes: usize,
}

impl Drop for Code {
    fn drop(&mut self) {
        unsafe { self.owner.take().unwrap().free_memory() };
    }
}

fn compile(wide: bool, packed: u32, pc: u32, hash: u64) -> Code {
    let mut settings = settings::builder();
    settings.set("opt_level", "speed").unwrap();
    let isa = cranelift_native::builder()
        .unwrap()
        .finish(settings::Flags::new(settings))
        .unwrap();
    let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
    builder.symbol(
        "wide_helper_probe",
        if wide {
            call as *const u8
        } else {
            PREHASHED_ENTRY as *const u8
        },
    );
    let mut module = JITModule::new(builder);
    let ptr = module.target_config().pointer_type();
    let mut signature = module.make_signature();
    signature.params.extend([AbiParam::new(ptr); 2]);
    signature.params.extend(
        if wide {
            vec![types::I64; 2]
        } else {
            vec![types::I32; 4]
        }
        .into_iter()
        .map(AbiParam::new),
    );
    signature.returns.push(AbiParam::new(types::I32));
    let helper = module
        .declare_function("wide_helper_probe", Linkage::Import, &signature)
        .unwrap();
    let mut context = module.make_context();
    context
        .func
        .signature
        .params
        .extend([AbiParam::new(ptr); 2]);
    context
        .func
        .signature
        .returns
        .push(AbiParam::new(types::I32));
    let id = module
        .declare_anonymous_function(&context.func.signature)
        .unwrap();
    let helper = module.declare_func_in_func(helper, &mut context.func);
    let mut frontend = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut context.func, &mut frontend);
        let block = builder.create_block();
        builder.append_block_params_for_function_params(block);
        builder.switch_to_block(block);
        let [host, slots]: [_; 2] = builder.block_params(block).try_into().unwrap();
        let instruction = if wide {
            let operands = builder
                .ins()
                .iconst(types::I64, operands(packed, pc) as i64);
            let hash = builder.ins().iconst(types::I64, hash as i64);
            builder.ins().call(helper, &[host, slots, operands, hash])
        } else {
            let [packed, low, high, pc] = [packed, hash as u32, (hash >> 32) as u32, pc]
                .map(|value| builder.ins().iconst(types::I32, i64::from(value)));
            builder
                .ins()
                .call(helper, &[host, slots, packed, low, high, pc])
        };
        let status = builder.inst_results(instruction)[0];
        builder.ins().return_(&[status]);
        builder.seal_all_blocks();
        builder.finalize(module.target_config());
    }
    module.define_function(id, &mut context).unwrap();
    let bytes = context.compiled_code().unwrap().code_buffer().len();
    module.finalize_definitions().unwrap();
    let entry =
        unsafe { std::mem::transmute::<*const u8, Entry>(module.get_finalized_function(id)) };
    Code {
        owner: Some(module),
        entry,
        bytes,
    }
}

fn source(ctx: Context<'_>) -> (Closure<'_>, usize, u32, u64) {
    let closure = Closure::load(ctx, None, b"return target").unwrap();
    let snapshot = Snapshot::new(&closure.prototype(), 4096, 1 << 20).unwrap();
    let Operation::GetUpTable {
        dest,
        table,
        key: RCIndex::Constant(index),
    } = snapshot.operations[0]
    else {
        panic!("fixture opcode")
    };
    let packed = (u32::from(dest.0) << 16) | (u32::from(table.0) << 8) | u32::from(index.0);
    (
        closure,
        snapshot.registers,
        packed,
        snapshot.prepared_keys.as_ref().unwrap().hash(index.0),
    )
}

#[test]
fn generated_wide_calls_match_current_values_guards_and_panics() {
    crate::Lua::empty().enter(|ctx| {
        let (closure, width, packed, hash) = source(ctx);
        let mut comparisons = 0;
        for pc in [0, 1, 255, 65536, u32::MAX] {
            let code = [false, true].map(|wide| compile(wide, packed, pc, hash));
            for case in 0..7 {
                let mt = crate::Table::new(&ctx);
                mt.set(ctx, "__index", crate::Table::new(&ctx)).unwrap();
                ctx.globals().set_metatable(ctx, (case == 3).then_some(mt));
                let value = match case {
                    0 => Value::Integer(i64::MAX),
                    1 => Value::Number(f64::from_bits(0x7ff8_0000_0000_1234)),
                    2 | 3 => Value::Nil,
                    4 => Value::Table(crate::Table::new(&ctx)),
                    _ => Value::Boolean(false),
                };
                ctx.globals().set(ctx, "target", value).unwrap();
                let reports = code.each_ref().map(|code| {
                    let mut canonical = vec![Value::Integer(73); width];
                    let mut current_pc = 17;
                    let report = LuaRegisters::with_test_frame(
                        ctx,
                        &mut current_pc,
                        &mut canonical,
                        |mut registers| {
                            let mut slots: Vec<_> = registers
                                .stack_frame
                                .iter()
                                .copied()
                                .map(Slot::from_value)
                                .collect();
                            let mut frame = Frame {
                                ctx,
                                closure,
                                registers: &mut registers,
                                count: Counts::default(),
                                slot_count: slots.len(),
                                panic: None,
                                projection: None,
                            };
                            let mut host = abi::Host {
                                data: std::ptr::from_mut(&mut frame).cast(),
                                projection: std::ptr::null_mut(),
                            };
                            let globals = ctx.globals().into_inner();
                            let borrow = (case == 5).then(|| globals.borrow_mut(&ctx));
                            let status = unsafe {
                                (code.entry)(
                                    if case == 6 {
                                        std::ptr::null_mut()
                                    } else {
                                        &mut host
                                    },
                                    slots.as_mut_ptr(),
                                )
                            };
                            drop(borrow);
                            let slot_bits: Vec<_> =
                                slots.iter().map(|slot| (slot.tag, slot.bits)).collect();
                            (
                                status,
                                *frame.registers.pc,
                                frame.count.calls,
                                frame.count.completed,
                                frame.count.declined,
                                frame.count.table_reads,
                                frame.count.upvalue_reads,
                                frame.panic.is_some(),
                                slot_bits,
                            )
                        },
                    );
                    (report, canonical)
                });
                assert_eq!(reports[0].0, reports[1].0, "pc={pc} case={case}");
                for (&actual, &expected) in reports[1].1.iter().zip(&reports[0].1) {
                    crate::jit::helpers::tests::assert_identical(actual, expected);
                }
                assert_eq!(
                    reports[1].0 .0,
                    if case == 5 {
                        abi::HELPER_PANICKED
                    } else if case == 3 || case == 6 {
                        abi::HELPER_DECLINED
                    } else {
                        abi::HELPER_COMPLETED
                    }
                );
                comparisons += 1;
            }
        }
        assert_eq!(comparisons, 35);
    });
}

#[inline(never)]
fn profile_calls(entry: Entry, host: *mut abi::Host, slots: *mut Slot, count: usize) -> u32 {
    let mut sum = 0u32;
    for _ in 0..count {
        sum = sum.wrapping_add(black_box(unsafe { entry(host, slots) }));
    }
    sum
}

#[test]
#[ignore = "generated helper ABI cost probe, not runtime performance acceptance"]
fn profile_generated_helper_calls() {
    let wide = match std::env::var("LUNA_WIDE_HELPER").unwrap().as_str() {
        "0" => false,
        "1" => true,
        _ => panic!("LUNA_WIDE_HELPER must be 0 or 1"),
    };
    crate::Lua::empty().enter(|ctx| {
        let (closure, width, packed, hash) = source(ctx);
        let code = compile(wide, packed, 0, hash);
        if let Ok(directory) = std::env::var("LUNA_WIDE_DIR") {
            let bytes = unsafe { std::slice::from_raw_parts(code.entry as *const u8, code.bytes) };
            std::fs::write(
                std::path::Path::new(&directory).join(format!("{wide}.native")),
                bytes,
            )
            .unwrap();
        }
        ctx.globals().set(ctx, "target", 42).unwrap();
        let mut canonical = vec![Value::Nil; width];
        let mut pc = 0;
        LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
            let mut slots: Vec<_> = registers
                .stack_frame
                .iter()
                .copied()
                .map(Slot::from_value)
                .collect();
            let mut frame = Frame {
                ctx,
                closure,
                registers: &mut registers,
                count: Counts::default(),
                slot_count: slots.len(),
                panic: None,
                projection: None,
            };
            let mut host = abi::Host {
                data: std::ptr::from_mut(&mut frame).cast(),
                projection: std::ptr::null_mut(),
            };
            let sum = profile_calls(code.entry, &mut host, slots.as_mut_ptr(), 100_000);
            assert_eq!(sum, 100_000 * abi::HELPER_COMPLETED);
            assert_eq!(*frame.registers.pc, 1);
            assert_eq!(
                (
                    frame.count.calls,
                    frame.count.completed,
                    frame.count.declined,
                    frame.count.table_reads,
                    frame.count.upvalue_reads
                ),
                (100_000, 100_000, 0, 100_000, 100_000)
            );
            assert!(frame.panic.is_none());
            assert!(matches!(frame.registers.stack_frame[0], Value::Integer(42)));
            assert_eq!((slots[0].tag, slots[0].bits), (abi::INTEGER, 42));
            eprintln!(
                "wide={wide} calls=100000 completed=100000 pc=1 result=42 native_bytes={}",
                code.bytes
            );
        });
    });
}
