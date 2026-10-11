use super::*;

#[cfg(test)]
#[path = "copy_tests.rs"]
mod tests;

#[derive(Clone, Copy)]
pub(super) struct Proof {
    tag_store: Inst,
    bits_store: Inst,
    dest: u32,
    source: u32,
    tag: IrValue,
    bits: IrValue,
}

pub(super) fn source(f: &Function, slots: IrValue, tag: IrValue, bits: IrValue) -> Option<u32> {
    let tag_load = f.dfg.value_def(tag).inst()?;
    let bits_load = f.dfg.value_def(bits).inst()?;
    let InstructionData::Load { offset, .. } = f.dfg.insts[tag_load] else {
        return None;
    };
    let offset = i32::from(offset);
    if !(0..4096).contains(&offset) || offset % 16 != 0 {
        return None;
    }
    if reads::load(f, tag_load, slots, offset, types::I64).ok()? != tag
        || reads::load(f, bits_load, slots, offset + 8, types::I64).ok()? != bits
    {
        return None;
    }
    Some(offset as u32 / 16)
}

pub(super) fn admit(
    f: &Function,
    slots: IrValue,
    tag_store: Inst,
    bits_store: Inst,
    dest: u32,
    tag: IrValue,
    bits: IrValue,
    tags: &mut same_tag::Analysis,
    terminal: &mut terminal::Analysis<'_>,
) -> Option<Proof> {
    let proof = Proof {
        tag_store,
        bits_store,
        dest,
        source: source(f, slots, tag, bits)?,
        tag,
        bits,
    };
    verify(f, slots, proof, tags, terminal, false).then_some(proof)
}

pub(super) fn verify(
    f: &Function,
    slots: IrValue,
    proof: Proof,
    tags: &mut same_tag::Analysis,
    terminal: &mut terminal::Analysis<'_>,
    verification: bool,
) -> bool {
    if proof.dest >= 256
        || source(f, slots, proof.tag, proof.bits) != Some(proof.source)
        || f.layout.next_inst(proof.tag_store) != Some(proof.bits_store)
    {
        return false;
    }
    for (inst, value, offset) in [
        (proof.tag_store, proof.tag, proof.dest as i32 * 16),
        (proof.bits_store, proof.bits, proof.dest as i32 * 16 + 8),
    ] {
        if !matches!(f.dfg.insts[inst], InstructionData::Store {
            opcode: Opcode::Store, args, offset: actual, flags,
        } if args == [value, slots] && i32::from(actual) == offset
            && f.dfg.mem_flags[flags] == MemFlagsData::new())
        {
            return false;
        }
    }
    [proof.tag, proof.bits].into_iter().all(|v| {
        f.dfg
            .value_def(v)
            .inst()
            .is_some_and(|load| tags.fresh_before(f, load, proof.tag_store, verification))
    }) && terminal.admits_copy(f, proof.bits_store, proof.dest, proof.source, verification)
}

impl Proof {
    pub(super) fn source(self) -> u32 {
        self.source
    }

    pub(super) fn matches(self, dest: u32, tag: IrValue, bits: IrValue) -> bool {
        self.dest == dest && self.tag == tag && self.bits == bits
    }
}
