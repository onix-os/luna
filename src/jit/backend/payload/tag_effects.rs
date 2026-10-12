use super::*;

#[cfg(test)]
#[path = "tag_effect_tests.rs"]
mod tests;

#[derive(Clone, Copy)]
pub(super) enum Effects {
    Opaque,
    SourceSlots,
    BoundMetadata,
}

impl Effects {
    pub(super) fn preserves(
        self,
        f: &Function,
        inst: Inst,
        slots: IrValue,
        index: u32,
        stores: Option<&[bool]>,
    ) -> bool {
        let InstructionData::Store {
            opcode: Opcode::Store,
            flags,
            args: [bits, address],
            offset,
        } = f.dfg.insts[inst]
        else {
            return false;
        };
        if f.dfg.mem_flags[flags] != MemFlagsData::new()
            || !matches!(f.dfg.value_type(bits), types::I64 | types::F64)
            || index >= 256
        {
            return false;
        }
        let offset = i32::from(offset);
        match self {
            Self::Opaque => false,
            Self::SourceSlots => {
                stores.and_then(|s| s.get(inst.as_u32() as usize)).copied() == Some(true)
                    && address == slots
                    && (0..4096).contains(&offset)
                    && offset % 8 == 0
                    && offset != index as i32 * 16
            }
            Self::BoundMetadata => {
                let Some(pointer) = f.layout.prev_inst(inst) else {
                    return false;
                };
                let InstructionData::Load {
                    opcode: Opcode::Load,
                    arg,
                    flags,
                    offset: field,
                } = f.dfg.insts[pointer]
                else {
                    return false;
                };
                let field = i32::from(field);
                offset == 0
                    && arg == slots
                    && f.dfg.mem_flags[flags] == MemFlagsData::new()
                    && (8..4096).contains(&field)
                    && field % 16 == 8
                    && f.dfg.inst_results(pointer) == [address]
                    && f.dfg.value_type(address) == types::I64
            }
        }
    }
}
