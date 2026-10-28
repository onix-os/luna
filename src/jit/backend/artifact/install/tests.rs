use super::*;
use crate::jit::{resources::Ledger, work};

fn allocator() -> BudgetAllocator {
    BudgetAllocator(Ledger::new(32 << 20))
}

fn image() -> ModuleImage {
    let source = crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(
            ctx,
            None,
            &b"local t={} for i=1,20 do t[i]=i end local u={} for i=1,20 do u[i]=i+1 end return u"
                [..],
        )
        .unwrap();
        Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap()
    });
    let bundle = emission::emit(
        &source,
        Route::Arrays,
        false,
        work::Limits::from(&crate::JitConfig::default()),
        allocator(),
        allocator(),
    )
    .unwrap();
    let image = bundle
        .modules
        .into_iter()
        .find(|image| image.kind == Kind::Arrays)
        .unwrap();
    assert!(image.functions.len() >= 2);
    image
}

fn load(
    image: &ModuleImage,
    metadata: BudgetAllocator,
    workspace: BudgetAllocator,
    total: MappingCounter,
    failure: Failure,
) -> Result<Loaded, JitError> {
    load_in(
        image,
        total,
        4 << 20,
        metadata,
        workspace,
        failure,
        BranchProtection::None,
    )
}

fn executable(pointer: *const u8) {
    let address = pointer.expose_provenance();
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    let permissions = maps
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let (start, end) = fields.next()?.split_once('-')?;
            let start = usize::from_str_radix(start, 16).ok()?;
            let end = usize::from_str_radix(end, 16).ok()?;
            (start <= address && address < end).then(|| fields.next().unwrap())
        })
        .unwrap();
    assert!(permissions.starts_with("r-x"), "{permissions}");
}

#[test]
fn bounded_loader_preserves_mapping_layout_wx_and_owned_storage() {
    let image = image();
    let metadata = allocator();
    let workspace = allocator();
    let total = MappingCounter::new(metadata.0.clone());
    let loaded = load(
        &image,
        metadata.clone(),
        workspace.clone(),
        total.clone(),
        Failure::None,
    )
    .unwrap();
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let mut requested = 0;
    let mut mapped = 0;
    for (function, &entry) in image.functions.iter().zip(&loaded.entries) {
        let layout = link::Layout::new(
            function.bytes.len(),
            &function.relocations,
            link::Architecture::native(),
        )
        .unwrap();
        requested += layout.bytes;
        mapped += Request::new(layout.bytes, u64::from(function.alignment), page)
            .unwrap()
            .bytes;
        assert_eq!(entry.expose_provenance() % function.alignment as usize, 0);
        executable(entry);
        assert!(function.relocations.is_empty());
        // The finalized bytes remain readable while loaded owns every mapping.
        assert_eq!(
            unsafe { std::slice::from_raw_parts(entry, function.bytes.len()) },
            &*function.bytes
        );
    }
    assert_eq!(total.load(Ordering::Relaxed), mapped);
    assert_eq!(total.requested(), requested);
    assert_eq!(
        workspace.0.current(),
        loaded.entries.capacity() * std::mem::size_of::<*const u8>()
    );
    drop(loaded);
    assert_eq!((total.load(Ordering::Relaxed), total.requested()), (0, 0));
    assert_eq!((metadata.0.current(), workspace.0.current()), (0, 0));
}

#[test]
fn bounded_loader_reclaims_partial_protection_and_every_allocator_refusal() {
    let image = image();
    for failure in [
        Failure::Allocate,
        Failure::Protect,
        Failure::ProtectAfterFirst,
    ] {
        let metadata = allocator();
        let workspace = allocator();
        let total = MappingCounter::new(metadata.0.clone());
        assert!(load(
            &image,
            metadata.clone(),
            workspace.clone(),
            total.clone(),
            failure
        )
        .is_err());
        assert_eq!((total.load(Ordering::Relaxed), total.requested()), (0, 0));
        assert_eq!((metadata.0.current(), workspace.0.current()), (0, 0));
    }
    for refusing_metadata in [false, true] {
        let (mut failures, mut successes) = (0, 0);
        for allowance in 0..40 {
            let metadata = allocator();
            let workspace = allocator();
            let denied = if refusing_metadata {
                &metadata
            } else {
                &workspace
            };
            denied.0.fail_after(allowance);
            let total = MappingCounter::new(metadata.0.clone());
            match load(
                &image,
                metadata.clone(),
                workspace.clone(),
                total.clone(),
                Failure::None,
            ) {
                Ok(loaded) => {
                    successes += 1;
                    drop(loaded);
                }
                Err(JitError::ResourceLimit(_)) => {
                    failures += 1;
                    assert!(BudgetVec::<u8, _>::new_in(denied.clone())
                        .try_reserve_exact(1)
                        .is_err());
                }
                Err(error) => panic!("unexpected loader refusal: {error:?}"),
            }
            assert_eq!((total.load(Ordering::Relaxed), total.requested()), (0, 0));
            assert_eq!((metadata.0.current(), workspace.0.current()), (0, 0));
        }
        assert!(failures > 0 && successes > 0);
    }
}

#[test]
fn symbolic_targets_stay_closed_and_invalid_binding_reclaims_all_maps() {
    let mut image = image();
    let metadata = allocator();
    let workspace = allocator();
    let total = MappingCounter::new(metadata.0.clone());
    let loaded = load(
        &image,
        metadata.clone(),
        workspace.clone(),
        total.clone(),
        Failure::None,
    )
    .unwrap();
    let mut relocation = Relocation {
        offset: 0,
        kind: 1,
        target_kind: 1,
        index: 0,
        target_offset: 4,
        addend: 0,
    };
    assert_eq!(
        target(&image, &loaded.entries, &relocation),
        Some(loaded.entries[0].expose_provenance() as u64 + 4)
    );
    relocation.target_offset = image.functions[0].bytes.len() as u32;
    assert!(target(&image, &loaded.entries, &relocation).is_none());
    relocation.target_offset = 0;
    relocation.target_kind = 2;
    assert!(target(&image, &loaded.entries, &relocation).is_none());
    relocation.target_kind = 0;
    relocation.index = 1;
    for symbol_id in 1..=12 {
        image.declarations[1].0[0] = symbol_id;
        assert_eq!(
            target(&image, &loaded.entries, &relocation),
            Some(symbol(symbol_id).1.expose_provenance() as u64)
        );
    }
    image.declarations[1].0[0] = 13;
    assert!(target(&image, &loaded.entries, &relocation).is_none());
    drop(loaded);
    image.declarations[1].0[0] = 0;
    relocation.index = u32::MAX;
    image.functions[0].relocations.try_reserve_exact(1).unwrap();
    image.functions[0].relocations.push(relocation);
    assert!(load(
        &image,
        metadata.clone(),
        workspace.clone(),
        total.clone(),
        Failure::None
    )
    .is_err());
    assert_eq!((total.load(Ordering::Relaxed), total.requested()), (0, 0));
    assert_eq!((metadata.0.current(), workspace.0.current()), (0, 0));
}
