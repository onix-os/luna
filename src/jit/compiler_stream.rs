use std::io::{self, Read, Write};

use allocator_api2::vec::Vec;

use super::{compiler_reply as reply, compiler_request as request, resources::BudgetAllocator};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IoFault {
    pub kind: io::ErrorKind,
    pub os: Option<i32>,
}

impl From<io::Error> for IoFault {
    fn from(error: io::Error) -> Self {
        Self {
            kind: error.kind(),
            os: error.raw_os_error(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Error {
    Configuration,
    Limit,
    Allocation,
    Read(IoFault),
    Write(IoFault),
    #[cfg(not(miri))]
    Service(super::compiler_service::Error),
}

fn read(
    input: &mut impl Read,
    limit: usize,
    allocator: BudgetAllocator,
) -> Result<Vec<u8, BudgetAllocator>, Error> {
    if limit > request::MAX_BYTES {
        return Err(Error::Configuration);
    }
    let mut bytes = Vec::new_in(allocator);
    let mut scratch = [0; 4096];
    loop {
        let remaining = limit - bytes.len();
        let offered = scratch.len().min(remaining + 1);
        let count = match input.read(&mut scratch[..offered]) {
            Ok(0) => return Ok(bytes),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Error::Read(error.into())),
        };
        if count > offered {
            return Err(Error::Read(IoFault {
                kind: io::ErrorKind::InvalidData,
                os: None,
            }));
        }
        if count > remaining {
            return Err(Error::Limit);
        }
        let needed = bytes.len() + count;
        if needed > bytes.capacity() {
            let capacity = bytes.capacity().saturating_mul(2).max(needed).min(limit);
            bytes
                .try_reserve_exact(capacity - bytes.len())
                .map_err(|_| Error::Allocation)?;
        }
        bytes.extend_from_slice(&scratch[..count]);
    }
}

fn write(output: &mut impl Write, mut bytes: &[u8]) -> Result<(), Error> {
    while !bytes.is_empty() {
        match output.write(bytes) {
            Ok(count) if count > bytes.len() => {
                return Err(Error::Write(IoFault {
                    kind: io::ErrorKind::InvalidData,
                    os: None,
                }))
            }
            Ok(0) => {
                return Err(Error::Write(IoFault {
                    kind: io::ErrorKind::WriteZero,
                    os: None,
                }))
            }
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(Error::Write(error.into())),
        }
    }
    Ok(())
}

fn send(output: &mut impl Write, header: &[u8], payload: &[u8]) -> Result<(), Error> {
    if header.len() != reply::HEADER || payload.len() > reply::MAX_BYTES - reply::HEADER {
        return Err(Error::Configuration);
    }
    write(output, header)?;
    write(output, payload)?;
    loop {
        match output.flush() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(Error::Write(error.into())),
        }
    }
}

#[cfg(not(miri))]
pub(super) struct Service {
    pub worker: [u8; 32],
    pub abi: [u8; 32],
    pub request_limit: usize,
    pub input: BudgetAllocator,
    pub limits: super::compiler_service::Limits,
    pub allocators: super::compiler_service::Allocators,
}

#[cfg(not(miri))]
pub(super) fn serve(
    input: &mut impl Read,
    output: &mut impl Write,
    service: Service,
) -> Result<(), Error> {
    service.limits.validate().map_err(Error::Service)?;
    let packet = read(input, service.request_limit, service.input)?;
    let response = super::compiler_service::handle(
        packet,
        &service.worker,
        &service.abi,
        service.limits,
        service.allocators,
    )
    .map_err(Error::Service)?;
    send(output, response.header(), response.payload())
}
