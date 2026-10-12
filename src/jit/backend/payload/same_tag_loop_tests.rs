use super::*;

fn loop_fixture(tag: u64) -> (Fixture, Block) {
    let mut fixture = fixture(tag, false);
    let f = &mut fixture.function;
    let exit = f.dfg.make_block();
    let body = f.dfg.make_block();
    f.layout.append_block(exit);
    f.layout.append_block(body);
    while let Some(inst) = f.layout.first_inst(fixture.yes) {
        f.layout.remove_inst(inst);
        f.layout.append_inst(inst, exit);
    }
    let mut c = FuncCursor::new(f);
    c.goto_bottom(fixture.yes);
    let again = c.ins().icmp_imm_s(IntCC::NotEqual, fixture.host, 0);
    c.ins().brif(again, body, &[], exit, &[]);
    c.goto_bottom(body);
    c.ins().jump(fixture.yes, &[]);
    (fixture, body)
}

#[test]
fn tag_proofs_cross_effect_free_cycles_with_guarded_entries() {
    for tag in [abi::NIL, abi::BOOLEAN, abi::INTEGER, abi::NUMBER] {
        let (fixture, _) = loop_fixture(tag);
        let f = &fixture.function;
        assert!(!admitted(f, &fixture));
        let mut analysis = Analysis::with_cycles(f, allocator()).unwrap();
        let proof = analysis
            .admit(f, fixture.slots, fixture.store, 0, fixture.tag)
            .unwrap();
        assert!(analysis.verify(f, fixture.slots, proof));
        let numeric = matches!(tag, abi::INTEGER | abi::NUMBER);
        assert_eq!(
            analysis.admit_numeric(f, fixture.slots, fixture.store, 0),
            numeric
        );
        assert_eq!(
            analysis.verify_numeric(f, fixture.slots, fixture.store, 0),
            numeric
        );
        analysis.remaining = 0;
        assert!(analysis
            .admit(f, fixture.slots, fixture.store, 0, fixture.tag)
            .is_none());
        analysis.verification_remaining = 0;
        assert!(!analysis.verify(f, fixture.slots, proof));
    }
}

#[test]
fn tag_cycle_proofs_reject_effects_bypasses_and_writes_after_query() {
    for tag in [abi::INTEGER, abi::NUMBER] {
        let (fixture, body) = loop_fixture(tag);
        let original = &fixture.function;
        for effect in 0..7 {
            let mut f = original.clone();
            match effect {
                0 | 1 => {
                    let end = f.layout.last_inst(body).unwrap();
                    let mut c = FuncCursor::new(&mut f);
                    c.goto_inst(end);
                    c.ins()
                        .store(MemFlagsData::new(), fixture.tag, fixture.slots, effect * 8);
                }
                2 => {
                    let end = f.layout.last_inst(body).unwrap();
                    let convention = f.signature.call_conv;
                    let signature = f.import_signature(Signature::new(convention));
                    let mut c = FuncCursor::new(&mut f);
                    c.goto_inst(end);
                    c.ins().call_indirect(signature, fixture.host, &[]);
                }
                3 => {
                    let end = f.layout.last_inst(fixture.no).unwrap();
                    f.replace(end).jump(body, &[]);
                }
                4 => {
                    f.replace(fixture.guard).jump(fixture.yes, &[]);
                }
                5 => {
                    let exit = f.layout.inst_block(fixture.store).unwrap();
                    let end = f.layout.last_inst(exit).unwrap();
                    f.replace(end).jump(fixture.yes, &[]);
                }
                _ => {
                    let InstructionData::Brif { blocks, .. } = &mut f.dfg.insts[fixture.guard]
                    else {
                        unreachable!()
                    };
                    blocks.swap(0, 1);
                }
            }
            assert!(!admitted(&f, &fixture), "tag={tag} effect={effect}");
            let mut analysis = Analysis::with_cycles(&f, allocator()).unwrap();
            assert!(analysis
                .admit(&f, fixture.slots, fixture.store, 0, fixture.tag)
                .is_none());
            assert!(!analysis.admit_numeric(&f, fixture.slots, fixture.store, 0));
            let forged = Proof {
                store: fixture.store,
                index: 0,
                tag: fixture.tag,
            };
            assert!(!analysis.verify(&f, fixture.slots, forged));
        }
    }
}

#[test]
fn tag_cycle_search_retains_depth_bound() {
    let (fixture, body) = loop_fixture(abi::INTEGER);
    let mut f = fixture.function.clone();
    let mut previous = body;
    for _ in 0..32 {
        let block = f.dfg.make_block();
        f.layout.append_block(block);
        let end = f.layout.last_inst(previous).unwrap();
        f.replace(end).jump(block, &[]);
        let mut c = FuncCursor::new(&mut f);
        c.goto_bottom(block);
        c.ins().jump(fixture.yes, &[]);
        previous = block;
    }
    assert!(!admitted(&f, &fixture));
    let mut analysis = Analysis::with_cycles(&f, allocator()).unwrap();
    assert!(analysis
        .admit(&f, fixture.slots, fixture.store, 0, fixture.tag)
        .is_none());
}

#[cfg(not(miri))]
#[test]
fn tag_cycle_proofs_match_forward_effect_reachability() {
    for edges in 0..64 {
        let targets: [usize; 3] = std::array::from_fn(|i| (edges >> (2 * i)) & 3);
        for effects in 0..8 {
            for bypass in [false, true] {
                let mut reachable = [false; 3];
                let mut pending = vec![0];
                if bypass {
                    pending.push(2);
                }
                while let Some(node) = pending.pop() {
                    if reachable[node] {
                        continue;
                    }
                    reachable[node] = true;
                    if targets[node] < 3 {
                        pending.push(targets[node]);
                    }
                }
                let (fixture, body) = loop_fixture(abi::INTEGER);
                let mut f = fixture.function.clone();
                let extra = f.dfg.make_block();
                f.layout.append_block(extra);
                let nodes = [fixture.yes, body, extra];
                let exit = f.layout.inst_block(fixture.store).unwrap();
                for (index, &block) in nodes.iter().enumerate() {
                    while let Some(inst) = f.layout.first_inst(block) {
                        f.layout.remove_inst(inst);
                    }
                    let mut c = FuncCursor::new(&mut f);
                    c.goto_bottom(block);
                    if !reachable[index] {
                        let zero = c.ins().iconst(types::I64, 0);
                        c.ins().return_(&[zero]);
                        continue;
                    }
                    if effects & (1 << index) != 0 {
                        c.ins()
                            .store(MemFlagsData::new(), fixture.tag, fixture.slots, 8);
                    }
                    let target = nodes.get(targets[index]).copied().unwrap_or(exit);
                    let condition = c.ins().icmp_imm_s(IntCC::NotEqual, fixture.host, 0);
                    if edges & 1 == 0 {
                        c.ins().brif(condition, target, &[], exit, &[]);
                    } else {
                        c.ins().brif(condition, exit, &[], target, &[]);
                    }
                }
                if bypass {
                    let end = f.layout.last_inst(fixture.no).unwrap();
                    f.replace(end).jump(extra, &[]);
                }
                cranelift_codegen::verify_function(&f, &settings::Flags::new(settings::builder()))
                    .unwrap();
                let mut seen = [[false; 2]; 3];
                let mut pending = vec![(0, true)];
                if bypass {
                    pending.push((2, false));
                }
                let mut expected = true;
                while let Some((node, valid)) = pending.pop() {
                    if seen[node][usize::from(valid)] {
                        continue;
                    }
                    seen[node][usize::from(valid)] = true;
                    let valid = valid && effects & (1 << node) == 0;
                    expected &= valid;
                    if targets[node] < 3 {
                        pending.push((targets[node], valid));
                    }
                }
                let mut analysis = Analysis::with_cycles(&f, allocator()).unwrap();
                let actual = analysis
                    .admit(&f, fixture.slots, fixture.store, 0, fixture.tag)
                    .is_some();
                assert_eq!(
                    actual, expected,
                    "edges={edges} effects={effects} bypass={bypass}"
                );
                let forged = Proof {
                    store: fixture.store,
                    index: 0,
                    tag: fixture.tag,
                };
                assert_eq!(analysis.verify(&f, fixture.slots, forged), expected);
            }
        }
    }
}
