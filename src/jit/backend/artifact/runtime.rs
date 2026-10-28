use super::*;

#[cfg(test)]
mod tests;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static COMPILES: Cell<u64> = const { Cell::new(0) };
}

pub(in crate::jit) fn with_enabled<T>(enabled: bool, run: impl FnOnce() -> T) -> (T, u64) {
    struct Restore(bool, u64);
    impl Drop for Restore {
        fn drop(&mut self) {
            ENABLED.set(self.0);
            COMPILES.set(self.1);
        }
    }
    let _restore = Restore(ENABLED.replace(enabled), COMPILES.replace(0));
    let result = run();
    (result, COMPILES.get())
}

pub(in crate::jit) fn compile(
    route: Route,
    source: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    work: crate::jit::work::Limits,
    failure: Failure,
) -> Option<Result<Code, JitError>> {
    if !ENABLED.get() {
        return None;
    }
    COMPILES.set(COMPILES.get() + 1);
    Some((|| {
        if !matches!(
            failure,
            Failure::None
                | Failure::RefuseReadonlyOwnerStorage
                | Failure::RefuseReadonlyOwnerAllocation
                | Failure::RefuseReadonlyCacheAllocation
                | Failure::RefuseOwnerStorage
                | Failure::RefuseOwnerAllocation
                | Failure::Allocate
                | Failure::Protect
                | Failure::RequireReleasedSnapshot
        ) {
            return Err(JitError::Unavailable(
                "unsupported artifact compiler injection",
            ));
        }
        crate::jit::work::Expansion::admit(source, work)?;
        let resume = crate::jit::readonly_lookup::binding::integer_resume::enabled();
        let packet = crate::jit::compiler_process::request_packet(source, route, resume, work)?;
        assemble(
            &packet.bytes,
            packet.binding(),
            Limits {
                bytes: 2 << 20,
                modules: 16,
                declarations: 256,
                functions: 128,
                relocations: 8192,
            },
            Assembly {
                source,
                route,
                resume,
                total,
                limit,
                metadata,
                work,
                failure,
            },
        )
        .map_err(|error| match error {
            InstallError::Native(error) => error,
            InstallError::Packet(Error::Allocation | Error::Limit) => {
                JitError::ResourceLimit("compiler artifact")
            }
            InstallError::Packet(_) => {
                JitError::Compilation("compiler artifact source or identity".into())
            }
        })
    })())
}

#[test]
fn worker_artifact_selector_restores_nested_and_panicking_scopes() {
    assert!(!ENABLED.get());
    let (_, calls) = with_enabled(true, || {
        assert!(ENABLED.get());
        with_enabled(false, || assert!(!ENABLED.get()));
        assert!(ENABLED.get());
        assert!(
            std::panic::catch_unwind(|| with_enabled(false, || panic!("worker selector"))).is_err()
        );
        assert!(ENABLED.get());
    });
    assert_eq!(calls, 0);
    assert!(!ENABLED.get());
}
