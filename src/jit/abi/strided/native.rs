use super::*;
use cranelift_codegen::{
    ir::{condcodes::IntCC, types, AbiParam, InstBuilder, MemFlagsData},
    settings::{self, Configurable},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Module};

mod probe;

struct Code {
    owner: Option<JITModule>,
    entry: Entry,
    required: Mask,
}

impl Drop for Code {
    fn drop(&mut self) {
        unsafe { self.owner.take().unwrap().free_memory() };
    }
}

impl Code {
    fn kernel(&self) -> Kernel<'_> {
        Kernel {
            entry: self.entry,
            required: self.required,
            owner: PhantomData,
        }
    }
}

fn compile(source: usize, dest: usize, required: Mask) -> Option<Code> {
    if !required.contains(source) || !required.contains(dest) {
        return None;
    }
    let mut settings = settings::builder();
    settings.set("opt_level", "speed").unwrap();
    let isa = cranelift_native::builder()
        .unwrap()
        .finish(settings::Flags::new(settings))
        .unwrap();
    let mut module = JITModule::new(JITBuilder::with_isa(isa, default_libcall_names()));
    let ptr = module.target_config().pointer_type();
    let mut context = module.make_context();
    context
        .func
        .signature
        .params
        .extend([AbiParam::new(ptr), AbiParam::new(types::I64)]);
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
        let header_block = b.create_block();
        let fields = b.create_block();
        let decline = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let [header, delta]: [_; 2] = b.block_params(entry).try_into().unwrap();
        let null = b.ins().icmp_imm_u(IntCC::Equal, header, 0);
        b.ins().brif(null, decline, &[], header_block, &[]);
        b.switch_to_block(header_block);
        let base = b.ins().load(
            ptr,
            MemFlagsData::new(),
            header,
            mem::offset_of!(Header, base) as i32,
        );
        let len = b.ins().load(
            ptr,
            MemFlagsData::new(),
            header,
            mem::offset_of!(Header, len) as i32,
        );
        let short =
            b.ins()
                .icmp_imm_u(IntCC::UnsignedLessThanOrEqual, len, source.max(dest) as i64);
        let null = b.ins().icmp_imm_u(IntCC::Equal, base, 0);
        let invalid = b.ins().bor(short, null);
        b.ins().brif(invalid, decline, &[], fields, &[]);
        b.switch_to_block(fields);
        let stride = b.ins().load(
            ptr,
            MemFlagsData::new(),
            header,
            mem::offset_of!(Header, stride) as i32,
        );
        let source_offset = b.ins().imul_imm_u(stride, source as i64);
        let dest_offset = b.ins().imul_imm_u(stride, dest as i64);
        let source = b.ins().iadd(base, source_offset);
        let dest = b.ins().iadd(base, dest_offset);
        let value = b.ins().load(types::I64, MemFlagsData::new(), source, 0);
        let value = b.ins().iadd(value, delta);
        b.ins().store(MemFlagsData::new(), value, dest, 0);
        let one = b.ins().iconst(types::I32, 1);
        b.ins().return_(&[one]);
        b.switch_to_block(decline);
        let zero = b.ins().iconst(types::I32, 0);
        b.ins().return_(&[zero]);
        b.seal_all_blocks();
        b.finalize(module.target_config());
    }
    module.define_function(id, &mut context).unwrap();
    module.finalize_definitions().unwrap();
    let entry = unsafe { mem::transmute::<*const u8, Entry>(module.get_finalized_function(id)) };
    Some(Code {
        owner: Some(module),
        entry,
        required,
    })
}

#[test]
fn native_field_access_matches_wrapping_and_preserves_neighbors() {
    for (source, dest, width) in [(0, 0, 1), (1, 5, 7), (255, 0, 256), (0, 255, 256)] {
        let code = compile(source, dest, Mask::new(&[source, dest]).unwrap()).unwrap();
        tests::corpus(&code.kernel(), source, dest, width);
    }
}

#[test]
fn native_admission_and_entry_guards_match_rust() {
    assert!(compile(0, 1, Mask::new(&[0]).unwrap()).is_none());
    assert!(compile(0, 256, Mask::new(&[0]).unwrap()).is_none());
    let code = compile(1, 3, Mask::new(&[1, 3]).unwrap()).unwrap();
    tests::kinds_and_rebinding(&code.kernel());
    tests::entry_guards(&code.kernel());
}
