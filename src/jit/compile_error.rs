use std::fmt;

use super::{
    diagnostic::{self, Message},
    resources::BudgetAllocator,
    JitError,
};

#[cfg(any(test, feature = "jit-worker-bootstrap"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Configuration,
    Unavailable,
    Resource,
    Compilation,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("invalid JIT configuration: {0}")]
    InvalidConfiguration(&'static str),
    #[error("JIT unavailable: {0}")]
    Unavailable(&'static str),
    #[error("JIT resource limit: {0}")]
    ResourceLimit(&'static str),
    #[error("native compilation failed: {0}")]
    Compilation(Message),
    #[error("native compilation failed: {label}{index}")]
    Indexed { label: &'static str, index: usize },
}

impl Error {
    #[cfg(any(test, feature = "jit-worker-bootstrap"))]
    pub(crate) fn class(&self) -> Class {
        match self {
            Self::InvalidConfiguration(_) => Class::Configuration,
            Self::Unavailable(_) => Class::Unavailable,
            Self::ResourceLimit(_) => Class::Resource,
            Self::Compilation(_) | Self::Indexed { .. } => Class::Compilation,
        }
    }

    pub(crate) fn compilation(arguments: fmt::Arguments<'_>, allocator: BudgetAllocator) -> Self {
        match Message::format(arguments, diagnostic::MAX_BYTES, allocator) {
            Ok(message) => Self::Compilation(message),
            Err(error) => Self::diagnostic_failure(error),
        }
    }

    pub(crate) fn indexed(label: &'static str, index: usize) -> Self {
        Self::Indexed { label, index }
    }

    fn diagnostic_failure(error: diagnostic::Error) -> Self {
        match error {
            diagnostic::Error::Limit => Self::ResourceLimit("native diagnostic bytes"),
            diagnostic::Error::Formatting => Self::Unavailable("native diagnostic formatting"),
            diagnostic::Error::Allocation => Self::ResourceLimit("native diagnostic allocation"),
        }
    }

    pub(crate) fn into_public(self, allocator: BudgetAllocator) -> JitError {
        match self {
            Self::InvalidConfiguration(reason) => JitError::InvalidConfiguration(reason),
            Self::Unavailable(reason) => JitError::Unavailable(reason),
            Self::ResourceLimit(reason) => JitError::ResourceLimit(reason),
            Self::Compilation(message) => match message.into_host(allocator) {
                Ok(text) => JitError::Compilation(text),
                Err(error) => Self::diagnostic_failure(error).into_public_unallocated(),
            },
            Self::Indexed { label, index } => {
                Self::compilation(format_args!("{label}{index}"), allocator.clone())
                    .into_public(allocator)
            }
        }
    }

    fn into_public_unallocated(self) -> JitError {
        match self {
            Self::InvalidConfiguration(reason) => JitError::InvalidConfiguration(reason),
            Self::Unavailable(reason) => JitError::Unavailable(reason),
            Self::ResourceLimit(reason) => JitError::ResourceLimit(reason),
            Self::Compilation(_) | Self::Indexed { .. } => {
                unreachable!("diagnostic publication failure class")
            }
        }
    }
}

#[cfg(test)]
mod tests;
