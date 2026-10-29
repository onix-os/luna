use allocator_api2::vec::Vec;
use sha2::{Digest, Sha256};

use super::{resources::BudgetAllocator, CompileError};

#[cfg(test)]
mod publication_tests;
#[cfg(test)]
mod tests;

const MAGIC: &[u8; 8] = b"LUNARES\0";
const VERSION: u32 = 1;
pub(super) const HEADER: usize = 112;
const PAYLOAD: usize = 2 << 20;
pub(super) const MAX_BYTES: usize = HEADER + PAYLOAD;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Class {
    Unavailable = 1,
    Resource = 2,
    Compilation = 3,
    Configuration = 4,
    Rejected = 5,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    Snapshot = 1,
    Target = 2,
    Compiler = 3,
    Artifact = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Fault {
    pub class: Class,
    pub stage: Stage,
}

impl Fault {
    pub fn classify(error: &CompileError, stage: Stage) -> Self {
        Self {
            class: match error.class() {
                super::compile_error::Class::Unavailable => Class::Unavailable,
                super::compile_error::Class::Resource => Class::Resource,
                super::compile_error::Class::Compilation => Class::Compilation,
                super::compile_error::Class::Configuration => Class::Configuration,
            },
            stage,
        }
    }

    pub fn label(self) -> &'static str {
        match self.stage {
            Stage::Snapshot => "compiler worker snapshot",
            Stage::Target => "compiler worker target",
            Stage::Compiler => "compiler worker compilation",
            Stage::Artifact => "compiler worker artifact",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Error {
    Header,
    Limit,
    Identity,
    Checksum,
    Outcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome<'a> {
    Artifact(&'a [u8]),
    Failure(Fault),
}

impl<'a> Outcome<'a> {
    pub fn into_result(self) -> Result<&'a [u8], Fault> {
        match self {
            Self::Artifact(bytes) => Ok(bytes),
            Self::Failure(fault) => Err(fault),
        }
    }
}

fn checksum(header: &[u8], payload: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"luna-compiler-reply-v1\0");
    hash.update(&header[..72]);
    hash.update(&header[104..HEADER]);
    hash.update(payload);
    hash.finalize().into()
}

pub(super) fn header(
    outcome: Outcome<'_>,
    request: u64,
    binding: &[u8; 32],
) -> Result<[u8; HEADER], Error> {
    if request == 0 {
        return Err(Error::Identity);
    }
    let (payload, class, stage) = match outcome {
        Outcome::Artifact(bytes) if !bytes.is_empty() => (bytes, 0, 0),
        Outcome::Artifact(_) => return Err(Error::Outcome),
        Outcome::Failure(fault) => (&[][..], fault.class as u32, fault.stage as u32),
    };
    if payload.len() > PAYLOAD {
        return Err(Error::Limit);
    }
    let mut header = [0; HEADER];
    header[..8].copy_from_slice(MAGIC);
    header[8..12].copy_from_slice(&VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&class.to_le_bytes());
    header[16..24].copy_from_slice(&((HEADER + payload.len()) as u64).to_le_bytes());
    header[24..32].copy_from_slice(&request.to_le_bytes());
    header[32..64].copy_from_slice(binding);
    header[64..72].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    header[104..108].copy_from_slice(&stage.to_le_bytes());
    let digest = checksum(&header, payload);
    header[72..104].copy_from_slice(&digest);
    Ok(header)
}

pub(super) fn decode<'a>(
    bytes: &'a [u8],
    request: u64,
    binding: &[u8; 32],
) -> Result<Outcome<'a>, Error> {
    if bytes.len() < HEADER || bytes.len() > MAX_BYTES {
        return Err(Error::Limit);
    }
    let word = |offset| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    if &bytes[..8] != MAGIC
        || bytes[8..12] != VERSION.to_le_bytes()
        || bytes[108..112] != [0; 4]
        || word(16) != bytes.len() as u64
        || word(64) != (bytes.len() - HEADER) as u64
    {
        return Err(Error::Header);
    }
    if request == 0 || word(24) != request || &bytes[32..64] != binding {
        return Err(Error::Identity);
    }
    let class = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    let stage = u32::from_le_bytes(bytes[104..108].try_into().unwrap());
    let payload = &bytes[HEADER..];
    let outcome = if class == 0 {
        if stage != 0 || payload.is_empty() {
            return Err(Error::Outcome);
        }
        Outcome::Artifact(payload)
    } else {
        if !payload.is_empty() {
            return Err(Error::Outcome);
        }
        let class = match class {
            1 => Class::Unavailable,
            2 => Class::Resource,
            3 => Class::Compilation,
            4 => Class::Configuration,
            5 => Class::Rejected,
            _ => return Err(Error::Outcome),
        };
        let stage = match stage {
            1 => Stage::Snapshot,
            2 => Stage::Target,
            3 => Stage::Compiler,
            4 => Stage::Artifact,
            _ => return Err(Error::Outcome),
        };
        Outcome::Failure(Fault { class, stage })
    };
    if bytes[72..104] != checksum(bytes, payload) {
        return Err(Error::Checksum);
    }
    Ok(outcome)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Failure {
    Protocol(Error),
    Worker(Fault),
}

impl Failure {
    pub fn into_owned(self, allocator: BudgetAllocator) -> CompileError {
        match self {
            Self::Protocol(_) => CompileError::Unavailable("compiler response protocol"),
            Self::Worker(fault) => match fault.class {
                Class::Resource => CompileError::ResourceLimit(fault.label()),
                Class::Unavailable | Class::Rejected => CompileError::Unavailable(fault.label()),
                Class::Configuration => CompileError::InvalidConfiguration(fault.label()),
                Class::Compilation => {
                    let (text, charge) =
                        match super::global_box::try_string(fault.label(), 32, allocator) {
                            Ok(value) => value,
                            Err(_) => {
                                return CompileError::ResourceLimit("compiler error publication")
                            }
                        };
                    match super::diagnostic::Message::from_owned(text, charge) {
                        Ok(message) => CompileError::Compilation(message),
                        Err(_) => CompileError::ResourceLimit("compiler error publication"),
                    }
                }
            },
        }
    }
}

pub(super) struct Artifact(Vec<u8, BudgetAllocator>);

impl Artifact {
    pub fn new(
        bytes: Vec<u8, BudgetAllocator>,
        request: u64,
        binding: &[u8; 32],
    ) -> Result<Self, Failure> {
        decode(&bytes, request, binding)
            .map_err(Failure::Protocol)?
            .into_result()
            .map_err(Failure::Worker)?;
        Ok(Self(bytes))
    }
}

impl std::ops::Deref for Artifact {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0[HEADER..]
    }
}
