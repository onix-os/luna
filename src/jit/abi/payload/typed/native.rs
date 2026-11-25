use cranelift_codegen::ir::{condcodes::IntCC, types, AbiParam, InstBuilder, MemFlagsData};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Module};

use super::*;

struct Code(Option<JITModule>);

impl Drop for Code {
    fn drop(&mut self) {
        unsafe { self.0.take().unwrap().free_memory() };
    }
}

#[test]
fn native_typed_access_and_terminal_stores_match_canonical_steps() {
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
    let mut writer = module.make_signature();
    writer.params.extend([AbiParam::new(types::I64); 3]);
    let mut frontend = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut context.func, &mut frontend);
        let writer = builder.import_signature(writer);
        let entry = builder.create_block();
        let add = builder.create_block();
        let materialize = builder.create_block();
        let done = builder.create_block();
        builder.append_block_param(done, types::I32);
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let [pointers, destination, retired, callee, budget]: [_; 5] =
            builder.block_params(entry).try_into().unwrap();
        let zero = builder.ins().iconst(types::I32, 0);
        let empty = builder.ins().icmp_imm_u(IntCC::Equal, budget, 0);
        builder.ins().brif(empty, done, &[zero.into()], add, &[]);
        builder.switch_to_block(add);
        let first = builder
            .ins()
            .load(types::I64, MemFlagsData::new(), pointers, 0);
        let second = builder
            .ins()
            .load(types::I64, MemFlagsData::new(), pointers, 8);
        let left = builder
            .ins()
            .load(types::I64, MemFlagsData::new(), first, 0);
        let right = builder
            .ins()
            .load(types::I64, MemFlagsData::new(), second, 0);
        let sum = builder.ins().iadd(left, right);
        builder.ins().store(MemFlagsData::new(), sum, first, 0);
        let one = builder.ins().iconst(types::I32, 1);
        let exhausted = builder.ins().icmp_imm_u(IntCC::Equal, budget, 1);
        builder
            .ins()
            .brif(exhausted, done, &[one.into()], materialize, &[]);
        builder.switch_to_block(materialize);
        builder
            .ins()
            .call_indirect(writer, callee, &[destination, retired, sum]);
        let two = builder.ins().iconst(types::I32, 2);
        builder.ins().jump(done, &[two.into()]);
        builder.switch_to_block(done);
        let count = builder.block_params(done)[0];
        builder.ins().return_(&[count]);
        builder.seal_all_blocks();
        builder.finalize(module.isa().frontend_config());
    }
    cranelift_codegen::verify_function(&context.func, module.isa()).unwrap();
    module.define_function(id, &mut context).unwrap();
    module.finalize_definitions().unwrap();
    let entry: Probe = unsafe { std::mem::transmute(module.get_finalized_function(id)) };
    check_terminal_probe(entry);
}
