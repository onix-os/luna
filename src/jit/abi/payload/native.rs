use cranelift_codegen::ir::{
    condcodes::IntCC, types, AbiParam, Block, InstBuilder, MemFlagsData, Value as IrValue,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Switch};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Module};

use super::*;

type Entry = unsafe extern "C" fn(*mut Payload, *mut (), MoveHelper, u64, u64) -> u32;

struct Code(Option<JITModule>);

impl Drop for Code {
    fn drop(&mut self) {
        unsafe { self.0.take().unwrap().free_memory() };
    }
}

fn emit_store(
    b: &mut FunctionBuilder<'_>,
    slots: IrValue,
    index: usize,
    tag: IrValue,
    bits: IrValue,
    decline: Block,
) {
    let offset = i32::try_from(index * std::mem::size_of::<Payload>()).unwrap();
    let actual = b.ins().load(types::I64, MemFlagsData::new(), slots, offset);
    let same = b.ins().icmp(IntCC::Equal, actual, tag);
    let dispatch = b.create_block();
    b.ins().brif(same, dispatch, &[], decline, &[]);
    b.switch_to_block(dispatch);
    let pointer = b.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots,
        offset + std::mem::offset_of!(Payload, pointer) as i32,
    );
    let wide = b.create_block();
    let boolean = b.create_block();
    let nil = b.create_block();
    let done = b.create_block();
    let mut switch = Switch::new();
    switch.set_entry(INTEGER.into(), wide);
    switch.set_entry(NUMBER.into(), wide);
    switch.set_entry(BOOLEAN.into(), boolean);
    switch.set_entry(NIL.into(), nil);
    switch.emit(b, actual, decline);
    b.switch_to_block(wide);
    let nonnull = b.ins().icmp_imm_u(IntCC::NotEqual, pointer, 0);
    let write_wide = b.create_block();
    b.ins().brif(nonnull, write_wide, &[], decline, &[]);
    b.switch_to_block(write_wide);
    b.ins().store(MemFlagsData::new(), bits, pointer, 0);
    b.ins().jump(done, &[]);
    b.switch_to_block(boolean);
    let nonnull = b.ins().icmp_imm_u(IntCC::NotEqual, pointer, 0);
    let valid = b.ins().icmp_imm_u(IntCC::UnsignedLessThanOrEqual, bits, 1);
    let valid = b.ins().band(nonnull, valid);
    let write_boolean = b.create_block();
    b.ins().brif(valid, write_boolean, &[], decline, &[]);
    b.switch_to_block(write_boolean);
    let byte = b.ins().ireduce(types::I8, bits);
    b.ins().store(MemFlagsData::new(), byte, pointer, 0);
    b.ins().jump(done, &[]);
    b.switch_to_block(nil);
    let null = b.ins().icmp_imm_u(IntCC::Equal, pointer, 0);
    b.ins().brif(null, done, &[], decline, &[]);
    b.switch_to_block(done);
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
    let mut helper = module.make_signature();
    helper
        .params
        .extend([types::I64, types::I64, types::I32, types::I32, types::I8].map(AbiParam::new));
    helper.returns.push(AbiParam::new(types::I32));
    let mut frontend = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut context.func, &mut frontend);
        let helper = b.import_signature(helper);
        let entry = b.create_block();
        let decline = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let [slots, host, callee, tag, bits]: [_; 5] = b.block_params(entry).try_into().unwrap();
        emit_store(&mut b, slots, 0, tag, bits, decline);
        let source = b.ins().iconst(types::I32, 0);
        let dest = b.ins().iconst(types::I32, 1);
        let no_panic = b.ins().iconst(types::I8, 0);
        let call = b
            .ins()
            .call_indirect(helper, callee, &[host, slots, dest, source, no_panic]);
        let result = b.inst_results(call)[0];
        let complete =
            b.ins()
                .icmp_imm_u(IntCC::Equal, result, super::super::HELPER_COMPLETED as i64);
        let rebound = b.create_block();
        b.ins().brif(complete, rebound, &[], decline, &[]);
        b.switch_to_block(rebound);
        emit_store(&mut b, slots, 1, tag, bits, decline);
        let source = b.ins().iconst(types::I32, 1);
        let dest = b.ins().iconst(types::I32, 2);
        let panic = b.ins().iconst(types::I8, 1);
        let call = b
            .ins()
            .call_indirect(helper, callee, &[host, slots, dest, source, panic]);
        let result = b.inst_results(call)[0];
        b.ins().return_(&[result]);
        b.switch_to_block(decline);
        let refused = b
            .ins()
            .iconst(types::I32, super::super::HELPER_DECLINED as i64);
        b.ins().return_(&[refused]);
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
fn generated_payload_stores_reload_after_helpers_and_preserve_unwind_state() {
    with_entry(|entry| {
        crate::Lua::empty().enter(|ctx| {
            let table = crate::Table::new(&ctx);
            for initial in [
                Value::Integer(0),
                Value::Number(0.0),
                Value::Boolean(false),
                Value::Nil,
            ] {
                let tag = Slot::from_value(initial).tag;
                for bits in [0, 1, u64::MAX, 0x7ff8000000000042, 0x8000000000000000] {
                    if tag == BOOLEAN && bits > 1 {
                        continue;
                    }
                    let mut values = [initial, Value::Nil, Value::Table(table)];
                    {
                        let mut host = Gateway {
                            frame: Frame::new(&mut values),
                            panic: None,
                        };
                        let mut slots: Vec<_> = (0..host.frame.len)
                            .map(|i| host.frame.bind(i).unwrap())
                            .collect();
                        let result = unsafe {
                            entry(
                                slots.as_mut_ptr(),
                                std::ptr::from_mut(&mut host).cast(),
                                move_helper,
                                tag,
                                bits,
                            )
                        };
                        assert_eq!(result, super::super::HELPER_PANICKED);
                        assert!(host.panic.is_some());
                    }
                    for value in values {
                        let actual = Slot::from_value(value);
                        assert_eq!(
                            (actual.tag, actual.bits),
                            (tag, if tag == NIL { 0 } else { bits })
                        );
                    }
                }
            }
        });
    });
}

#[test]
fn generated_payload_guards_refuse_without_canonical_writes() {
    with_entry(|entry| {
        crate::Lua::empty().enter(|ctx| {
            let table = crate::Table::new(&ctx);
            for (value, tag, bits, null) in [
                (Value::Integer(7), NUMBER, 42, false),
                (Value::Integer(7), INTEGER, 42, true),
                (Value::Boolean(false), BOOLEAN, 2, false),
                (Value::Table(table), REFERENCE, 0, false),
            ] {
                let mut values = [value, Value::Nil, Value::Table(table)];
                {
                    let mut host = Gateway {
                        frame: Frame::new(&mut values),
                        panic: None,
                    };
                    let mut slots: Vec<_> = (0..host.frame.len)
                        .map(|i| host.frame.bind(i).unwrap())
                        .collect();
                    if null {
                        slots[0].pointer = std::ptr::null_mut();
                    }
                    let result = unsafe {
                        entry(
                            slots.as_mut_ptr(),
                            std::ptr::from_mut(&mut host).cast(),
                            move_helper,
                            tag,
                            bits,
                        )
                    };
                    assert_eq!(result, super::super::HELPER_DECLINED);
                    assert!(host.panic.is_none());
                }
                let actual = Slot::from_value(values[0]);
                let expected = Slot::from_value(value);
                assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
                if matches!(value, Value::Table(_)) {
                    assert!(matches!(values[0], Value::Table(t) if t == table));
                }
                assert!(matches!(values[1], Value::Nil));
                assert!(matches!(values[2], Value::Table(t) if t == table));
            }
        });
    });
}
