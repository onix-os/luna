use std::fmt;

use super::{global_box, resources::BudgetAllocator};

pub(super) const MAX_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Error {
    Limit,
    Formatting,
    Allocation,
}

pub(crate) struct Message(Body);

enum Body {
    Static(&'static str),
    Owned {
        value: String,
        charge: global_box::Charge,
    },
}

struct Writer {
    bytes: [u8; MAX_BYTES],
    length: usize,
    maximum: usize,
    error: Option<Error>,
}

impl fmt::Write for Writer {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.error.is_some() {
            return Err(fmt::Error);
        }
        let Some(end) = self
            .length
            .checked_add(text.len())
            .filter(|&n| n <= self.maximum)
        else {
            self.error = Some(Error::Limit);
            return Err(fmt::Error);
        };
        self.bytes[self.length..end].copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}

impl Message {
    pub(super) fn format(
        arguments: fmt::Arguments<'_>,
        maximum: usize,
        allocator: BudgetAllocator,
    ) -> Result<Self, Error> {
        if maximum > MAX_BYTES {
            return Err(Error::Limit);
        }
        let mut writer = Writer {
            bytes: [0; MAX_BYTES],
            length: 0,
            maximum,
            error: None,
        };
        let formatted = fmt::write(&mut writer, arguments);
        if let Some(error) = writer.error {
            return Err(error);
        }
        formatted.map_err(|_| Error::Formatting)?;
        let text =
            std::str::from_utf8(&writer.bytes[..writer.length]).expect("diagnostic UTF-8 chunks");
        let (value, charge) =
            global_box::try_string(text, maximum, allocator).map_err(|_| Error::Allocation)?;
        Ok(Self(Body::Owned { value, charge }))
    }

    #[cfg(any(test, feature = "jit-worker-bootstrap"))]
    pub(super) fn from_owned(value: String, charge: global_box::Charge) -> Result<Self, Error> {
        if value.capacity() > MAX_BYTES {
            drop(value);
            drop(charge);
            return Err(Error::Limit);
        }
        Ok(Self(Body::Owned { value, charge }))
    }

    pub(super) fn as_str(&self) -> &str {
        match &self.0 {
            Body::Static(text) => text,
            Body::Owned { value, .. } => value,
        }
    }

    /// Transfers the text to caller ownership at the public error boundary.
    pub(super) fn into_host(self, allocator: BudgetAllocator) -> Result<String, Error> {
        match self.0 {
            Body::Static(text) => {
                if text.len() > MAX_BYTES {
                    return Err(Error::Limit);
                }
                let (value, charge) = global_box::try_string(text, MAX_BYTES, allocator)
                    .map_err(|_| Error::Allocation)?;
                drop(charge);
                Ok(value)
            }
            Body::Owned { value, charge } => {
                drop(charge);
                Ok(value)
            }
        }
    }
}

impl From<&'static str> for Message {
    fn from(text: &'static str) -> Self {
        Self(Body::Static(text))
    }
}

impl fmt::Display for Message {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Debug for Message {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), formatter)
    }
}

impl PartialEq<str> for Message {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Message {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[cfg(test)]
mod tests;
