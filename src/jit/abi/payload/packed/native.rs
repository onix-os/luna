use cranelift_codegen::ir::{condcodes::IntCC, types, AbiParam, InstBuilder, MemFlagsData};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Module};

use super::*;

type Entry = unsafe extern "C" fn(*const Packed, u64, u64, *mut u64, u32) -> u32;

struct Code(Option<JITModule>);

impl Drop for Code {
    fn drop(&mut self) {
        unsafe { self.0.take().unwrap().free_memory() };
    }
}

fn with_entry(body: impl FnOnce(Entry)) {
    let mut owner = Code(Some(JITModule::new(
        JITBuilder::new(default_libcall_names()).unwrap(),
    )));
    let module = owner.0.as_mut().unwrap();
    let mut context = module.make_context();
    context
        .func
        .signature
        .params
        .extend([types::I64, types::I64, types::I64, types::I64, types::I32].map(AbiParam::new));
    context
        .func
        .signature
        .returns
        .push(AbiParam::new(types::I32));
    let id = module
        .declare_anonymous_function(&context.func.signature)
        .unwrap();
    let mut frontend = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut context.func, &mut frontend);
        let entry = b.create_block();
        let decode = b.create_block();
        let write = b.create_block();
        let done = b.create_block();
        b.append_block_param(done, types::I32);
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let [slots, tag, bits, previous, budget]: [_; 5] =
            b.block_params(entry).try_into().unwrap();
        let zero = b.ins().iconst(types::I32, 0);
        let empty = b.ins().icmp_imm_u(IntCC::Equal, budget, 0);
        b.ins().brif(empty, done, &[zero.into()], decode, &[]);
        b.switch_to_block(decode);
        let packed = b.ins().load(types::I64, MemFlagsData::new(), slots, 0);
        let kind = b.ins().band_imm_u(packed, MASK as i64);
        let pointer = b.ins().band_imm_s(packed, !(MASK as i64));
        let numeric = b
            .ins()
            .icmp_imm_u(IntCC::UnsignedLessThanOrEqual, kind, FLOAT as i64);
        let nonnull = b.ins().icmp_imm_u(IntCC::NotEqual, pointer, 0);
        let integer = b.ins().icmp_imm_u(IntCC::Equal, kind, INT as i64);
        let int = b.ins().iconst(types::I64, INTEGER as i64);
        let float = b.ins().iconst(types::I64, NUMBER as i64);
        let actual = b.ins().select(integer, int, float);
        let same = b.ins().icmp(IntCC::Equal, actual, tag);
        let valid = b.ins().band(numeric, nonnull);
        let valid = b.ins().band(valid, same);
        let refused = b.ins().iconst(types::I32, 2);
        b.ins().brif(valid, write, &[], done, &[refused.into()]);
        b.switch_to_block(write);
        let old = b.ins().load(types::I64, MemFlagsData::new(), pointer, 0);
        b.ins().store(MemFlagsData::new(), bits, pointer, 0);
        b.ins().store(MemFlagsData::new(), old, previous, 0);
        let one = b.ins().iconst(types::I32, 1);
        b.ins().jump(done, &[one.into()]);
        b.switch_to_block(done);
        let result = b.block_params(done)[0];
        b.ins().return_(&[result]);
        b.seal_all_blocks();
        b.finalize(module.isa().frontend_config());
    }
    cranelift_codegen::verify_function(&context.func, module.isa()).unwrap();
    module.define_function(id, &mut context).unwrap();
    module.finalize_definitions().unwrap();
    let entry =
        unsafe { std::mem::transmute::<*const u8, Entry>(module.get_finalized_function(id)) };
    body(entry);
}

#[test]
fn packed_native_access_matches_typed_model_at_every_budget() {
    with_entry(|entry| {
        assert_eq!(
            unsafe { entry(std::ptr::null(), INTEGER, 0, std::ptr::null_mut(), 0) },
            0
        );
        crate::Lua::empty().enter(|ctx| {
            for initial in kinds(ctx) {
                for tag in [NIL, BOOLEAN, INTEGER, NUMBER, REFERENCE, u64::MAX] {
                    for bits in [
                        0,
                        1,
                        u64::MAX,
                        i64::MIN as u64,
                        (-0.0f64).to_bits(),
                        0x7ff8000000000042,
                    ] {
                        for budget in [0, 1, 2, 64, u32::MAX] {
                            let mut value = initial;
                            let packed = Packed::bind(&mut value).unwrap();
                            let original = Slot::from_value(initial);
                            let mut previous = 731;
                            let result =
                                unsafe { entry(&packed, tag, bits, &mut previous, budget) };
                            let accepted = tag == original.tag && matches!(tag, INTEGER | NUMBER);
                            assert_eq!(
                                result,
                                if budget == 0 {
                                    0
                                } else if accepted {
                                    1
                                } else {
                                    2
                                }
                            );
                            if result == 1 {
                                assert_eq!(previous, original.bits);
                                assert_eq!(unsafe { packed.load() }.unwrap().bits, bits);
                            } else {
                                assert_eq!(previous, 731);
                                if let Some(actual) = unsafe { packed.load() } {
                                    assert_eq!(
                                        (actual.tag, actual.bits),
                                        (original.tag, original.bits)
                                    );
                                }
                            }
                            let actual = Slot::from_value(value);
                            assert_eq!(actual.tag, original.tag);
                            assert_eq!(actual.bits, if result == 1 { bits } else { original.bits });
                        }
                    }
                }
            }
        });
        for address in 0..128 {
            if address >= 8 && matches!(address & MASK, INT | FLOAT) {
                continue;
            }
            let packed = Packed(std::ptr::without_provenance_mut(address));
            let mut previous = 731;
            for tag in [INTEGER, NUMBER] {
                assert_eq!(unsafe { entry(&packed, tag, 42, &mut previous, 1) }, 2);
                assert_eq!(previous, 731);
            }
        }
    });
}
