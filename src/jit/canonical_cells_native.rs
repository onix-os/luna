use cranelift_codegen::ir::{condcodes::IntCC, types, AbiParam, InstBuilder, MemFlagsData};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Module};

use super::{tests, Cell};

type Entry = for<'gc> unsafe extern "C" fn(*mut Cell<'gc>, usize, usize, usize, u64) -> u32;

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
        .extend([AbiParam::new(types::I64); 5]);
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
        let store = b.create_block();
        let decline = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let [cells, len, index, tag, bits]: [_; 5] = b.block_params(entry).try_into().unwrap();
        let in_bounds = b.ins().icmp(IntCC::UnsignedLessThan, index, len);
        let scalar = b.ins().icmp_imm_u(IntCC::UnsignedLessThanOrEqual, tag, 3);
        let not_bool = b.ins().icmp_imm_u(IntCC::NotEqual, tag, 1);
        let bool_bits = b.ins().icmp_imm_u(IntCC::UnsignedLessThanOrEqual, bits, 1);
        let valid_bits = b.ins().bor(not_bool, bool_bits);
        let valid = b.ins().band(in_bounds, scalar);
        let valid = b.ins().band(valid, valid_bits);
        b.ins().brif(valid, store, &[], decline, &[]);
        b.switch_to_block(store);
        let offset = b
            .ins()
            .imul_imm_u(index, std::mem::size_of::<Cell<'_>>() as i64);
        let cell = b.ins().iadd(cells, offset);
        let nil = b.ins().icmp_imm_u(IntCC::Equal, tag, 0);
        let zero = b.ins().iconst(types::I64, 0);
        let bits = b.ins().select(nil, zero, bits);
        b.ins().store(
            MemFlagsData::new(),
            bits,
            cell,
            std::mem::offset_of!(Cell<'_>, payload) as i32,
        );
        b.ins().store(
            MemFlagsData::new(),
            tag,
            cell,
            std::mem::offset_of!(Cell<'_>, kind) as i32,
        );
        let yes = b.ins().iconst(types::I32, 1);
        b.ins().return_(&[yes]);
        b.switch_to_block(decline);
        let no = b.ins().iconst(types::I32, 0);
        b.ins().return_(&[no]);
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
fn canonical_generated_stores_match_rust_without_binding() {
    with_entry(tests::check_scalar_entry);
}

#[test]
fn canonical_generated_stores_release_only_root_references() {
    with_entry(|entry| {
        tests::only_roots(|cell, bits| unsafe { entry(cell, 1, 0, 2, bits) == 1 });
    });
}
