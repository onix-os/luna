use super::*;
use cranelift_codegen::{
    ir::{condcodes::IntCC, types, AbiParam, Block, InstBuilder, MemFlagsData, Type},
    settings::{self, Configurable},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Linkage, Module};

pub(super) struct Code {
    owner: Option<JITModule>,
    pub entry: Transition,
}

impl Drop for Code {
    fn drop(&mut self) {
        unsafe { self.owner.take().unwrap().free_memory() };
    }
}

fn math(
    b: &mut FunctionBuilder<'_>,
    header: cranelift_codegen::ir::Value,
    ptr: Type,
    delta: i64,
    next: Block,
) {
    let fields = b.create_block();
    let skipped = b.create_block();
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
    let null = b.ins().icmp_imm_u(IntCC::Equal, base, 0);
    let short = b.ins().icmp_imm_u(IntCC::UnsignedLessThanOrEqual, len, 1);
    let invalid = b.ins().bor(null, short);
    b.ins().brif(invalid, skipped, &[], fields, &[]);
    b.switch_to_block(fields);
    let stride = b.ins().load(
        ptr,
        MemFlagsData::new(),
        header,
        mem::offset_of!(Header, stride) as i32,
    );
    let address = b.ins().iadd(base, stride);
    let value = b.ins().load(types::I64, MemFlagsData::new(), address, 0);
    let delta = b.ins().iconst(types::I64, delta);
    let value = b.ins().iadd(value, delta);
    b.ins().store(MemFlagsData::new(), value, address, 0);
    let one = b.ins().iconst(types::I32, 1);
    b.ins().jump(next, &[one.into()]);
    b.switch_to_block(skipped);
    let zero = b.ins().iconst(types::I32, 0);
    b.ins().jump(next, &[zero.into()]);
    b.switch_to_block(next);
}

pub(super) fn compile(helper: Helper) -> Code {
    let mut settings = settings::builder();
    settings.set("opt_level", "speed").unwrap();
    let isa = cranelift_native::builder()
        .unwrap()
        .finish(settings::Flags::new(settings))
        .unwrap();
    let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
    builder.symbol("strided_transition_helper", helper as *const u8);
    let mut module = JITModule::new(builder);
    let ptr = module.target_config().pointer_type();
    let mut context = module.make_context();
    context
        .func
        .signature
        .params
        .extend([AbiParam::new(ptr); 2]);
    context
        .func
        .signature
        .params
        .extend([AbiParam::new(types::I32); 4]);
    context
        .func
        .signature
        .returns
        .push(AbiParam::new(types::I32));
    let helper = module
        .declare_function(
            "strided_transition_helper",
            Linkage::Import,
            &context.func.signature,
        )
        .unwrap();
    let id = module
        .declare_anonymous_function(&context.func.signature)
        .unwrap();
    let helper = module.declare_func_in_func(helper, &mut context.func);
    let mut frontend = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut context.func, &mut frontend);
        let entry = b.create_block();
        let call = b.create_block();
        let suffix = b.create_block();
        let merged = b.create_block();
        let exit = b.create_block();
        for block in [call, merged, exit] {
            b.append_block_param(block, types::I32);
        }
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let args = b.block_params(entry).to_vec();
        math(&mut b, args[0], ptr, 1, call);
        let invocation = b.ins().call(helper, &args);
        let status = b.inst_results(invocation)[0];
        let completed = b
            .ins()
            .icmp_imm_u(IntCC::Equal, status, i64::from(abi::HELPER_COMPLETED));
        b.ins().brif(completed, suffix, &[], exit, &[status.into()]);
        b.switch_to_block(suffix);
        math(&mut b, args[0], ptr, 2, merged);
        let ready = b.block_params(merged)[0];
        let flag = b.ins().imul_imm_u(ready, i64::from(READY));
        let result = b.ins().bor(status, flag);
        b.ins().jump(exit, &[result.into()]);
        b.switch_to_block(exit);
        let result = b.block_params(exit)[0];
        b.ins().return_(&[result]);
        b.seal_all_blocks();
        b.finalize(module.target_config());
    }
    module.define_function(id, &mut context).unwrap();
    module.finalize_definitions().unwrap();
    let entry =
        unsafe { mem::transmute::<*const u8, Transition>(module.get_finalized_function(id)) };
    Code {
        owner: Some(module),
        entry,
    }
}
