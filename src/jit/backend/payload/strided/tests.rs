use super::*;

fn fixture(pointer: bool, index: usize) -> (Function, IrValue, Read) {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature.params.push(AbiParam::new(types::I64));
    f.signature.returns.push(AbiParam::new(types::I64));
    let mut context = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut f, &mut context);
    let block = b.create_block();
    b.append_block_params_for_function_params(block);
    b.switch_to_block(block);
    let slots = b.block_params(block)[0];
    let result = b.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots,
        (16 * index + if pointer { 8 } else { 0 }) as i32,
    );
    let inst = b.func.dfg.value_def(result).inst().unwrap();
    b.ins().return_(&[result]);
    b.seal_all_blocks();
    let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
        .unwrap()
        .finish(settings::Flags::new(settings::builder()))
        .unwrap();
    b.finalize(isa.frontend_config());
    (
        f,
        slots,
        Read {
            inst,
            index,
            pointer,
            result,
            uniform: if pointer {
                Header::uniform_offset()
            } else {
                None
            },
            numeric: false,
        },
    )
}

fn memory(limit: usize) -> BudgetAllocator {
    BudgetAllocator(crate::jit::resources::Ledger::new(limit))
}

#[test]
fn strided_expansion_accounts_nil_fanout_without_changing_limits() {
    crate::Lua::empty().enter(|ctx| {
        let closure =
            crate::Closure::load(ctx, None, b"local a,b,c,d,e,f,g return 'anchored'").unwrap();
        let source = Snapshot::new(&closure.prototype(), 4096, 2 * 1024 * 1024).unwrap();
        let limits = crate::jit::work::Limits::from(&crate::jit::JitConfig::default());
        let original = super::super::expansion(&source, limits).unwrap();
        let actual = expansion(&source, limits).unwrap();
        assert_eq!(actual.instructions, original.instructions + 17 * 7);
        assert_eq!(actual.blocks, original.blocks);
        assert!(matches!(
            expansion(
                &source,
                crate::jit::work::Limits {
                    instructions: actual.instructions - 1,
                    ..limits
                }
            ),
            Err(JitError::ResourceLimit("IR instructions"))
        ));
        assert_eq!(
            expansion(
                &source,
                crate::jit::work::Limits {
                    instructions: actual.instructions,
                    ..limits
                }
            )
            .unwrap(),
            actual
        );
    });
}

#[test]
fn strided_lowering_preserves_result_and_checks_quota_atomically() {
    for pointer in [false, true] {
        for index in [0, 6, 63, 64, 255] {
            let (source, slots, read) = fixture(pointer, index);
            let expansion = crate::jit::work::Expansion {
                instructions: 32,
                blocks: 1,
            };
            let quota = memory(1 << 20);
            let mut lowered = source.clone();
            lower(&mut lowered, slots, index + 1, quota.clone(), expansion).unwrap();
            verify(&lowered, slots, read).unwrap();
            cranelift_codegen::verify_function(
                &lowered,
                &settings::Flags::new(settings::builder()),
            )
            .unwrap();
            assert_eq!(size(&lowered), (size(&source).0 + read.extra(), 1));
            assert_eq!(quota.0.current(), 0);
            let peak = quota.0.peak();
            assert!(peak > 0);
            for limit in [0, peak - 1, peak] {
                let quota = memory(limit);
                let mut candidate = source.clone();
                assert_eq!(
                    lower(&mut candidate, slots, index + 1, quota.clone(), expansion).is_ok(),
                    limit == peak
                );
                assert_eq!(quota.0.current(), 0);
                if limit < peak {
                    assert_eq!(candidate, source);
                }
            }
            let mut candidate = source.clone();
            assert!(lower(
                &mut candidate,
                slots,
                index + 1,
                memory(1 << 20),
                crate::jit::work::Expansion {
                    instructions: size(&lowered).0 - 1,
                    blocks: 1
                }
            )
            .is_err());
            assert_eq!(candidate, source);
        }
    }
}

#[test]
fn strided_checker_rejects_changed_addresses_tags_widths_and_guards() {
    for (pointer, uniform) in [(false, false), (true, false), (true, true)] {
        let (mut f, slots, mut read) = fixture(pointer, 255);
        if !uniform {
            read.uniform = None;
        }
        emit(&mut f, slots, read);
        verify(&f, slots, read).unwrap();
        let block = f.layout.entry_block().unwrap();
        let mut rejected = 0;
        for inst in f
            .layout
            .block_insts(block)
            .filter(|&i| f.dfg.insts[i].opcode() != Opcode::Return)
        {
            let mut changed = f.clone();
            match &mut changed.dfg.insts[inst] {
                InstructionData::Load { offset, .. } => *offset = (i32::from(*offset) + 1).into(),
                InstructionData::UnaryImm { imm, .. } => *imm = (i64::from(*imm) ^ 1).into(),
                InstructionData::Binary { opcode, .. } => *opcode = Opcode::Bxor,
                InstructionData::Unary { opcode, .. } => *opcode = Opcode::Sextend,
                InstructionData::IntCompare { cond, .. } => *cond = IntCC::Equal,
                InstructionData::Ternary { args, .. } => args.swap(1, 2),
                other => panic!("unhandled instruction {other:?}"),
            }
            assert!(verify(&changed, slots, read).is_err());
            rejected += 1;
        }
        assert_eq!(rejected, 1 + read.extra());
    }
}

#[test]
fn strided_uniform_layout_checks_measured_offset_and_keeps_fallback() {
    for index in [0, 6, 255] {
        let (source, slots, mut read) = fixture(true, index);
        if let Some(offset) = read.uniform {
            let mut candidate = source.clone();
            emit(&mut candidate, slots, read);
            verify(&candidate, slots, read).unwrap();
            let altered = Read {
                uniform: Some(offset ^ 1),
                ..read
            };
            assert!(verify(&candidate, slots, altered).is_err());
            assert_eq!(size(&candidate).0, size(&source).0 + 9);
        }
        read.uniform = None;
        let mut fallback = source.clone();
        emit(&mut fallback, slots, read);
        verify(&fallback, slots, read).unwrap();
        assert_eq!(size(&fallback).0, size(&source).0 + 14);
        cranelift_codegen::verify_function(&fallback, &settings::Flags::new(settings::builder()))
            .unwrap();
    }
}

#[test]
fn strided_byte_tag_verifier_rejects_signed_wide_and_retargeted_loads() {
    for (pointer, uniform) in [(false, false), (true, false), (true, true)] {
        let (mut source, slots, mut read) = fixture(pointer, 255);
        if !uniform {
            read.uniform = None;
        }
        emit(&mut source, slots, read);
        verify(&source, slots, read).unwrap();
        let tag = source
            .layout
            .first_inst(source.layout.entry_block().unwrap())
            .unwrap();
        let offset = (std::mem::offset_of!(Header, tags) + read.index) as i32;
        for fault in 0..6 {
            let mut changed = source.clone();
            let flags = MemFlagsData::new();
            match fault {
                0 => {
                    changed
                        .replace(tag)
                        .sload8(types::I64, flags, slots, offset);
                }
                1 => {
                    changed
                        .replace(tag)
                        .uload16(types::I64, flags, slots, offset);
                }
                2 => {
                    changed.replace(tag).load(types::I64, flags, slots, offset);
                }
                3 => {
                    let mut flags = flags;
                    flags.set_readonly();
                    changed
                        .replace(tag)
                        .uload8(types::I64, flags, slots, offset);
                }
                4 => {
                    changed
                        .replace(tag)
                        .uload8(types::I64, flags, slots, offset - 1);
                }
                _ => {
                    let mut c = FuncCursor::new(&mut changed);
                    c.goto_inst(tag);
                    let other = c.ins().iconst(types::I64, 0);
                    c.func.replace(tag).uload8(types::I64, flags, other, offset);
                }
            }
            cranelift_codegen::verify_function(
                &changed,
                &settings::Flags::new(settings::builder()),
            )
            .unwrap();
            assert!(verify(&changed, slots, read).is_err(), "fault={fault}");
        }
    }
}

#[cfg(not(miri))]
#[test]
fn strided_byte_tag_native_zero_extends_every_byte_at_boundary_registers() {
    for index in [0, 6, 63, 64, 255] {
        let (mut f, slots, read) = fixture(false, index);
        let original = size(&f);
        emit(&mut f, slots, read);
        verify(&f, slots, read).unwrap();
        assert_eq!(size(&f), original);
        crate::jit::backend::projection_probe::with_projection_probe(f, |pointer| {
            let entry = unsafe {
                std::mem::transmute::<*const u8, unsafe extern "C" fn(*const Header) -> u64>(
                    pointer,
                )
            };
            let mut header = Header {
                base: std::ptr::null_mut(),
                offsets: [usize::MAX; 5],
                tags: [0xa5; 256],
            };
            for tag in 0..=u8::MAX {
                header.tags[index] = tag;
                let expected = header.tags;
                assert_eq!(unsafe { entry(&header) }, u64::from(tag));
                assert_eq!(header.tags, expected);
            }
        })
        .unwrap();
    }
}

#[test]
fn strided_lowering_refuses_non_descriptor_accesses() {
    for fault in 0..5 {
        let (mut f, slots, read) = fixture(true, 0);
        match fault {
            0 => {
                f.replace(read.inst)
                    .load(types::I64, MemFlagsData::new(), slots, 7);
            }
            1 => {
                f.replace(read.inst)
                    .load(types::I64, MemFlagsData::new(), slots, 16);
            }
            2 => {
                let mut flags = MemFlagsData::new();
                flags.set_readonly();
                f.replace(read.inst).load(types::I64, flags, slots, 8);
            }
            3 => {
                f.replace(read.inst).iadd(slots, slots);
            }
            _ => {
                f.replace(read.inst)
                    .bitcast(types::F64, MemFlagsData::new(), slots);
            }
        }
        let source = f.clone();
        let quota = memory(1 << 20);
        assert!(lower(
            &mut f,
            slots,
            1,
            quota.clone(),
            crate::jit::work::Expansion {
                instructions: 4096,
                blocks: 32
            }
        )
        .is_err());
        assert_eq!(f, source);
        assert_eq!(quota.0.current(), 0);
    }
}

#[test]
fn strided_scalar_range_verifier_rejects_signed_bounds_and_wrapped_operands() {
    for uniform in [false, true] {
        let (mut source, slots, mut read) = fixture(true, 0);
        if !uniform {
            read.uniform = None;
        }
        emit(&mut source, slots, read);
        verify(&source, slots, read).unwrap();
        let null = source.layout.prev_inst(read.inst).unwrap();
        let scalar = source.layout.prev_inst(null).unwrap();
        let width = source.layout.prev_inst(scalar).unwrap();
        let offset = source.layout.prev_inst(width).unwrap();
        let first = source.layout.prev_inst(offset).unwrap();
        for fault in 0..7 {
            let mut changed = source.clone();
            match fault {
                0 | 1 => {
                    let InstructionData::IntCompare { args, .. } = changed.dfg.insts[scalar] else {
                        unreachable!()
                    };
                    let cond = if fault == 0 {
                        IntCC::SignedLessThanOrEqual
                    } else {
                        IntCC::UnsignedLessThan
                    };
                    changed.replace(scalar).icmp(cond, args[0], args[1]);
                }
                2 => {
                    changed.replace(width).iconst(types::I64, 3);
                }
                3 => {
                    changed.replace(first).iconst(types::I64, 0);
                }
                4 | 5 => {
                    let InstructionData::Binary { args, .. } = changed.dfg.insts[offset] else {
                        unreachable!()
                    };
                    if fault == 4 {
                        changed.replace(offset).isub(args[1], args[0]);
                    } else {
                        changed.replace(offset).iadd(args[0], args[1]);
                    }
                }
                _ => {
                    let InstructionData::Ternary { args, .. } = &mut changed.dfg.insts[read.inst]
                    else {
                        unreachable!()
                    };
                    args.swap(1, 2);
                }
            }
            cranelift_codegen::verify_function(
                &changed,
                &settings::Flags::new(settings::builder()),
            )
            .unwrap();
            assert!(
                verify(&changed, slots, read).is_err(),
                "uniform={uniform} fault={fault}"
            );
        }
    }
}

#[cfg(not(miri))]
#[test]
fn strided_scalar_range_native_matches_every_byte_tag_and_wide_boundary() {
    let mut f = Function::new();
    f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
    f.signature.params.push(AbiParam::new(types::I64));
    f.signature.returns.push(AbiParam::new(types::I64));
    let mut context = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut f, &mut context);
    let block = b.create_block();
    b.append_block_params_for_function_params(block);
    b.switch_to_block(block);
    let tag = b.block_params(block)[0];
    let end = b.ins().return_(&[tag]);
    b.seal_all_blocks();
    let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
        .unwrap()
        .finish(settings::Flags::new(settings::builder()))
        .unwrap();
    b.finalize(isa.frontend_config());
    let mut c = FuncCursor::new(&mut f);
    c.goto_inst(end);
    let scalar = scalar_range::emit(&mut c, tag);
    let result = c.ins().uextend(types::I64, scalar);
    c.func.replace(end).return_(&[result]);
    crate::jit::backend::projection_probe::with_projection_probe(f, |pointer| {
        let entry =
            unsafe { std::mem::transmute::<*const u8, unsafe extern "C" fn(u64) -> u64>(pointer) };
        for tag in (0..=255).chain([256, u32::MAX as u64, 1 << 63, u64::MAX - 1, u64::MAX]) {
            assert_eq!(
                unsafe { entry(tag) },
                u64::from((1..=3).contains(&tag)),
                "tag={tag}"
            );
        }
    })
    .unwrap();
}

#[cfg(not(miri))]
#[test]
fn strided_scalar_range_native_pointers_preserve_null_and_measured_offsets() {
    for index in [0, 6, 255] {
        for uniform in [false, true] {
            let (mut f, slots, mut read) = fixture(true, index);
            if !uniform {
                read.uniform = None;
            }
            if uniform && read.uniform.is_none() {
                continue;
            }
            emit(&mut f, slots, read);
            verify(&f, slots, read).unwrap();
            crate::jit::backend::projection_probe::with_projection_probe(f, |pointer| {
                let entry = unsafe {
                    std::mem::transmute::<*const u8, unsafe extern "C" fn(*const Header) -> usize>(
                        pointer,
                    )
                };
                let mut storage = [0u8; 256 * std::mem::size_of::<crate::Value<'_>>() + 32];
                let mut header = Header {
                    base: storage.as_mut_ptr(),
                    offsets: [0, 1, 8, 8, 0],
                    tags: [0; 256],
                };
                let last = if uniform {
                    u8::MAX
                } else {
                    abi::REFERENCE as u8
                };
                for tag in 0..=last {
                    header.tags[index] = tag;
                    let expected = if (1..=3).contains(&tag) {
                        header.base.addr()
                            + index * std::mem::size_of::<crate::Value<'_>>()
                            + read.uniform.unwrap_or(header.offsets[usize::from(tag)])
                    } else {
                        0
                    };
                    assert_eq!(
                        unsafe { entry(&header) },
                        expected,
                        "index={index} uniform={uniform} tag={tag}"
                    );
                }
            })
            .unwrap();
        }
    }
}
