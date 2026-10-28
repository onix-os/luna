use super::*;

fn relocation(kind: u8, offset: u32) -> Relocation {
    Relocation {
        offset,
        kind,
        target_kind: 0,
        index: 0,
        target_offset: 0,
        addend: if matches!(kind, 2..=4 | 6) { -4 } else { 0 },
    }
}

fn patch(
    kind: u8,
    architecture: Architecture,
    base: u64,
    target: u64,
    code: [u8; 32],
) -> (Vec<u8>, Layout) {
    let relocation = relocation(kind, 4);
    let layout = Layout::new(code.len(), &[relocation], architecture).unwrap();
    let mut bytes = vec![0; layout.bytes];
    bytes[..code.len()].copy_from_slice(&code);
    Patcher::new(&mut bytes, layout, architecture, base)
        .unwrap()
        .apply(&relocation, target)
        .unwrap();
    (bytes, layout)
}

fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn wide(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

#[test]
fn layout_bounds_all_six_kinds_and_rejects_overflow_and_cross_arch_forms() {
    for architecture in [Architecture::X64, Architecture::A64] {
        for kind in 0..=7 {
            let relocation = relocation(kind, 4);
            let accepted = kind == 1
                || match architecture {
                    Architecture::X64 => matches!(kind, 2..=4 | 6),
                    Architecture::A64 => kind == 5,
                };
            let layout = Layout::new(32, &[relocation], architecture);
            assert_eq!(layout.is_ok(), accepted);
            if let Ok(layout) = layout {
                assert_eq!(
                    layout.bytes,
                    match kind {
                        2 | 3 | 5 => 48,
                        4 => 40,
                        _ => 32,
                    }
                );
            }
        }
        assert!(Layout::new(0, &[], architecture).is_err());
        assert!(Layout::new(usize::MAX, &[], architecture).is_err());
        assert!(Layout::new(8, &[relocation(1, 1)], architecture).is_err());
    }
    let relocations = [
        relocation(2, 0),
        relocation(3, 4),
        relocation(4, 8),
        relocation(4, 12),
    ];
    let layout = Layout::new(33, &relocations, Architecture::X64).unwrap();
    assert_eq!((layout.veneers, layout.got, layout.bytes), (2, 72, 88));
    let mut bytes = [0; 88];
    assert!(Patcher::new(&mut bytes[..87], layout, Architecture::X64, 0).is_err());
    assert!(Patcher::new(&mut bytes, layout, Architecture::X64, u64::MAX - 80).is_err());
    let base = 1u64 << 40;
    let target = 1u64 << 56;
    Patcher::new(&mut bytes, layout, Architecture::X64, base)
        .map(|mut patcher| {
            for relocation in &relocations {
                patcher.apply(relocation, target).unwrap();
            }
            assert!(patcher.apply(&relocations[0], target).is_err());
            assert!(patcher.apply(&relocations[2], target).is_err());
        })
        .unwrap();
    assert_eq!(
        (
            word(&bytes, 0),
            word(&bytes, 4),
            word(&bytes, 8),
            word(&bytes, 12)
        ),
        (29, 41, 60, 64)
    );
    assert_eq!(
        (
            wide(&bytes, 39),
            wide(&bytes, 55),
            wide(&bytes, 72),
            wide(&bytes, 80)
        ),
        (target, target, target, target)
    );
    assert_eq!(&bytes[65..72], &[0; 7]);
}

#[test]
fn absolute_and_relative_fields_preserve_exact_endpoints_and_addends() {
    for target in [0, 1, 1 << 63, u64::MAX] {
        let (bytes, _) = patch(1, Architecture::X64, 0x1000, target, [0; 32]);
        assert_eq!(wide(&bytes, 4), target);
    }
    let base = 1u64 << 40;
    for delta in [i32::MIN, -1, 0, 1, i32::MAX] {
        let target = base.checked_add_signed(i64::from(delta) + 8).unwrap();
        let (bytes, _) = patch(6, Architecture::X64, base, target, [0; 32]);
        assert_eq!(word(&bytes, 4) as i32, delta);
    }
    for (base, target, addend) in [(0, 0, -1), (0, u64::MAX, 1)] {
        let mut relocation = relocation(1, 4);
        relocation.addend = addend;
        let layout = Layout::new(32, &[relocation], Architecture::X64).unwrap();
        let mut bytes = [0; 32];
        assert!(Patcher::new(&mut bytes, layout, Architecture::X64, base)
            .unwrap()
            .apply(&relocation, target)
            .is_err());
    }
    for delta in [i64::from(i32::MIN) - 1, i64::from(i32::MAX) + 1] {
        assert!(displacement(base.checked_add_signed(delta).unwrap(), base).is_err());
    }
}

#[test]
fn x64_calls_use_direct_displacements_or_checked_absolute_veneers() {
    let base = 1u64 << 40;
    for kind in [2, 3] {
        for delta in [
            i64::from(i32::MIN) - 1,
            i64::from(i32::MIN),
            -1,
            0,
            1,
            i64::from(i32::MAX),
            i64::from(i32::MAX) + 1,
        ] {
            let target = base.checked_add_signed(delta + 8).unwrap();
            let (bytes, _) = patch(kind, Architecture::X64, base, target, [0; 32]);
            if let Ok(direct) = i32::try_from(delta) {
                assert_eq!(word(&bytes, 4) as i32, direct);
                assert_eq!(&bytes[32..], &[0; 16]);
            } else {
                assert_eq!(word(&bytes, 4), 24);
                assert_eq!(&bytes[32..38], &[0xff, 0x25, 0, 0, 0, 0]);
                assert_eq!(wide(&bytes, 38), target);
            }
        }
    }
}

#[test]
fn x64_got_preserves_near_relaxation_and_far_or_nonmov_indirection() {
    let base = 1u64 << 40;
    for prefix in [0x48, 0x49, 0x4f] {
        for modrm in [0x05, 0x0d, 0x35, 0x3d] {
            let mut code = [0; 32];
            code[1..4].copy_from_slice(&[prefix, 0x8b, modrm]);
            for target in [base + 100, base + (1u64 << 32)] {
                let (bytes, layout) = patch(4, Architecture::X64, base, target, code);
                if target == base + 100 {
                    assert_eq!(bytes[2], 0x8d);
                    assert_eq!(word(&bytes, 4), 92);
                    assert_eq!(wide(&bytes, layout.got), 0);
                } else {
                    assert_eq!(bytes[2], 0x8b);
                    assert_eq!(word(&bytes, 4), 24);
                    assert_eq!(wide(&bytes, layout.got), target);
                }
            }
        }
    }
    for code in [[0; 32], {
        let mut code = [0; 32];
        code[1..4].copy_from_slice(&[0x48, 0x8b, 0xc5]);
        code
    }] {
        let (bytes, layout) = patch(4, Architecture::X64, base, base + 100, code);
        assert_eq!(word(&bytes, 4), 24);
        assert_eq!(wide(&bytes, layout.got), base + 100);
    }
}

#[test]
fn a64_calls_cover_signed_branch_limits_and_preserve_opcode_through_veneers() {
    let base = 1u64 << 40;
    for opcode in [0x1400_0000u32, 0x9400_0000] {
        for delta in [
            -(1i64 << 27) - 4,
            -(1i64 << 27),
            -4,
            0,
            4,
            (1i64 << 27) - 4,
            1i64 << 27,
        ] {
            let mut code = [0; 32];
            code[4..8].copy_from_slice(&(opcode | 0x03ff_ffff).to_le_bytes());
            let target = base.checked_add_signed(delta + 4).unwrap();
            let (bytes, _) = patch(5, Architecture::A64, base, target, code);
            if let Ok(immediate) = branch(target, base + 4) {
                assert_eq!(word(&bytes, 4), opcode | immediate);
            } else {
                assert_eq!(word(&bytes, 4), opcode | 7);
                assert_eq!(word(&bytes, 32), 0x5800_0050);
                assert_eq!(word(&bytes, 36), 0xd61f_0200);
                assert_eq!(wide(&bytes, 40), target);
            }
        }
    }
    assert!(branch(base + 1, base).is_err());
    assert!(branch(base + (1 << 27), base).is_err());
    assert!(branch(base - (1 << 27) - 4, base).is_err());
}

#[test]
fn patch_refusals_never_write_outside_the_bounded_slice() {
    let mut guard = [0xa5; 160];
    for architecture in [Architecture::X64, Architecture::A64] {
        let kind = match architecture {
            Architecture::X64 => 2,
            Architecture::A64 => 5,
        };
        let valid = relocation(kind, 4);
        let layout = Layout::new(32, &[valid], architecture).unwrap();
        for offset in 29..=40 {
            let mut invalid = valid;
            invalid.offset = offset;
            let slice = &mut guard[16..16 + layout.bytes];
            assert!(Patcher::new(slice, layout, architecture, 0x1000)
                .unwrap()
                .apply(&invalid, 0x2000)
                .is_err());
            assert_eq!(&guard[..16], &[0xa5; 16]);
            assert_eq!(
                &guard[16 + layout.bytes..],
                &[0xa5; 160][16 + layout.bytes..]
            );
        }
    }
    let valid = relocation(5, 4);
    let layout = Layout::new(32, &[valid], Architecture::A64).unwrap();
    for base in [0x1000, 0x1001] {
        let mut bytes = [0; 48];
        assert!(Patcher::new(&mut bytes, layout, Architecture::A64, base)
            .unwrap()
            .apply(&valid, 0x2000)
            .is_err());
    }
    for (base, target) in [(0x1000, 0x2001), (0x1001, 0x2000)] {
        let mut bytes = [0; 48];
        bytes[4..8].copy_from_slice(&0x9400_0000u32.to_le_bytes());
        assert!(Patcher::new(&mut bytes, layout, Architecture::A64, base)
            .unwrap()
            .apply(&valid, target)
            .is_err());
        assert_eq!(word(&bytes, 4), 0x9400_0000);
    }
}

#[test]
fn bti_uses_only_canonical_complete_environment_fields() {
    use cranelift_jit::BranchProtection;
    assert!(matches!(
        protection(b"artifact=1;use_bti=1;end", Architecture::A64),
        Ok(BranchProtection::BTI)
    ));
    assert!(matches!(
        protection(b"artifact=1;use_bti=0;end", Architecture::A64),
        Ok(BranchProtection::None)
    ));
    assert!(matches!(
        protection(b"artifact=1;has_avx=1", Architecture::X64),
        Ok(BranchProtection::None)
    ));
    for token in [
        &b""[..],
        b"not_use_bti=1",
        b"use_bti=true",
        b"use_bti=2",
        b"use_bti=1;use_bti=0",
        b"use_bti=0;use_bti=0",
    ] {
        assert!(protection(token, Architecture::A64).is_err());
    }
    assert!(protection(b"use_bti=0", Architecture::X64).is_err());
}
