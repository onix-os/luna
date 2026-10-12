use super::abi::{Slot, REFERENCE};
use crate::Value;

#[repr(C)]
struct Host<'gc> {
    values: *const Value<'gc>,
    count: usize,
}

unsafe extern "C" fn import(host: *const Host<'_>, slots: *mut Slot, index: usize) -> u32 {
    if host.is_null() || slots.is_null() {
        return 0;
    }
    let host = unsafe { &*host };
    if index >= host.count {
        return 0;
    }
    let value = unsafe { host.values.add(index).read() };
    unsafe { slots.add(index).write(Slot::from_value(value)) };
    1
}

fn proxies() -> [Slot; 7] {
    [Slot {
        tag: REFERENCE,
        bits: 0,
    }; 7]
}

type BulkImport = for<'gc> unsafe extern "C" fn(*const Host<'gc>, *mut Slot) -> u32;

unsafe extern "C" fn bulk_import<const MASK: u8>(host: *const Host<'_>, slots: *mut Slot) -> u32 {
    if host.is_null() || slots.is_null() {
        return 0;
    }
    let host = unsafe { &*host };
    if host.count < 7 {
        return 0;
    }
    #[inline(always)]
    unsafe fn cell<const MASK: u8, const INDEX: usize>(host: &Host<'_>) -> Slot {
        if MASK & (1 << INDEX) == 0 {
            Slot::canonical()
        } else {
            Slot::from_value(unsafe { host.values.add(INDEX).read() })
        }
    }
    let values = unsafe {
        [
            cell::<MASK, 0>(host),
            cell::<MASK, 1>(host),
            cell::<MASK, 2>(host),
            cell::<MASK, 3>(host),
            cell::<MASK, 4>(host),
            cell::<MASK, 5>(host),
            cell::<MASK, 6>(host),
        ]
    };
    unsafe { slots.cast::<[Slot; 7]>().write(values) };
    1
}

const BULK_IMPORTS: [BulkImport; 128] = [
    bulk_import::<0>,
    bulk_import::<1>,
    bulk_import::<2>,
    bulk_import::<3>,
    bulk_import::<4>,
    bulk_import::<5>,
    bulk_import::<6>,
    bulk_import::<7>,
    bulk_import::<8>,
    bulk_import::<9>,
    bulk_import::<10>,
    bulk_import::<11>,
    bulk_import::<12>,
    bulk_import::<13>,
    bulk_import::<14>,
    bulk_import::<15>,
    bulk_import::<16>,
    bulk_import::<17>,
    bulk_import::<18>,
    bulk_import::<19>,
    bulk_import::<20>,
    bulk_import::<21>,
    bulk_import::<22>,
    bulk_import::<23>,
    bulk_import::<24>,
    bulk_import::<25>,
    bulk_import::<26>,
    bulk_import::<27>,
    bulk_import::<28>,
    bulk_import::<29>,
    bulk_import::<30>,
    bulk_import::<31>,
    bulk_import::<32>,
    bulk_import::<33>,
    bulk_import::<34>,
    bulk_import::<35>,
    bulk_import::<36>,
    bulk_import::<37>,
    bulk_import::<38>,
    bulk_import::<39>,
    bulk_import::<40>,
    bulk_import::<41>,
    bulk_import::<42>,
    bulk_import::<43>,
    bulk_import::<44>,
    bulk_import::<45>,
    bulk_import::<46>,
    bulk_import::<47>,
    bulk_import::<48>,
    bulk_import::<49>,
    bulk_import::<50>,
    bulk_import::<51>,
    bulk_import::<52>,
    bulk_import::<53>,
    bulk_import::<54>,
    bulk_import::<55>,
    bulk_import::<56>,
    bulk_import::<57>,
    bulk_import::<58>,
    bulk_import::<59>,
    bulk_import::<60>,
    bulk_import::<61>,
    bulk_import::<62>,
    bulk_import::<63>,
    bulk_import::<64>,
    bulk_import::<65>,
    bulk_import::<66>,
    bulk_import::<67>,
    bulk_import::<68>,
    bulk_import::<69>,
    bulk_import::<70>,
    bulk_import::<71>,
    bulk_import::<72>,
    bulk_import::<73>,
    bulk_import::<74>,
    bulk_import::<75>,
    bulk_import::<76>,
    bulk_import::<77>,
    bulk_import::<78>,
    bulk_import::<79>,
    bulk_import::<80>,
    bulk_import::<81>,
    bulk_import::<82>,
    bulk_import::<83>,
    bulk_import::<84>,
    bulk_import::<85>,
    bulk_import::<86>,
    bulk_import::<87>,
    bulk_import::<88>,
    bulk_import::<89>,
    bulk_import::<90>,
    bulk_import::<91>,
    bulk_import::<92>,
    bulk_import::<93>,
    bulk_import::<94>,
    bulk_import::<95>,
    bulk_import::<96>,
    bulk_import::<97>,
    bulk_import::<98>,
    bulk_import::<99>,
    bulk_import::<100>,
    bulk_import::<101>,
    bulk_import::<102>,
    bulk_import::<103>,
    bulk_import::<104>,
    bulk_import::<105>,
    bulk_import::<106>,
    bulk_import::<107>,
    bulk_import::<108>,
    bulk_import::<109>,
    bulk_import::<110>,
    bulk_import::<111>,
    bulk_import::<112>,
    bulk_import::<113>,
    bulk_import::<114>,
    bulk_import::<115>,
    bulk_import::<116>,
    bulk_import::<117>,
    bulk_import::<118>,
    bulk_import::<119>,
    bulk_import::<120>,
    bulk_import::<121>,
    bulk_import::<122>,
    bulk_import::<123>,
    bulk_import::<124>,
    bulk_import::<125>,
    bulk_import::<126>,
    bulk_import::<127>,
];

#[inline(always)]
fn prepare<const SCHEDULED: bool>(
    values: &[Value<'_>; 7],
    scratch: &mut [std::mem::MaybeUninit<Slot>; 7],
) {
    if SCHEDULED {
        for slot in scratch {
            slot.write(Slot::canonical());
        }
    } else {
        for (slot, value) in scratch[..7].iter_mut().zip(values[..7].iter().copied()) {
            slot.write(Slot::from_value(value));
        }
    }
}

#[test]
fn scheduled_reads_preserve_live_values_and_omitted_proxies() {
    crate::Lua::empty().enter(|ctx| {
        let kinds = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8_0000_0000_1234)),
            Value::String(ctx.intern(b"scheduled")),
            Value::Table(crate::Table::new(&ctx)),
            Value::Function(crate::Closure::load(ctx, None, b"return 1").unwrap().into()),
            Value::Function(
                crate::Callback::from_fn(&ctx, |_, _, _| Ok(crate::CallbackReturn::Return)).into(),
            ),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 17)),
        ];
        for kind in kinds {
            for mask in [0u8, 1, 2, 5, 0x2a, 0x55, 0x7e, 0x7f] {
                let mut values = [kind; 7];
                for replacement in [kind, Value::Integer(31), Value::Number(-0.0)] {
                    values[3] = replacement;
                    let host = Host {
                        values: values.as_ptr(),
                        count: values.len(),
                    };
                    let mut slots = proxies();
                    let mut bulk = [std::mem::MaybeUninit::<Slot>::uninit(); 7];
                    assert_eq!(
                        unsafe { BULK_IMPORTS[mask as usize](&host, bulk.as_mut_ptr().cast()) },
                        1
                    );
                    let mut full = [std::mem::MaybeUninit::<Slot>::uninit(); 7];
                    let mut omitted = [std::mem::MaybeUninit::<Slot>::uninit(); 7];
                    prepare::<false>(&values, &mut full);
                    prepare::<true>(&values, &mut omitted);
                    for index in 0..7 {
                        if mask & (1 << index) != 0 {
                            assert_eq!(unsafe { import(&host, slots.as_mut_ptr(), index) }, 1);
                        }
                    }
                    for index in 0..7 {
                        let expected = if mask & (1 << index) != 0 {
                            Slot::from_value(values[index])
                        } else {
                            Slot::canonical()
                        };
                        assert_eq!(
                            (slots[index].tag, slots[index].bits),
                            (expected.tag, expected.bits)
                        );
                        let actual = Slot::from_value(slots[index].value(values[index]));
                        let bulk = unsafe { bulk[index].assume_init() };
                        assert_eq!((bulk.tag, bulk.bits), (slots[index].tag, slots[index].bits));
                        let prepared = unsafe { full[index].assume_init() };
                        let canonical = Slot::from_value(values[index]);
                        assert_eq!(
                            (prepared.tag, prepared.bits),
                            (canonical.tag, canonical.bits)
                        );
                        let proxy = unsafe { omitted[index].assume_init() };
                        assert_eq!((proxy.tag, proxy.bits), (REFERENCE, 0));
                        let expected = Slot::from_value(values[index]);
                        assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
                    }
                }
            }
        }
    });
}

#[test]
fn declined_imports_leave_initialized_slots_unchanged() {
    let values = [Value::Integer(17); 7];
    for count in 0..=7 {
        let host = Host {
            values: values.as_ptr(),
            count,
        };
        let mut slots = proxies();
        for index in [count, 7, usize::MAX] {
            assert_eq!(unsafe { import(&host, slots.as_mut_ptr(), index) }, 0);
        }
        for bulk in BULK_IMPORTS {
            if count < 7 {
                assert_eq!(unsafe { bulk(&host, slots.as_mut_ptr()) }, 0);
            }
            assert_eq!(unsafe { bulk(std::ptr::null(), slots.as_mut_ptr()) }, 0);
            assert_eq!(unsafe { bulk(&host, std::ptr::null_mut()) }, 0);
        }
        assert_eq!(
            unsafe { import(std::ptr::null(), slots.as_mut_ptr(), 0) },
            0
        );
        assert_eq!(unsafe { import(&host, std::ptr::null_mut(), 0) }, 0);
        for slot in slots {
            assert_eq!((slot.tag, slot.bits), (REFERENCE, 0));
        }
    }
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod native {
    use super::*;
    use cranelift_codegen::ir::{types, AbiParam, InstBuilder, MemFlagsData};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
    use cranelift_jit::{JITBuilder, JITModule};
    use cranelift_module::{default_libcall_names, Linkage, Module};
    use std::hint::black_box;

    type Entry = for<'gc> unsafe extern "C" fn(*const Host<'gc>, *mut Slot) -> u64;
    struct Code(Option<JITModule>);
    impl Drop for Code {
        fn drop(&mut self) {
            unsafe { self.0.take().unwrap().free_memory() };
        }
    }

    fn with_entry(mask: u8, mode: u8, body: impl FnOnce(Entry)) {
        assert_eq!(mask & 0x80, 0);
        assert!(mode <= 2);
        let mut builder = JITBuilder::new(default_libcall_names()).unwrap();
        let address = if mode == 2 {
            BULK_IMPORTS[mask as usize] as *const u8
        } else {
            import as *const u8
        };
        builder.symbol("luna_scheduled_import_probe", address);
        let mut owner = Code(Some(JITModule::new(builder)));
        let module = owner.0.as_mut().unwrap();
        let ptr = module.target_config().pointer_type();
        let mut helper_sig = module.make_signature();
        helper_sig.params.extend(std::iter::repeat_n(
            AbiParam::new(ptr),
            if mode == 2 { 2 } else { 3 },
        ));
        helper_sig.returns.push(AbiParam::new(types::I32));
        let helper = module
            .declare_function("luna_scheduled_import_probe", Linkage::Import, &helper_sig)
            .unwrap();
        let mut context = module.make_context();
        context
            .func
            .signature
            .params
            .extend([AbiParam::new(ptr); 2]);
        context
            .func
            .signature
            .returns
            .push(AbiParam::new(types::I64));
        let id = module
            .declare_anonymous_function(&context.func.signature)
            .unwrap();
        let helper = module.declare_func_in_func(helper, &mut context.func);
        let mut frontend = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut context.func, &mut frontend);
            let block = b.create_block();
            let refused = b.create_block();
            b.append_block_params_for_function_params(block);
            b.switch_to_block(block);
            let [host, slots]: [_; 2] = b.block_params(block).try_into().unwrap();
            if mode == 2 {
                let call = b.ins().call(helper, &[host, slots]);
                let status = b.inst_results(call)[0];
                let next = b.create_block();
                b.ins().brif(status, next, &[], refused, &[]);
                b.switch_to_block(next);
            } else if mode == 1 {
                for index in 0..7 {
                    if mask & (1 << index) != 0 {
                        let index = b.ins().iconst(ptr, index);
                        let call = b.ins().call(helper, &[host, slots, index]);
                        let status = b.inst_results(call)[0];
                        let next = b.create_block();
                        b.ins().brif(status, next, &[], refused, &[]);
                        b.switch_to_block(next);
                    }
                }
            }
            let mut sum = b.ins().iconst(types::I64, 0);
            for index in 0..7 {
                if mask & (1 << index) != 0 {
                    let tag = b
                        .ins()
                        .load(types::I64, MemFlagsData::new(), slots, index * 16);
                    let bits = b
                        .ins()
                        .load(types::I64, MemFlagsData::new(), slots, index * 16 + 8);
                    let word = b.ins().bxor(tag, bits);
                    sum = b.ins().iadd(sum, word);
                }
            }
            b.ins().return_(&[sum]);
            b.switch_to_block(refused);
            let failed = b.ins().iconst(types::I64, -1);
            b.ins().return_(&[failed]);
            b.seal_all_blocks();
            b.finalize(module.target_config());
        }
        module.define_function(id, &mut context).unwrap();
        module.finalize_definitions().unwrap();
        let entry: Entry = unsafe { std::mem::transmute(module.get_finalized_function(id)) };
        body(entry);
    }

    fn checksum(values: &[Value<'_>; 7], mask: u8) -> u64 {
        values
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1 << index) != 0)
            .fold(0u64, |sum, (_, value)| {
                let slot = Slot::from_value(*value);
                sum.wrapping_add(slot.tag ^ slot.bits)
            })
    }

    #[test]
    fn generated_imports_match_all_masks_and_refresh_after_mutation() {
        for mask in 0..128 {
            for mode in [1, 2] {
                with_entry(mask, mode, |entry| {
                    let mut values = [Value::Integer(17); 7];
                    for index in 0..7 {
                        values[index] = Value::Number(f64::from_bits(0x7ff8_0000_0000_1234));
                        let host = Host {
                            values: values.as_ptr(),
                            count: 7,
                        };
                        let mut slots = [std::mem::MaybeUninit::<Slot>::uninit(); 7];
                        if mode == 1 {
                            prepare::<true>(&values, &mut slots);
                        }
                        assert_eq!(
                            unsafe { entry(&host, slots.as_mut_ptr().cast()) },
                            checksum(&values, mask)
                        );
                        for i in 0..7 {
                            let expected = if mask & (1 << i) != 0 {
                                Slot::from_value(values[i])
                            } else {
                                Slot::canonical()
                            };
                            let slot = unsafe { slots[i].assume_init() };
                            assert_eq!((slot.tag, slot.bits), (expected.tag, expected.bits));
                        }
                    }
                    if mask != 0 || mode == 2 {
                        let host = Host {
                            values: values.as_ptr(),
                            count: 0,
                        };
                        let mut slots = [std::mem::MaybeUninit::<Slot>::uninit(); 7];
                        assert_eq!(unsafe { entry(&host, slots.as_mut_ptr().cast()) }, u64::MAX);
                    }
                });
            }
        }
    }

    #[inline(never)]
    fn profile_entry<const MODE: u8>(entry: Entry, values: &[Value<'_>; 7]) -> u64 {
        let mut sum = 0u64;
        for _ in 0..100_000 {
            let values = black_box(values);
            let host = Host {
                values: values.as_ptr(),
                count: 7,
            };
            let mut slots = [std::mem::MaybeUninit::<Slot>::uninit(); 7];
            if MODE == 0 {
                prepare::<false>(values, &mut slots);
            }
            if MODE == 1 {
                prepare::<true>(values, &mut slots);
            }
            sum = sum.wrapping_add(unsafe { entry(&host, slots.as_mut_ptr().cast()) });
            black_box(&slots);
        }
        sum
    }

    #[test]
    #[ignore = "boundary-only native import diagnostic, not whole-program acceptance"]
    fn scheduled_import_profile() {
        let width: u8 = std::env::var("LUNA_IMPORT_WIDTH").unwrap().parse().unwrap();
        assert!(width <= 7);
        let mode = match std::env::var("LUNA_IMPORT_MODE").unwrap().as_str() {
            "full" => 0,
            "scheduled" => 1,
            "bulk" => 2,
            _ => panic!("unknown import mode"),
        };
        let mask = (1u8 << width) - 1;
        with_entry(mask, mode, |entry| {
            let values = [
                Value::Integer(17),
                Value::Integer(31),
                Value::Integer(43),
                Value::Integer(59),
                Value::Integer(73),
                Value::Integer(97),
                Value::Nil,
            ];
            let sum = match mode {
                0 => profile_entry::<0>(entry, &values),
                1 => profile_entry::<1>(entry, &values),
                2 => profile_entry::<2>(entry, &values),
                _ => unreachable!(),
            };
            assert_eq!(sum, checksum(&values, mask).wrapping_mul(100_000));
            println!("mode={mode} width={width} entries=100000 verified=1");
        });
    }
}
