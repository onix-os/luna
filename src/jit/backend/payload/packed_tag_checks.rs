use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Exact(u64),
    Numeric,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Source {
    pub tag: Inst,
    index: u32,
    inst: Inst,
    result: IrValue,
    kind: Kind,
}

pub(super) struct Check {
    source: Source,
    packed: IrValue,
    first: Inst,
}

fn definition(f: &Function, v: IrValue) -> Option<Inst> {
    match f.dfg.value_def(v) {
        cranelift_codegen::ir::ValueDef::Result(inst, 0) => Some(inst),
        _ => None,
    }
}

pub(super) fn source(f: &Function, inst: Inst, slots: IrValue, registers: usize) -> Option<Source> {
    let InstructionData::IntCompare {
        cond: IntCC::Equal,
        args,
        ..
    } = f.dfg.insts[inst]
    else {
        return None;
    };
    let result = reads::compare(f, inst, IntCC::Equal, args).ok()?;
    let constant = definition(f, args[1])?;
    let InstructionData::UnaryImm {
        opcode: Opcode::Iconst,
        imm,
    } = f.dfg.insts[constant]
    else {
        return None;
    };
    let expected = i64::from(imm);
    if !(abi::NIL as i64..=abi::REFERENCE as i64).contains(&expected) {
        return None;
    }
    reads::constant(f, constant, expected).ok()?;
    let mut tag = definition(f, args[0])?;
    let kind = if let InstructionData::Binary {
        opcode: Opcode::Band,
        args,
    } = f.dfg.insts[tag]
    {
        if expected != abi::INTEGER as i64 {
            return None;
        }
        reads::value(f, tag, Opcode::Band, &args, types::I64).ok()?;
        reads::constant(f, definition(f, args[1])?, -2).ok()?;
        tag = definition(f, args[0])?;
        Kind::Numeric
    } else {
        Kind::Exact(expected as u64)
    };
    let InstructionData::Load { offset, .. } = f.dfg.insts[tag] else {
        return None;
    };
    if i32::from(offset) % 16 != 0 {
        return None;
    }
    packed_reads::admit(f, tag, slots, registers).ok()?;
    Some(Source {
        tag,
        index: i32::from(offset) as u32 / 16,
        inst,
        result,
        kind,
    })
}

impl Source {
    pub(super) fn added(self) -> usize {
        match self.kind {
            Kind::Exact(abi::NIL | abi::REFERENCE) => 1,
            Kind::Exact(abi::BOOLEAN) => 3,
            _ => 6,
        }
    }
}

pub(super) fn emit(f: &mut Function, source: Source, packed: IrValue) -> Check {
    let mut c = FuncCursor::new(f);
    c.goto_inst(source.inst);
    match source.kind {
        Kind::Exact(abi::NIL | abi::REFERENCE) => {
            let encoding = if source.kind == Kind::Exact(abi::NIL) {
                4
            } else {
                5
            };
            let expected = c.ins().iconst(types::I64, encoding);
            c.func
                .replace(source.inst)
                .icmp(IntCC::Equal, packed, expected);
        }
        Kind::Exact(abi::BOOLEAN) => {
            let mask = c.ins().iconst(types::I64, -2);
            let selected = c.ins().band(packed, mask);
            let expected = c.ins().iconst(types::I64, 2);
            c.func
                .replace(source.inst)
                .icmp(IntCC::Equal, selected, expected);
        }
        kind => {
            let mask = c.ins().iconst(types::I64, 3);
            let low = c.ins().band(packed, mask);
            let expected = c
                .ins()
                .iconst(types::I64, i64::from(kind != Kind::Exact(abi::INTEGER)));
            let test = c.ins().icmp(
                if kind == Kind::Numeric {
                    IntCC::UnsignedLessThanOrEqual
                } else {
                    IntCC::Equal
                },
                low,
                expected,
            );
            let top = c.ins().iconst(types::I64, 5);
            let above = c.ins().icmp(IntCC::UnsignedGreaterThan, packed, top);
            c.func.replace(source.inst).band(test, above);
        }
    }
    let first = std::iter::successors(Some(source.inst), |&i| c.func.layout.prev_inst(i))
        .nth(source.added())
        .unwrap();
    Check {
        source,
        packed,
        first,
    }
}

impl Check {
    pub(super) fn tag(&self) -> Inst {
        self.source.tag
    }

    pub(super) fn verify_source(
        &self,
        f: &Function,
        slots: IrValue,
        registers: usize,
        snapshot: (Inst, IrValue),
    ) -> Result<(), CompileError> {
        if source(f, self.source.inst, slots, registers) != Some(self.source)
            || snapshot != (self.source.tag, self.packed)
        {
            return Err(invalid());
        }
        Ok(())
    }

    pub(super) fn fragment(
        &self,
        f: &Function,
    ) -> Result<packed_lower::Fragment<'_>, CompileError> {
        let mut cursor = Some(self.first);
        let mut take = || {
            let inst = cursor.ok_or_else(invalid)?;
            cursor = f.layout.next_inst(inst);
            Ok::<_, CompileError>(inst)
        };
        let result = match self.source.kind {
            Kind::Exact(abi::NIL | abi::REFERENCE) => {
                let encoding = if self.source.kind == Kind::Exact(abi::NIL) {
                    4
                } else {
                    5
                };
                let expected = reads::constant(f, take()?, encoding)?;
                reads::compare(f, take()?, IntCC::Equal, [self.packed, expected])?
            }
            Kind::Exact(abi::BOOLEAN) => {
                let mask = reads::constant(f, take()?, -2)?;
                let selected =
                    reads::value(f, take()?, Opcode::Band, &[self.packed, mask], types::I64)?;
                let expected = reads::constant(f, take()?, 2)?;
                reads::compare(f, take()?, IntCC::Equal, [selected, expected])?
            }
            kind => {
                let mask = reads::constant(f, take()?, 3)?;
                let low = reads::value(f, take()?, Opcode::Band, &[self.packed, mask], types::I64)?;
                let expected =
                    reads::constant(f, take()?, i64::from(kind != Kind::Exact(abi::INTEGER)))?;
                let test = reads::compare(
                    f,
                    take()?,
                    if kind == Kind::Numeric {
                        IntCC::UnsignedLessThanOrEqual
                    } else {
                        IntCC::Equal
                    },
                    [low, expected],
                )?;
                let top = reads::constant(f, take()?, 5)?;
                let above =
                    reads::compare(f, take()?, IntCC::UnsignedGreaterThan, [self.packed, top])?;
                reads::value(f, take()?, Opcode::Band, &[test, above], types::I8)?
            }
        };
        if result != self.source.result
            || cursor != f.layout.next_inst(self.source.inst)
            || f.dfg.value_type(self.packed) != types::I64
        {
            return Err(invalid());
        }
        Ok(packed_lower::Fragment {
            source: self.source.inst,
            first: self.first,
            next: cursor,
            blocks: &[],
        })
    }
}

#[cfg(test)]
#[path = "packed_tag_check_tests.rs"]
mod tests;
