use super::*;

fn snapshot(program: &[u8]) -> Snapshot {
    crate::Lua::empty().enter(|ctx| {
        let proto = crate::FunctionPrototype::compile(ctx, "transport", program).unwrap();
        Snapshot::new(&proto, 4096, 2 * 1024 * 1024).unwrap()
    })
}

fn cases() -> impl Iterator<Item = (&'static [u8], bool)> {
    [
        (&b"local s=0 for i=1,100 do s=s+i end return s"[..], true),
        (
            &b"local s=0.0 for i=100,1,-1 do s=s+0.5 end return s"[..],
            true,
        ),
        (&b"local s=0 for i=1,100 do s=s+0.5 end return s"[..], true),
        (
            &b"sink(0) local s=0 for i=1,100 do s=s+i end return s"[..],
            true,
        ),
        (
            &b"local s=0 for i=1,100 do s=s+i end local t={} return s,t"[..],
            true,
        ),
        (
            &b"local s=0.0 for i=1.0,100.0 do s=s+i end return s"[..],
            false,
        ),
        (
            &b"local s=0 for i=1,100 do s=1 s=s+0.5 end return s"[..],
            false,
        ),
        (
            &b"local s,x=... for i=1,100 do s=s+x end return s"[..],
            false,
        ),
        (&b"local f=... for i=1,100 do f(i) end"[..], false),
        (&b"local t={} for i=1,100 do t[i]=i end return t"[..], false),
        (&b"return 1"[..], false),
    ]
    .into_iter()
}

#[test]
fn transport_recognition_preserves_source_and_charged_storage() {
    for (program, expected) in cases() {
        let source = snapshot(program);
        let before = format!("{:?}", source.operations);
        let ledger = &source.operations.allocator().0;
        let (current, peak) = (ledger.current(), ledger.peak());
        assert_eq!(integer_loop::has_source_plan(&source).unwrap(), expected);
        assert_eq!((ledger.current(), ledger.peak()), (current, peak));
        assert_eq!(format!("{:?}", source.operations), before);
    }
    let source = snapshot(b"local a,b,c,d,e,f,g,h,i,j,k,l,m,n,o,p=... local s=0 for x=1,100 do s=s+x end return s,a,b,c,d,e,f,g,h,i,j,k,l,m,n,o,p");
    assert!(source.registers > 16);
    assert!(!integer_loop::has_source_plan(&source).unwrap());
}

#[test]
fn transport_recognition_refuses_invalid_snapshots_before_planning() {
    for fault in 0..6 {
        let mut source = snapshot(b"local s=0 for i=1,100 do s=s+i end return s");
        match fault {
            0 => source.registers = 257,
            1 => source.registers = 0,
            2 => source.constants[0].tag = 255,
            3 => {
                source.operations[0] = Operation::LoadConstant {
                    dest: crate::types::RegisterIndex(0),
                    constant: crate::types::ConstantIndex16(u16::MAX),
                }
            }
            4 => {
                source.operations[0] = Operation::NumericForLoop {
                    base: crate::types::RegisterIndex(255),
                    jump: -1,
                }
            }
            _ => {
                source.operations[0] = Operation::NumericForLoop {
                    base: crate::types::RegisterIndex(0),
                    jump: i16::MAX,
                }
            }
        }
        assert!(matches!(
            integer_loop::has_source_plan(&source),
            Err(JitError::Compilation(_))
        ));
    }
}

#[cfg(not(miri))]
#[test]
fn native_transport_selection_keeps_both_verified_entry_families() {
    for (program, ordinary) in cases() {
        let source = snapshot(program);
        let ordinary = ordinary || super::super::array_window::has_source_plan(&source).unwrap();
        let memory = MappingCounter::new(super::super::resources::Ledger::new(usize::MAX));
        let metadata = BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024));
        let code = compile_in(
            &source,
            memory.clone(),
            1024 * 1024,
            metadata.clone(),
            super::super::work::Limits::from(&super::super::JitConfig::default()),
            Failure::PreferBoundPayload,
        )
        .unwrap();
        assert_eq!(
            code.entry_kind,
            if ordinary {
                EntryKind::Ordinary
            } else {
                EntryKind::FrameBound
            }
        );
        assert!(memory.load(Ordering::Relaxed) > 0);
        drop(code);
        assert_eq!(memory.load(Ordering::Relaxed), 0);
        assert_eq!(metadata.0.current(), 0);
    }
}

#[cfg(not(miri))]
#[test]
fn array_transport_keeps_native_fallback_when_kernel_mapping_is_refused() {
    let source = snapshot(b"local seed=40 local t={} for i=1,100 do t[i]=i end return seed");
    assert!(super::super::array_window::has_source_plan(&source).unwrap());
    let memory = MappingCounter::new(super::super::resources::Ledger::new(usize::MAX));
    let metadata = BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024));
    let limits = super::super::work::Limits::from(&super::super::JitConfig::default());
    let ordinary = compile_in(
        &source,
        memory.clone(),
        1024 * 1024,
        metadata.clone(),
        limits,
        Failure::None,
    )
    .unwrap();
    let bytes = memory.load(Ordering::Relaxed);
    drop(ordinary);
    assert_eq!(memory.load(Ordering::Relaxed), 0);
    let candidate = array::compile_pair(
        &source,
        memory.clone(),
        bytes,
        metadata.clone(),
        limits,
        Failure::PreferBoundPayload,
    )
    .unwrap();
    assert_eq!(candidate.entry_kind, EntryKind::Ordinary);
    assert!(candidate.array_kernels.is_none());
    assert_eq!(memory.load(Ordering::Relaxed), bytes);
    let mut slots = vec![Slot::from_value(crate::Value::Nil); source.registers];
    assert_eq!(candidate.invoke(&mut slots, 0, 1).instructions, 1);
    assert!(slots
        .iter()
        .any(|slot| slot.tag == abi::INTEGER && slot.bits == 40));
    drop(candidate);
    assert_eq!(memory.load(Ordering::Relaxed), 0);
    assert_eq!(metadata.0.current(), 0);
}

#[cfg(not(miri))]
#[test]
fn native_transport_selection_preserves_existing_specialized_modes() {
    for program in [&b"return 1"[..], &b"return _ENV.key"[..]] {
        for (projected, scoped_helpers, rooted_moves) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let source = snapshot(program);
            let compile = |failure| {
                let memory = MappingCounter::new(super::super::resources::Ledger::new(usize::MAX));
                let metadata =
                    BudgetAllocator(super::super::resources::Ledger::new(2 * 1024 * 1024));
                let result = compile_selected_rooted(
                    &source,
                    memory.clone(),
                    1024 * 1024,
                    metadata.clone(),
                    super::super::work::Limits::from(&super::super::JitConfig::default()),
                    Selection {
                        #[cfg(all(test, not(miri)))]
                        readonly_scope: false,
                        projected,
                        scoped_helpers,
                        leaf: false,
                        cell_kernel: false,
                        integer_activation: false,
                        failure,
                    },
                    rooted_moves,
                );
                let result = result
                    .map(|code| code.entry_kind)
                    .map_err(|error| format!("{error:?}"));
                assert_eq!(memory.load(Ordering::Relaxed), 0);
                assert_eq!(metadata.0.current(), 0);
                result
            };
            let expected = compile(Failure::None);
            if program == b"return 1" {
                assert_eq!(expected, Ok(EntryKind::Ordinary));
            }
            assert_eq!(
                compile(Failure::PreferBoundPayload),
                expected,
                "projected={projected}, scoped={scoped_helpers}, rooted={rooted_moves}"
            );
        }
    }
}
