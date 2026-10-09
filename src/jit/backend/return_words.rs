use super::*;

#[test]
fn native_two_word_return_matches_rust_c_aggregate_abi() {
    let module = JITModule::new(native_builder(cranelift_native::builder()).unwrap());
    let mut function = cranelift_codegen::ir::Function::new();
    function.signature = module.make_signature();
    let pointer = module.target_config().pointer_type();
    function.signature.params.extend(
        [pointer, types::I64, types::I32, pointer]
            .into_iter()
            .map(AbiParam::new),
    );
    function
        .signature
        .returns
        .extend([types::I64, types::I64].into_iter().map(AbiParam::new));
    let mut context = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut function, &mut context);
        let entry = builder.create_block();
        let host = builder.create_block();
        let finish = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let args: [_; 4] = builder.block_params(entry).try_into().unwrap();
        let reason = builder
            .ins()
            .load(types::I64, MemFlagsData::new(), args[0], 0);
        let reason = builder.ins().ishl_imm_u(reason, 32);
        let count = builder.ins().uextend(types::I64, args[2]);
        let counts = builder.ins().bor(reason, count);
        builder
            .ins()
            .store(MemFlagsData::new(), args[1], args[0], 8);
        let present = builder.ins().icmp_imm_u(IntCC::NotEqual, args[3], 0);
        builder.ins().brif(present, host, &[], finish, &[]);
        builder.switch_to_block(host);
        let data = builder.ins().load(pointer, MemFlagsData::new(), args[3], 0);
        builder.ins().store(MemFlagsData::new(), args[2], data, 0);
        builder.ins().jump(finish, &[]);
        builder.switch_to_block(finish);
        builder.ins().return_(&[args[1], counts]);
        builder.seal_all_blocks();
        builder.finalize(module.target_config());
    }
    cranelift_codegen::verify_function(&function, module.isa()).unwrap();
    drop(module);
    projection_probe::with_projection_probe(function, |pointer| {
        let entry = unsafe { std::mem::transmute::<*const u8, abi::return_words::Entry>(pointer) };
        abi::return_words::check_entry(entry);
    })
    .unwrap();
}
