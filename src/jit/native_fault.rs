use std::io;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Operation {
    Map,
    Cache,
    Protect,
    Bti,
    Pipeline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct System {
    code: Option<i32>,
    kind: io::ErrorKind,
}

impl System {
    fn capture(error: io::Error) -> Self {
        Self {
            code: error.raw_os_error(),
            kind: error.kind(),
        }
    }

    #[cfg(any(test, miri))]
    pub fn kind(kind: io::ErrorKind) -> Self {
        Self { code: None, kind }
    }

    fn into_io(self) -> io::Error {
        self.code
            .map_or_else(|| self.kind.into(), io::Error::from_raw_os_error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fault {
    AlignmentOverflow,
    InvalidAlignment,
    SizeOverflow,
    AlignmentOutsideMapping,
    SegmentUnavailable,
    ImageLimit,
    MetadataLimit,
    MappingLimit,
    HostLimit,
    #[cfg(test)]
    AllocationDenied,
    #[cfg(test)]
    ProtectionDenied,
    #[cfg(test)]
    PartialProtectionDenied,
    System(Operation, System),
}

impl Fault {
    pub fn capture(operation: Operation, error: io::Error) -> Self {
        Self::System(operation, System::capture(error))
    }

    pub fn into_io(self) -> io::Error {
        #[cfg(test)]
        assert_adapters_allowed();
        let (kind, message) = match self {
            Self::AlignmentOverflow => (io::ErrorKind::Other, "native alignment overflow"),
            Self::InvalidAlignment => (
                io::ErrorKind::InvalidInput,
                "invalid native alignment or page size",
            ),
            Self::SizeOverflow => (io::ErrorKind::Other, "native allocation size overflow"),
            Self::AlignmentOutsideMapping => {
                (io::ErrorKind::Other, "native alignment outside mapping")
            }
            Self::SegmentUnavailable => (io::ErrorKind::Other, "native segment unavailable"),
            Self::ImageLimit => (
                io::ErrorKind::Other,
                "native image exceeds code cache limit",
            ),
            Self::MetadataLimit => (
                io::ErrorKind::Other,
                "native allocation record quota exhausted",
            ),
            Self::MappingLimit => (io::ErrorKind::Other, "native memory quota exhausted"),
            Self::HostLimit => (io::ErrorKind::Other, "host memory quota exhausted"),
            #[cfg(test)]
            Self::AllocationDenied => (
                io::ErrorKind::PermissionDenied,
                "injected native allocation denial",
            ),
            #[cfg(test)]
            Self::ProtectionDenied => (io::ErrorKind::Other, "injected native protection denial"),
            #[cfg(test)]
            Self::PartialProtectionDenied => {
                (io::ErrorKind::Other, "injected partial protection denial")
            }
            Self::System(Operation::Cache, error) => {
                return io::Error::other(error.into_io().to_string())
            }
            Self::System(_, error) => return error.into_io(),
        };
        io::Error::new(kind, message)
    }

    pub fn into_module(self) -> cranelift_module::ModuleError {
        #[cfg(test)]
        assert_adapters_allowed();
        let error = match self {
            #[cfg(test)]
            Self::ProtectionDenied => anyhow::anyhow!("injected native protection denial"),
            #[cfg(test)]
            Self::PartialProtectionDenied => anyhow::anyhow!("injected partial protection denial"),
            Self::System(Operation::Pipeline, error) => {
                anyhow::anyhow!("native pipeline flush: {}", error.into_io())
            }
            other => anyhow::Error::new(other.into_io()),
        };
        cranelift_module::ModuleError::Backend(error)
    }
}

#[cfg(not(miri))]
pub(super) fn coherence<T>(operation: Operation, result: io::Result<T>) -> Result<T, Fault> {
    result.map_err(|error| Fault::capture(operation, error))
}

#[cfg(miri)]
pub(super) fn coherence<T, E>(operation: Operation, result: Result<T, E>) -> Result<T, Fault> {
    result.map_err(|_| Fault::System(operation, System::kind(io::ErrorKind::Other)))
}

#[cfg(test)]
thread_local! {
    static BOUNDED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(super) fn assert_adapters_allowed() {
    assert!(
        !BOUNDED.get(),
        "bounded native operation entered an allocating error adapter"
    );
}

#[cfg(test)]
pub(super) fn bounded<T>(run: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            BOUNDED.set(self.0);
        }
    }
    let _restore = Restore(BOUNDED.replace(true));
    run()
}
