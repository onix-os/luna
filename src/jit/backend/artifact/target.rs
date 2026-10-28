use std::{cell::Cell, fmt, ops::Deref};

use cranelift_codegen::settings::{self, Configurable};

use super::{link::Architecture, BudgetAllocator, BudgetVec, Error};

#[cfg(not(miri))]
mod native_tests;
mod profile;
#[cfg(test)]
mod tests;

const MAX_BYTES: usize = 32768;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Features {
    architecture: Architecture,
    bits: u32,
}

impl Features {
    fn names(self) -> &'static [&'static str] {
        match self.architecture {
            Architecture::X64 => profile::X64,
            Architecture::A64 => profile::A64,
        }
    }

    fn value(self, index: usize) -> bool {
        self.bits & (1 << index) != 0
    }

    fn parse(architecture: Architecture, text: &str) -> Result<Self, Error> {
        if text.len() > MAX_BYTES {
            return Err(Error::Limit);
        }
        let mut features = Self {
            architecture,
            bits: 0,
        };
        let mut fields = text.split(';');
        for (index, &name) in features.names().iter().enumerate() {
            let (key, value) = fields
                .next()
                .and_then(|field| field.split_once('='))
                .ok_or(Error::Identity)?;
            if key != name {
                return Err(Error::Identity);
            }
            match value {
                "true" => features.bits |= 1 << index,
                "false" => {}
                _ => return Err(Error::Identity),
            }
        }
        if fields.next().is_some() {
            return Err(Error::Identity);
        }
        Ok(features)
    }

    fn native() -> Result<Self, Error> {
        let mut collector = Collector {
            features: Self {
                architecture: Architecture::native(),
                bits: 0,
            },
            invalid: false,
        };
        cranelift_native::infer_native_flags(&mut collector).map_err(|_| Error::Identity)?;
        collector.finish()
    }

    fn current() -> Result<Self, Error> {
        REQUESTED.get().map_or_else(Self::native, Ok)
    }

    fn emit(self, output: &mut dyn fmt::Write, environment: bool) -> fmt::Result {
        if environment {
            let host = &target_lexicon::HOST;
            let convention = cranelift_codegen::isa::CallConv::triple_default(host);
            write!(
                output,
                "cranelift=0.136.1;artifact=1;slot=16;exit=16;{host};{convention};{}{}{};",
                profile::SHARED_HEAD,
                self.architecture == Architecture::X64,
                profile::SHARED_TAIL
            )?;
        }
        for (index, &name) in self.names().iter().enumerate() {
            if index != 0 {
                output.write_char(';')?;
            }
            let value = match (environment, self.value(index)) {
                (true, true) => "1",
                (true, false) => "0",
                (false, true) => "true",
                (false, false) => "false",
            };
            write!(output, "{name}={value}")?;
        }
        Ok(())
    }

    fn encode(
        self,
        environment: bool,
        allocator: BudgetAllocator,
    ) -> Result<BudgetVec<u8, BudgetAllocator>, Error> {
        let mut count = Count(0);
        self.emit(&mut count, environment)
            .map_err(|_| Error::Limit)?;
        let mut bytes = BudgetVec::new_in(allocator);
        bytes
            .try_reserve_exact(count.0)
            .map_err(|_| Error::Allocation)?;
        let mut output = Output {
            bytes,
            length: count.0,
        };
        self.emit(&mut output, environment)
            .map_err(|_| Error::Identity)?;
        if output.bytes.len() != count.0 {
            return Err(Error::Identity);
        }
        Ok(output.bytes)
    }
}

struct Collector {
    features: Features,
    invalid: bool,
}

impl Configurable for Collector {
    fn set(&mut self, name: &str, value: &str) -> settings::SetResult<()> {
        if let Some(index) = self.features.names().iter().position(|&key| key == name) {
            match value {
                "true" => self.features.bits |= 1 << index,
                "false" => self.features.bits &= !(1 << index),
                _ => self.invalid = true,
            }
        } else {
            self.invalid = true;
        }
        Ok(())
    }

    fn enable(&mut self, name: &str) -> settings::SetResult<()> {
        self.set(name, "true")
    }
}

impl Collector {
    fn finish(self) -> Result<Features, Error> {
        if self.invalid {
            Err(Error::Identity)
        } else {
            Ok(self.features)
        }
    }
}

struct Count(usize);

impl fmt::Write for Count {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0 = self
            .0
            .checked_add(text.len())
            .filter(|&len| len <= MAX_BYTES)
            .ok_or(fmt::Error)?;
        Ok(())
    }
}

struct Output {
    bytes: BudgetVec<u8, BudgetAllocator>,
    length: usize,
}

impl fmt::Write for Output {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if !self
            .bytes
            .len()
            .checked_add(text.len())
            .is_some_and(|end| end <= self.length)
        {
            return Err(fmt::Error);
        }
        self.bytes.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

#[derive(Debug)]
pub(in crate::jit) struct FeatureBytes(BudgetVec<u8, BudgetAllocator>);

impl Deref for FeatureBytes {
    type Target = str;
    fn deref(&self) -> &str {
        std::str::from_utf8(&self.0).expect("canonical target encoding")
    }
}

impl fmt::Display for FeatureBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self)
    }
}

impl AsRef<std::ffi::OsStr> for FeatureBytes {
    fn as_ref(&self) -> &std::ffi::OsStr {
        std::ffi::OsStr::new(&**self)
    }
}

thread_local! {
    static REQUESTED: Cell<Option<Features>> = const { Cell::new(None) };
    static IDENTITY: Cell<bool> = const { Cell::new(false) };
}

pub(in crate::jit) fn assert_compiler_allowed() {
    assert!(
        !IDENTITY.get(),
        "bounded target identity attempted compiler construction"
    );
}

fn identity<T>(run: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            IDENTITY.set(self.0);
        }
    }
    let _restore = Restore(IDENTITY.replace(true));
    run()
}

pub(in crate::jit) fn target_features() -> Result<FeatureBytes, Error> {
    target_features_in(BudgetAllocator(crate::jit::resources::Ledger::new(
        MAX_BYTES,
    )))
}

pub(in crate::jit) fn target_features_in(
    allocator: BudgetAllocator,
) -> Result<FeatureBytes, Error> {
    identity(|| {
        Features::current()?
            .encode(false, allocator)
            .map(FeatureBytes)
    })
}

pub(in crate::jit) fn environment_in(
    allocator: BudgetAllocator,
) -> Result<BudgetVec<u8, BudgetAllocator>, Error> {
    identity(|| Features::current()?.encode(true, allocator))
}

fn with_features<T>(features: Features, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<Features>);
    impl Drop for Restore {
        fn drop(&mut self) {
            REQUESTED.set(self.0);
        }
    }
    let _restore = Restore(REQUESTED.replace(Some(features)));
    run()
}

pub(in crate::jit) fn with_target<T>(features: &str, run: impl FnOnce() -> T) -> Result<T, Error> {
    let features = identity(|| Features::parse(Architecture::native(), features))?;
    Ok(with_features(features, run))
}

pub(in crate::jit) fn apply_target(
    builder: &mut cranelift_codegen::isa::Builder,
) -> Result<(), crate::jit::JitError> {
    if let Some(features) = REQUESTED.get() {
        for (index, &name) in features.names().iter().enumerate() {
            builder
                .set(
                    name,
                    if features.value(index) {
                        "true"
                    } else {
                        "false"
                    },
                )
                .map_err(|_| crate::jit::JitError::Unavailable("compiler target feature schema"))?;
        }
    }
    Ok(())
}
