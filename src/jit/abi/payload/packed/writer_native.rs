use cranelift_codegen::ir::{condcodes::IntCC, types, AbiParam, InstBuilder, MemFlagsData};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Module};

use super::*;

type Entry = unsafe extern "C" fn(*mut Packed, *const Bridge, u32, u32, u64, u64) -> u32;

struct Code(Option<JITModule>);

impl Drop for Code {
    fn drop(&mut self) {
        unsafe { self.0.take().unwrap().free_memory() };
    }
}

fn compile(sequence: bool) -> (Code, Entry) {
    let mut owner = Code(Some(JITModule::new(
        JITBuilder::new(default_libcall_names()).unwrap(),
    )));
    let module = owner.0.as_mut().unwrap();
    let mut context = module.make_context();
    context.func.signature.params.extend(
        [
            types::I64,
            types::I64,
            types::I32,
            types::I32,
            types::I64,
            types::I64,
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
    let mut writer = module.make_signature();
    writer
        .params
        .extend([types::I64, types::I32, types::I64, types::I64].map(AbiParam::new));
    writer.returns.push(AbiParam::new(types::I32));
    let mut helper = module.make_signature();
    helper
        .params
        .extend([types::I64, types::I32, types::I32, types::I32, types::I32].map(AbiParam::new));
    helper.returns.push(AbiParam::new(types::I32));
    let mut frontend = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut context.func, &mut frontend);
        let signature = b.import_signature(writer);
        let helper_signature = b.import_signature(helper);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let [slots, bridge, terminal, index, tag, bits]: [_; 6] =
            b.block_params(entry).try_into().unwrap();
        let control = b.ins().load(
            types::I64,
            MemFlagsData::new(),
            bridge,
            std::mem::offset_of!(Bridge, control) as i32,
        );
        let table = b.ins().load(
            types::I64,
            MemFlagsData::new(),
            bridge,
            std::mem::offset_of!(Bridge, functions) as i32,
        );
        let ordinary = b.ins().load(
            types::I64,
            MemFlagsData::new(),
            table,
            std::mem::offset_of!(Functions, write) as i32,
        );
        let retired = b.ins().load(
            types::I64,
            MemFlagsData::new(),
            table,
            std::mem::offset_of!(Functions, terminal) as i32,
        );
        let callee = b.ins().select(terminal, retired, ordinary);
        let done = b.create_block();
        b.append_block_param(done, types::I32);
        if sequence {
            let boolean = b.ins().iconst(types::I64, BOOLEAN as i64);
            let invalid = b.ins().iconst(types::I64, 2);
            let call =
                b.ins()
                    .call_indirect(signature, callee, &[control, index, boolean, invalid]);
            let result = b.inst_results(call)[0];
            let declined = b
                .ins()
                .icmp_imm_u(IntCC::Equal, result, abi::HELPER_DECLINED as i64);
            let retry = b.create_block();
            b.ins().brif(declined, retry, &[], done, &[result.into()]);
            b.switch_to_block(retry);
        }
        let call = b
            .ins()
            .call_indirect(signature, callee, &[control, index, tag, bits]);
        let result = b.inst_results(call)[0];
        if sequence {
            let after_write = b.create_block();
            let completed = b
                .ins()
                .icmp_imm_u(IntCC::Equal, result, abi::HELPER_COMPLETED as i64);
            b.ins()
                .brif(completed, after_write, &[], done, &[result.into()]);
            b.switch_to_block(after_write);
            let mover = b.ins().load(
                types::I64,
                MemFlagsData::new(),
                table,
                std::mem::offset_of!(Functions, helpers) as i32,
            );
            let one = b.ins().iconst(types::I32, 1);
            let zero = b.ins().iconst(types::I32, 0);
            let pc = b.ins().iconst(types::I32, 7);
            let call =
                b.ins()
                    .call_indirect(helper_signature, mover, &[control, one, zero, zero, pc]);
            let result = b.inst_results(call)[0];
            let completed = b
                .ins()
                .icmp_imm_u(IntCC::Equal, result, abi::HELPER_COMPLETED as i64);
            let after_helper = b.create_block();
            b.ins()
                .brif(completed, after_helper, &[], done, &[result.into()]);
            b.switch_to_block(after_helper);
            let packed = b.ins().load(
                types::I64,
                MemFlagsData::new(),
                slots,
                (2 * std::mem::size_of::<Packed>()) as i32,
            );
            let pointer = b.ins().band_imm_s(packed, !(MASK as i64));
            let value = b.ins().load(types::I64, MemFlagsData::new(), pointer, 0);
            let value = b.ins().iadd_imm_s(value, 1);
            b.ins().store(MemFlagsData::new(), value, pointer, 0);
            b.ins().jump(done, &[result.into()]);
        } else {
            b.ins().jump(done, &[result.into()]);
        }
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
    (owner, entry)
}

#[test]
fn packed_native_writers_match_model_and_preserve_identity_and_refusals() {
    let (_owner, entry) = compile(false);
    let invoke = |slots, bridge: &Bridge, terminal, index, tag, bits| unsafe {
        entry(slots, bridge, u32::from(terminal), index, tag, bits)
    };
    check_kinds(invoke);
    check_bounds(invoke);
    check_reentry(invoke);
    check_stopped(invoke);
}

#[test]
fn packed_native_writer_sequence_recovers_then_refreshes_before_numeric_access() {
    let (_owner, entry) = compile(true);
    sequence::check(|slots, bridge, terminal, tag, bits| unsafe {
        entry(slots, bridge, u32::from(terminal), 0, tag, bits)
    });
}
