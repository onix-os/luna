use super::*;
use crate::jit::abi::payload::packed::scoped::bridge::entry;
use crate::jit::entry_flow::Paths;

#[derive(Clone, Copy)]
pub(super) struct Site {
    source: Inst,
    pc: usize,
    count: IrValue,
    exit: IrValue,
    kind: u32,
}

impl Site {
    pub(super) fn new(
        source: &Function,
        inst: Inst,
        paths: &Paths,
        headers: &[Block],
        kind: u32,
    ) -> Result<Self, JitError> {
        if !matches!(kind, entry::STORE_FAILURE | entry::HELPER_FAILURE) {
            return Err(invalid());
        }
        let pc = paths.instruction_owner(source, inst).ok_or_else(invalid)?;
        let header = *headers.get(pc).ok_or_else(invalid)?;
        let [count] = source.dfg.block_params(header) else {
            return Err(invalid());
        };
        let entry = source.layout.entry_block().ok_or_else(invalid)?;
        let exit = *source.dfg.block_params(entry).get(3).ok_or_else(invalid)?;
        if source.dfg.value_type(*count) != types::I32
            || source.dfg.value_type(exit) != types::I64
            || pc > u32::MAX as usize
        {
            return Err(invalid());
        }
        Ok(Self {
            source: inst,
            pc,
            count: *count,
            exit,
            kind,
        })
    }

    pub(super) fn verify_source(
        &self,
        source: &Function,
        inst: Inst,
        paths: &Paths,
        headers: &[Block],
        kind: u32,
    ) -> Result<(), JitError> {
        let expected = Self::new(source, inst, paths, headers, kind)?;
        if self.source != inst
            || self.pc != expected.pc
            || self.count != expected.count
            || self.exit != expected.exit
            || self.kind != expected.kind
        {
            return Err(invalid());
        }
        Ok(())
    }

    pub(super) fn emit(&self, f: &mut Function, block: Block) -> Result<(), JitError> {
        let [ret] = shape(f.layout.block_insts(block))?;
        if f.dfg.insts[ret].opcode() != Opcode::Return {
            return Err(invalid());
        }
        let mut c = FuncCursor::new(f);
        c.goto_inst(ret);
        let pc = c.ins().iconst(types::I64, self.pc as i64);
        c.ins().store(MemFlagsData::new(), pc, self.exit, 0);
        c.ins().store(MemFlagsData::new(), self.count, self.exit, 8);
        let kind = c.ins().iconst(types::I32, self.kind as i64);
        c.ins().store(MemFlagsData::new(), kind, self.exit, 12);
        Ok(())
    }
}

#[test]
fn packed_progress_checks_source_ownership_and_every_emitted_field() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(ctx, None, b"return").unwrap();
        let snapshot = Snapshot::new(&closure.prototype(), 4096, 2 * 1024 * 1024).unwrap();
        let mut paths = Paths::new(&snapshot).unwrap();
        let mut f = Function::new();
        f.signature.call_conv = cranelift_codegen::isa::CallConv::SystemV;
        f.signature.params.extend(
            [types::I64, types::I64, types::I32, types::I64, types::I64].map(AbiParam::new),
        );
        f.signature.returns.push(AbiParam::new(types::I32));
        let mut frontend = FunctionBuilderContext::new();
        let (header, body, ret, budget, alternate_exit, status);
        {
            let mut b = FunctionBuilder::new(&mut f, &mut frontend);
            let start = b.create_block();
            header = b.create_block();
            body = b.create_block();
            b.append_block_params_for_function_params(start);
            b.append_block_param(header, types::I32);
            b.switch_to_block(start);
            budget = b.block_params(start)[2];
            alternate_exit = b.block_params(start)[0];
            b.ins().jump(header, &[budget.into()]);
            b.switch_to_block(header);
            b.ins().jump(body, &[]);
            b.switch_to_block(body);
            status = b.ins().iconst(types::I32, 3);
            ret = b.ins().return_(&[status]);
            paths.record(crate::jit::entry_flow::Point {
                trampoline: start,
                body,
                region: [body.as_u32(), body.as_u32() + 1],
            });
            b.seal_all_blocks();
            let isa = cranelift_codegen::isa::lookup_by_name(std::env::consts::ARCH)
                .unwrap()
                .finish(settings::Flags::new(settings::builder()))
                .unwrap();
            b.finalize(isa.frontend_config());
        }
        let site = Site::new(&f, ret, &paths, &[header], entry::STORE_FAILURE).unwrap();
        site.verify_source(&f, ret, &paths, &[header], entry::STORE_FAILURE)
            .unwrap();
        for mode in 0..5 {
            let mut changed = site;
            match mode {
                0 => changed.pc += 1,
                1 => changed.count = budget,
                2 => changed.exit = alternate_exit,
                3 => changed.source = f.layout.first_inst(body).unwrap(),
                _ => changed.kind = entry::HELPER_FAILURE,
            }
            assert!(changed
                .verify_source(&f, ret, &paths, &[header], entry::STORE_FAILURE)
                .is_err());
        }
        let source_size = size(&f);
        site.emit(&mut f, body).unwrap_err();
        let constant = f.layout.first_inst(body).unwrap();
        f.layout.remove_inst(constant);
        let jump = f.layout.last_inst(header).unwrap();
        let mut cursor = FuncCursor::new(&mut f);
        cursor.goto_inst(jump);
        cursor.insert_inst(constant);
        site.emit(&mut f, body).unwrap();
        assert_eq!(size(&f), (source_size.0 + 5, source_size.1));
        assert_eq!(
            verify(&f, body, Some(&site), entry::STORE_FAILURE).unwrap(),
            ret
        );
        let flags = settings::Flags::new(settings::builder());
        cranelift_codegen::verify_function(&f, &flags).unwrap();
        let instructions: Vec<_> = f.layout.block_insts(body).take(5).collect();
        for (index, inst) in instructions.into_iter().enumerate() {
            let mut changed = f.clone();
            match &mut changed.dfg.insts[inst] {
                InstructionData::UnaryImm { imm, .. } => *imm = (i64::from(*imm) + 1).into(),
                InstructionData::Store { args, .. } if index == 2 => args[0] = status,
                InstructionData::Store { offset, .. } => *offset = (i32::from(*offset) + 8).into(),
                _ => panic!("unexpected failure field"),
            }
            cranelift_codegen::verify_function(&changed, &flags).unwrap();
            assert!(verify(&changed, body, Some(&site), entry::STORE_FAILURE).is_err());
        }
    });
}

pub(super) fn verify(
    f: &Function,
    block: Block,
    site: Option<&Site>,
    kind: u32,
) -> Result<Inst, JitError> {
    let Some(site) = site else {
        let [ret] = shape(f.layout.block_insts(block))?;
        return Ok(ret);
    };
    if site.kind != kind {
        return Err(invalid());
    }
    let [pc, pc_store, count_store, reason, reason_store, ret] =
        shape(f.layout.block_insts(block))?;
    let pc = reads::constant(f, pc, site.pc as i64)?;
    let reason_value = reads::value(f, reason, Opcode::Iconst, &[], types::I32)?;
    if !matches!(f.dfg.insts[reason], InstructionData::UnaryImm { imm, .. } if i64::from(imm) == kind as i64)
    {
        return Err(invalid());
    }
    for (inst, value, expected_offset) in [
        (pc_store, pc, 0),
        (count_store, site.count, 8),
        (reason_store, reason_value, 12),
    ] {
        if !matches!(f.dfg.insts[inst], InstructionData::Store { opcode: Opcode::Store, args, offset, flags }
            if args == [value, site.exit] && i32::from(offset) == expected_offset && f.dfg.mem_flags[flags] == MemFlagsData::new())
        {
            return Err(invalid());
        }
    }
    Ok(ret)
}
