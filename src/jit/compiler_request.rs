use allocator_api2::vec::Vec;
use sha2::{Digest, Sha256};

use super::{backend::artifact, resources::BudgetAllocator, work};

#[cfg(test)]
mod tests;

const MAGIC: &[u8; 8] = b"LUNAREQ\0";
const VERSION: u32 = 1;
const HEADER: usize = 192;
const SOURCE: usize = 1 << 20;
const IDENTITY: usize = 32768;
pub(super) const MAX_BYTES: usize = HEADER + SOURCE + 2 * IDENTITY;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Error {
    Header,
    Limit,
    Identity,
    Checksum,
    Allocation,
    Io,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    Ordinary = 1,
    Arrays = 2,
    Readonly = 3,
}

pub(super) struct View<'a> {
    pub request: u64,
    pub worker: &'a [u8; 32],
    pub abi: &'a [u8; 32],
    pub work: work::Limits,
    pub route: Route,
    pub resume: bool,
    pub features: &'a str,
    pub environment: &'a [u8],
    pub source: &'a [u8],
}

fn sizes(view: &View<'_>) -> Result<usize, Error> {
    if view.request == 0 {
        return Err(Error::Header);
    }
    if view.features.len() > IDENTITY
        || view.environment.len() > IDENTITY
        || view.source.len() > SOURCE
        || view.work.instructions > 1 << 20
        || view.work.blocks > 65536
        || view.work.relocations > 65536
    {
        return Err(Error::Limit);
    }
    artifact::validate_identity(view.features, view.environment).map_err(|_| Error::Identity)?;
    Ok(HEADER + view.features.len() + view.environment.len() + view.source.len())
}

pub(super) fn encode(
    view: &View<'_>,
    allocator: BudgetAllocator,
) -> Result<Vec<u8, BudgetAllocator>, Error> {
    let length = sizes(view)?;
    let mut bytes = Vec::new_in(allocator);
    bytes
        .try_reserve_exact(length)
        .map_err(|_| Error::Allocation)?;
    bytes.resize(HEADER, 0);
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8..12].copy_from_slice(&VERSION.to_le_bytes());
    bytes[16..24].copy_from_slice(&(length as u64).to_le_bytes());
    bytes[24..32].copy_from_slice(&view.request.to_le_bytes());
    bytes[32..64].copy_from_slice(view.worker);
    bytes[64..96].copy_from_slice(view.abi);
    for (offset, word) in [
        (96, view.work.instructions),
        (104, view.work.blocks),
        (112, view.work.relocations),
        (128, view.features.len()),
        (136, view.environment.len()),
        (144, view.source.len()),
    ] {
        bytes[offset..offset + 8].copy_from_slice(&(word as u64).to_le_bytes());
    }
    bytes[120..124].copy_from_slice(&(view.route as u32).to_le_bytes());
    bytes[124..128].copy_from_slice(&u32::from(view.resume).to_le_bytes());
    bytes.extend_from_slice(view.features.as_bytes());
    bytes.extend_from_slice(view.environment);
    bytes.extend_from_slice(view.source);
    let checksum = fingerprint(&bytes);
    bytes[152..184].copy_from_slice(&checksum);
    Ok(bytes)
}

fn fingerprint(bytes: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"luna-compiler-request-v1\0");
    hash.update(&bytes[..152]);
    hash.update(&bytes[184..]);
    hash.finalize().into()
}

pub(super) fn decode<'a>(
    bytes: &'a [u8],
    worker: &[u8; 32],
    abi: &[u8; 32],
) -> Result<View<'a>, Error> {
    if bytes.len() < HEADER || bytes.len() > MAX_BYTES {
        return Err(Error::Limit);
    }
    let word = |offset| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    let count = |offset| usize::try_from(word(offset)).map_err(|_| Error::Limit);
    if &bytes[..8] != MAGIC
        || bytes[8..12] != VERSION.to_le_bytes()
        || bytes[12..16] != [0; 4]
        || bytes[184..192] != [0; 8]
        || count(16)? != bytes.len()
        || word(24) == 0
    {
        return Err(Error::Header);
    }
    if &bytes[32..64] != worker || &bytes[64..96] != abi {
        return Err(Error::Identity);
    }
    let features_len = count(128)?;
    let environment_len = count(136)?;
    let source_len = count(144)?;
    if features_len > IDENTITY || environment_len > IDENTITY || source_len > SOURCE {
        return Err(Error::Limit);
    }
    let features_end = HEADER + features_len;
    let environment_end = features_end + environment_len;
    if environment_end + source_len != bytes.len() {
        return Err(Error::Header);
    }
    let route = match u32::from_le_bytes(bytes[120..124].try_into().unwrap()) {
        1 => Route::Ordinary,
        2 => Route::Arrays,
        3 => Route::Readonly,
        _ => return Err(Error::Header),
    };
    let resume = match u32::from_le_bytes(bytes[124..128].try_into().unwrap()) {
        0 => false,
        1 => true,
        _ => return Err(Error::Header),
    };
    let view = View {
        request: word(24),
        worker: bytes[32..64].try_into().unwrap(),
        abi: bytes[64..96].try_into().unwrap(),
        work: work::Limits {
            instructions: count(96)?,
            blocks: count(104)?,
            relocations: count(112)?,
        },
        route,
        resume,
        features: std::str::from_utf8(&bytes[HEADER..features_end]).map_err(|_| Error::Identity)?,
        environment: &bytes[features_end..environment_end],
        source: &bytes[environment_end..],
    };
    sizes(&view)?;
    if bytes[152..184] != fingerprint(bytes) {
        return Err(Error::Checksum);
    }
    Ok(view)
}

pub(super) fn binding(bytes: &[u8]) -> Result<[u8; 32], Error> {
    let checksum = bytes.get(152..184).ok_or(Error::Header)?;
    Ok(checksum.try_into().unwrap())
}

#[cfg(not(miri))]
pub(super) fn abi_identity() -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"luna-native-abi-source-v1;request=1;reply=1;source=1;artifact=1\0");
    for source in [
        &include_bytes!("abi.rs")[..],
        &include_bytes!("helpers.rs")[..],
        &include_bytes!("abi/transport_inputs.rs")[..],
        &include_bytes!("array_window/native.rs")[..],
        &include_bytes!("readonly_lookup/binding.rs")[..],
        &include_bytes!("readonly_lookup/binding/integer_resume.rs")[..],
    ] {
        hash.update((source.len() as u64).to_le_bytes());
        hash.update(source);
    }
    for layout in [
        std::mem::size_of::<usize>(),
        std::mem::size_of::<super::abi::Slot>(),
        std::mem::align_of::<super::abi::Slot>(),
        std::mem::offset_of!(super::abi::Slot, tag),
        std::mem::offset_of!(super::abi::Slot, bits),
        std::mem::size_of::<super::abi::Exit>(),
        std::mem::align_of::<super::abi::Exit>(),
        std::mem::offset_of!(super::abi::Exit, pc),
        std::mem::offset_of!(super::abi::Exit, instructions),
        std::mem::offset_of!(super::abi::Exit, reason),
        std::mem::size_of::<super::abi::Host>(),
        std::mem::align_of::<super::abi::Host>(),
        std::mem::offset_of!(super::abi::Host, data),
        std::mem::offset_of!(super::abi::Host, projection),
    ] {
        hash.update((layout as u64).to_le_bytes());
    }
    hash.update([u8::from(cfg!(target_endian = "little"))]);
    hash.finalize().into()
}

pub(super) fn image_identity(
    mut reader: impl std::io::Read,
    limit: u64,
) -> Result<[u8; 32], Error> {
    let mut hash = Sha256::new();
    hash.update(b"luna-compiler-image-v1\0");
    let mut scratch = [0; 16384];
    let mut length = 0u64;
    loop {
        let count = match reader.read(&mut scratch) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(Error::Io),
        };
        if count == 0 {
            break;
        }
        length = length.checked_add(count as u64).ok_or(Error::Limit)?;
        if length > limit {
            return Err(Error::Limit);
        }
        hash.update(&scratch[..count]);
    }
    if length == 0 {
        return Err(Error::Identity);
    }
    hash.update(length.to_le_bytes());
    Ok(hash.finalize().into())
}

#[cfg(not(miri))]
pub(super) fn worker_identity() -> Result<[u8; 32], Error> {
    static IMAGE: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    if let Some(identity) = IMAGE.get() {
        return Ok(*identity);
    }
    let file = std::fs::File::open("/proc/self/exe").map_err(|_| Error::Io)?;
    let digest = image_identity(file, 512 << 20)?;
    let _ = IMAGE.set(digest);
    Ok(*IMAGE.get().unwrap())
}
