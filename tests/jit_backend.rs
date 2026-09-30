#![cfg(all(
    feature = "jit",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use cranelift_codegen::ir::{types, AbiParam, InstBuilder, UserFuncName};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, FuncId, Linkage, Module};

extern "C" fn double(value: i64) -> i64 {
    value.wrapping_mul(2)
}

fn compile_probe() -> (JITModule, FuncId) {
    let mut jit_builder = JITBuilder::new(default_libcall_names()).unwrap();
    jit_builder.symbol("luna_abi_double", double as *const u8);
    let mut module = JITModule::new(jit_builder);
    let mut signature = module.make_signature();
    signature.params.push(AbiParam::new(types::I64));
    signature.returns.push(AbiParam::new(types::I64));
    let helper = module
        .declare_function("luna_abi_double", Linkage::Import, &signature)
        .unwrap();
    let function = module
        .declare_function("luna_abi_probe", Linkage::Local, &signature)
        .unwrap();
    let mut context = module.make_context();
    context.func.signature = signature;
    context.func.name = UserFuncName::user(0, function.as_u32());
    let mut builder_context = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut context.func, &mut builder_context);
        let entry = builder.create_block();
        builder.switch_to_block(entry);
        builder.append_block_params_for_function_params(entry);
        let argument = builder.block_params(entry)[0];
        let helper = module.declare_func_in_func(helper, builder.func);
        let call = builder.ins().call(helper, &[argument]);
        let doubled = builder.inst_results(call)[0];
        let result = builder.ins().iadd_imm_s(doubled, 2);
        builder.ins().return_(&[result]);
        builder.seal_all_blocks();
        builder.finalize(module.target_config());
    }
    module.define_function(function, &mut context).unwrap();
    module.finalize_definitions().unwrap();
    (module, function)
}

#[test]
fn generated_code_calls_a_rust_helper_from_rx_memory() {
    let (module, function) = compile_probe();
    let code = module.get_finalized_function(function);
    let address = code as usize;
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    let mapping = maps
        .lines()
        .find(|line| {
            let range = line.split_whitespace().next().unwrap();
            let (start, end) = range.split_once('-').unwrap();
            let start = usize::from_str_radix(start, 16).unwrap();
            let end = usize::from_str_radix(end, 16).unwrap();
            (start..end).contains(&address)
        })
        .unwrap();
    let permissions = mapping.split_whitespace().nth(1).unwrap();
    assert!(permissions.contains('x'));
    assert!(!permissions.contains('w'));
    let entry: unsafe extern "C" fn(i64) -> i64 = unsafe { std::mem::transmute(code) };
    assert_eq!(unsafe { entry(20) }, 42);
    assert_eq!(unsafe { entry(-4) }, -6);
    unsafe { module.free_memory() };
}

#[test]
fn worker_owned_code_can_transfer_to_the_invoking_thread() {
    let (module, function) = std::thread::spawn(compile_probe).join().unwrap();
    let code = module.get_finalized_function(function);
    let entry: unsafe extern "C" fn(i64) -> i64 = unsafe { std::mem::transmute(code) };
    assert_eq!(unsafe { entry(20) }, 42);
    unsafe { module.free_memory() };
}
