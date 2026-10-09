use super::*;
use crate::jit::array_window::native::{self, Entry};
use cranelift_codegen::ir::{AbiParam, Opcode};
use cranelift_frontend::FunctionBuilderContext;
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Linkage, Module};

struct Code(Option<JITModule>);

impl Drop for Code {
    fn drop(&mut self) {
        unsafe { self.0.take().unwrap().free_memory() };
    }
}

fn with_entries(body: impl FnOnce(Entry, Entry)) {
    let mut owner = Code(Some(JITModule::new(
        JITBuilder::new(default_libcall_names()).unwrap(),
    )));
    let module = owner.0.as_mut().unwrap();
    let mut ids = Vec::new();
    for (name, access) in [("array_read", Access::Read), ("array_write", Access::Write)] {
        let mut context = module.make_context();
        context
            .func
            .signature
            .params
            .extend([AbiParam::new(types::I64); 3]);
        context
            .func
            .signature
            .returns
            .push(AbiParam::new(types::I32));
        let id = module
            .declare_function(name, Linkage::Local, &context.func.signature)
            .unwrap();
        let mut frontend = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut context.func, &mut frontend);
            let entry = b.create_block();
            let decline = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let [view, key, slot]: [_; 3] = b.block_params(entry).try_into().unwrap();
            emit_access(&mut b, access, view, key, slot, decline);
            let complete = b.ins().iconst(types::I32, 1);
            b.ins().return_(&[complete]);
            b.switch_to_block(decline);
            let refused = b.ins().iconst(types::I32, 0);
            b.ins().return_(&[refused]);
            b.seal_all_blocks();
            b.finalize(module.isa().frontend_config());
        }
        cranelift_codegen::verify_function(&context.func, module.isa()).unwrap();
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
        module.define_function(id, &mut context).unwrap();
        ids.push(id);
    }
    module.finalize_definitions().unwrap();
    let read =
        unsafe { std::mem::transmute::<*const u8, Entry>(module.get_finalized_function(ids[0])) };
    let write =
        unsafe { std::mem::transmute::<*const u8, Entry>(module.get_finalized_function(ids[1])) };
    body(read, write);
}

#[test]
fn generated_accesses_execute_without_rust_helpers() {
    with_entries(native::tests::exercise);
}

#[test]
fn native_accesses_match_the_model_across_scalar_and_index_boundaries() {
    with_entries(|read, write| {
        for length in [1, 2, 31, 32, 63, 64] {
            for first in [1, 17, i64::MAX - 63] {
                for writable in [0, 1] {
                    for key in [
                        i64::MIN,
                        0,
                        first - 1,
                        first,
                        first + (length as i64 - 1),
                        i64::MAX,
                    ] {
                        for tag in [
                            abi::NIL,
                            abi::BOOLEAN,
                            abi::INTEGER,
                            abi::NUMBER,
                            abi::REFERENCE,
                            u64::MAX,
                        ] {
                            for bits in [
                                0,
                                1,
                                2,
                                i64::MIN as u64,
                                i64::MAX as u64,
                                u64::MAX,
                                0x7ff8_0000_0000_1234,
                            ] {
                                for (native, model) in [
                                    (read, native::tests::read as Entry),
                                    (write, native::tests::write as Entry),
                                ] {
                                    let value = Slot { tag, bits };
                                    let run = |entry: Entry| {
                                        let mut slots = [value; LIMIT];
                                        let mut dirty = 0;
                                        let mut counts = Counts::default();
                                        let view = View {
                                            version: native::VERSION,
                                            slots: slots.as_mut_ptr(),
                                            length,
                                            first,
                                            dirty: &mut dirty,
                                            counts: &mut counts,
                                            writable,
                                        };
                                        let mut slot = value;
                                        let outcome = unsafe {
                                            entry((&view as *const View).cast(), key, &mut slot)
                                        };
                                        (
                                            outcome,
                                            (slot.tag, slot.bits),
                                            slots.map(|s| (s.tag, s.bits)),
                                            dirty,
                                            counts,
                                        )
                                    };
                                    assert_eq!(run(native), run(model), "length={length} first={first} writable={writable} key={key} tag={tag} bits={bits}");
                                }
                            }
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn invalid_descriptor_fields_decline_without_effects() {
    with_entries(|read, write| {
        for entry in [read, write] {
            for invalid in 0..10 {
                let original = Slot::from_value(Value::Integer(7));
                let mut slots = [original; 1];
                let mut dirty = 0;
                let mut counts = Counts::default();
                let mut slot = original;
                let mut view = View {
                    version: native::VERSION,
                    slots: slots.as_mut_ptr(),
                    length: 1,
                    first: 1,
                    dirty: &mut dirty,
                    counts: &mut counts,
                    writable: 1,
                };
                match invalid {
                    0 => view.version += 1,
                    1 => view.length = 0,
                    2 => view.length = 65,
                    3 => view.first = 0,
                    4 => view.writable = 2,
                    5 => view.slots = std::ptr::null_mut(),
                    6 => view.dirty = std::ptr::null_mut(),
                    7 => view.counts = std::ptr::null_mut(),
                    _ => (),
                }
                let pointer = if invalid == 8 {
                    std::ptr::null()
                } else {
                    &view as *const View
                };
                let output = if invalid == 9 {
                    std::ptr::null_mut()
                } else {
                    &mut slot as *mut Slot
                };
                assert_eq!(unsafe { entry(pointer.cast(), 1, output) }, 0);
                assert_eq!((slot.tag, slot.bits), (original.tag, original.bits));
                assert_eq!((slots[0].tag, slots[0].bits), (original.tag, original.bits));
                assert_eq!(dirty, 0);
                assert_eq!(counts, Counts::default());
            }
        }
    });
}
