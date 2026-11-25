use super::*;

fn masked(mask: i64, expected: i64, reverse: bool) -> Fixture {
    let mut fixture = fixture(expected as u64, false);
    let f = &mut fixture.function;
    let InstructionData::Brif { arg, .. } = f.dfg.insts[fixture.guard] else {
        unreachable!()
    };
    let compare = definition(f, arg).unwrap();
    let InstructionData::IntCompare { args, .. } = f.dfg.insts[compare] else {
        unreachable!()
    };
    let mut c = FuncCursor::new(f);
    c.goto_inst(compare);
    let mask = c.ins().iconst(types::I64, mask);
    let bits = if reverse {
        c.ins().band(mask, args[0])
    } else {
        c.ins().band(args[0], mask)
    };
    if reverse {
        c.func.replace(compare).icmp(IntCC::Equal, args[1], bits);
    } else {
        c.func.replace(compare).icmp(IntCC::Equal, bits, args[1]);
    }
    fixture
}

fn check(f: &Function, fixture: &Fixture, expected: bool) {
    cranelift_codegen::verify_function(f, &settings::Flags::new(settings::builder())).unwrap();
    let quota = allocator();
    let mut analysis = Analysis::with_cycles(f, quota.clone()).unwrap();
    assert_eq!(
        analysis.admit_numeric(f, fixture.slots, fixture.store, 0),
        expected
    );
    assert_eq!(
        analysis.verify_numeric(f, fixture.slots, fixture.store, 0),
        expected
    );
    assert!(analysis
        .admit(f, fixture.slots, fixture.store, 0, fixture.tag)
        .is_none());
    assert!(!analysis.verify(
        f,
        fixture.slots,
        Proof {
            store: fixture.store,
            index: 0,
            tag: fixture.tag
        }
    ));
    analysis.remaining = 0;
    assert!(!analysis.admit_numeric(f, fixture.slots, fixture.store, 0));
    analysis.verification_remaining = 0;
    assert!(!analysis.verify_numeric(f, fixture.slots, fixture.store, 0));
    drop(analysis);
    assert_eq!(quota.0.current(), 0);
}

#[test]
fn numeric_union_masks_never_authorize_exact_tag_writes() {
    for mask in [-4, -3, -2, -1, 0, 1, 2, 3, 4] {
        for expected in 0..=4 {
            for reverse in [false, true] {
                let fixture = masked(mask, expected, reverse);
                check(&fixture.function, &fixture, mask == -2 && expected == 2);
            }
        }
    }
}

#[test]
fn numeric_union_guards_reject_stale_loads_helpers_bypasses_and_other_slots() {
    let fixture = masked(-2, 2, false);
    for fault in 0..10 {
        let mut f = fixture.function.clone();
        match fault {
            0 | 1 => {
                let mut c = FuncCursor::new(&mut f);
                c.goto_inst(if fault == 0 {
                    fixture.guard
                } else {
                    fixture.store
                });
                c.ins()
                    .store(MemFlagsData::new(), fixture.tag, fixture.slots, 0);
            }
            2 | 3 => {
                let convention = f.signature.call_conv;
                let signature = f.import_signature(Signature::new(convention));
                let mut c = FuncCursor::new(&mut f);
                c.goto_inst(if fault == 2 {
                    fixture.guard
                } else {
                    fixture.store
                });
                c.ins().call_indirect(signature, fixture.host, &[]);
            }
            4 => {
                f.replace(fixture.guard).jump(fixture.yes, &[]);
            }
            5 => {
                let end = f.layout.last_inst(fixture.no).unwrap();
                f.replace(end).jump(fixture.yes, &[]);
            }
            6 => {
                let InstructionData::Brif { blocks, .. } = &mut f.dfg.insts[fixture.guard] else {
                    unreachable!()
                };
                blocks.swap(0, 1);
            }
            7 | 8 => {
                let entry = f.layout.entry_block().unwrap();
                let load = f
                    .layout
                    .block_insts(entry)
                    .find(|&i| f.dfg.insts[i].opcode() == Opcode::Load)
                    .unwrap();
                let base = if fault == 7 {
                    fixture.host
                } else {
                    fixture.slots
                };
                f.replace(load).load(
                    types::I64,
                    MemFlagsData::new(),
                    base,
                    if fault == 7 { 0 } else { 16 },
                );
            }
            _ => {
                let entry = f.layout.entry_block().unwrap();
                let mask = f
                    .layout
                    .block_insts(entry)
                    .find(|&i| f.dfg.insts[i].opcode() == Opcode::Band)
                    .unwrap();
                let InstructionData::Binary { args, .. } = f.dfg.insts[mask] else {
                    unreachable!()
                };
                f.replace(mask).bxor(args[0], args[1]);
            }
        }
        check(&f, &fixture, false);
    }
}

#[test]
fn numeric_union_proves_mixed_incoming_tags_without_an_exact_write_tag() {
    let mut fixture = fixture(abi::INTEGER, false);
    let f = &mut fixture.function;
    let no_return = f.layout.last_inst(fixture.no).unwrap();
    let rejected = f.dfg.make_block();
    f.layout.append_block(rejected);
    let mut c = FuncCursor::new(f);
    c.goto_inst(no_return);
    let actual = c
        .ins()
        .load(types::I64, MemFlagsData::new(), fixture.slots, 0);
    let number = c.ins().iconst(types::I64, abi::NUMBER as i64);
    let accepted = c.ins().icmp(IntCC::Equal, actual, number);
    c.func
        .replace(no_return)
        .brif(accepted, fixture.yes, &[], rejected, &[]);
    c.goto_bottom(rejected);
    let zero = c.ins().iconst(types::I64, 0);
    c.ins().return_(&[zero]);
    check(&fixture.function, &fixture, true);
    let mut changed = fixture.function.clone();
    let number = definition(&changed, number).unwrap();
    changed
        .replace(number)
        .iconst(types::I64, abi::REFERENCE as i64);
    check(&changed, &fixture, false);
}
