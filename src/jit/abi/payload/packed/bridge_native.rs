use cranelift_codegen::ir::{condcodes::IntCC, types, AbiParam, InstBuilder, MemFlagsData};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Module};

use super::*;

type Entry = unsafe extern "C" fn(*mut Packed, *mut (), Helper, u32, u32, u32, u32) -> u32;

struct Code(Option<JITModule>);

impl Drop for Code {
    fn drop(&mut self) {
        unsafe { self.0.take().unwrap().free_memory() };
    }
}

#[test]
fn packed_native_c_bridge_reloads_shared_storage_and_honors_every_status() {
    let mut owner = Code(Some(JITModule::new(
        JITBuilder::new(default_libcall_names()).unwrap(),
    )));
    let module = owner.0.as_mut().unwrap();
    let mut context = module.make_context();
    context.func.signature.params.extend(
        [
            types::I64,
            types::I64,
            types::I64,
            types::I32,
            types::I32,
            types::I32,
            types::I32,
        ]
        .map(AbiParam::new),
    );
    context
        .func
        .signature
        .returns
        .push(AbiParam::new(types::I32));
    let id = module
        .declare_anonymous_function(&context.func.signature)
        .unwrap();
    let mut helper = module.make_signature();
    helper
        .params
        .extend([types::I64, types::I32, types::I32, types::I32, types::I32].map(AbiParam::new));
    helper.returns.push(AbiParam::new(types::I32));
    let mut frontend = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut context.func, &mut frontend);
        let signature = b.import_signature(helper);
        let entry = b.create_block();
        let after = b.create_block();
        let done = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.append_block_param(done, types::I32);
        b.switch_to_block(entry);
        let [slots, control, callee, a, arg_b, c, pc]: [_; 7] =
            b.block_params(entry).try_into().unwrap();
        let packed = b.ins().load(types::I64, MemFlagsData::new(), slots, 0);
        let pointer = b.ins().band_imm_s(packed, !(MASK as i64));
        let value = b.ins().load(types::I64, MemFlagsData::new(), pointer, 0);
        let value = b.ins().iadd_imm_s(value, 1);
        b.ins().store(MemFlagsData::new(), value, pointer, 0);
        let call = b
            .ins()
            .call_indirect(signature, callee, &[control, a, arg_b, c, pc]);
        let result = b.inst_results(call)[0];
        let completed = b
            .ins()
            .icmp_imm_u(IntCC::Equal, result, abi::HELPER_COMPLETED as i64);
        b.ins().brif(completed, after, &[], done, &[result.into()]);
        b.switch_to_block(after);
        let packed = b.ins().load(
            types::I64,
            MemFlagsData::new(),
            slots,
            std::mem::size_of::<Packed>() as i32,
        );
        let pointer = b.ins().band_imm_s(packed, !(MASK as i64));
        let value = b.ins().load(types::I64, MemFlagsData::new(), pointer, 0);
        let value = b.ins().iadd_imm_s(value, 1);
        b.ins().store(MemFlagsData::new(), value, pointer, 0);
        b.ins().jump(done, &[result.into()]);
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
    check(|slots, control, callee, args| unsafe {
        entry(slots, control, callee, args[0], args[1], args[2], args[3])
    });
}
