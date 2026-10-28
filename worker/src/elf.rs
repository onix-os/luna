fn word<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], String> {
    bytes
        .get(offset..offset.checked_add(N).ok_or("ELF offset overflow")?)
        .ok_or_else(|| "truncated ELF field".to_owned())?
        .try_into()
        .map_err(|_| "invalid ELF field".into())
}

pub fn validate(bytes: &[u8], target: &str) -> Result<(), String> {
    let machine = match target {
        "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl" => 62,
        "aarch64-unknown-linux-gnu" => 183,
        _ => return Err("unsupported producer target".into()),
    };
    if bytes.len() < 64
        || &bytes[..7] != b"\x7fELF\x02\x01\x01"
        || !matches!(bytes[7], 0 | 3)
        || bytes[8] != 0
        || !matches!(u16::from_le_bytes(word(bytes, 16)?), 2 | 3)
        || u16::from_le_bytes(word(bytes, 18)?) != machine
        || u32::from_le_bytes(word(bytes, 20)?) != 1
        || u16::from_le_bytes(word(bytes, 52)?) != 64
        || u16::from_le_bytes(word(bytes, 54)?) != 56
    {
        return Err("producer must be a target-matched 64-bit Linux ELF".into());
    }
    let offset = usize::try_from(u64::from_le_bytes(word(bytes, 32)?))
        .map_err(|_| "ELF program offset overflow")?;
    let count = usize::from(u16::from_le_bytes(word(bytes, 56)?));
    if count == 0 || count > 128 {
        return Err("invalid producer program-header count".into());
    }
    let end = offset
        .checked_add(count * 56)
        .ok_or("ELF headers overflow")?;
    let headers = bytes.get(offset..end).ok_or("truncated ELF headers")?;
    let entry = u64::from_le_bytes(word(bytes, 24)?);
    let mut executable_entry = false;
    for header in headers.chunks_exact(56) {
        let kind = u32::from_le_bytes(word(header, 0)?);
        if kind == 3 {
            return Err("embedded producer has an ELF interpreter".into());
        }
        let start = usize::try_from(u64::from_le_bytes(word(header, 8)?))
            .map_err(|_| "ELF segment offset overflow")?;
        let size = usize::try_from(u64::from_le_bytes(word(header, 32)?))
            .map_err(|_| "ELF segment size overflow")?;
        let end = start.checked_add(size).ok_or("ELF segment overflow")?;
        let segment = bytes.get(start..end).ok_or("truncated ELF segment")?;
        if kind == 1 {
            let address = u64::from_le_bytes(word(header, 16)?);
            let memory = u64::from_le_bytes(word(header, 40)?);
            if memory < size as u64 || address.checked_add(memory).is_none() {
                return Err("invalid ELF load range".into());
            }
            if u32::from_le_bytes(word(header, 4)?) & 1 != 0
                && entry >= address
                && entry - address < size as u64
            {
                executable_entry = true;
            }
        }
        if kind == 2 {
            if size == 0 || size % 16 != 0 {
                return Err("invalid ELF dynamic range".into());
            }
            let mut terminated = false;
            for item in segment.chunks_exact(16) {
                match u64::from_le_bytes(word(item, 0)?) {
                    0 => {
                        terminated = true;
                        break;
                    }
                    1 | 15 | 29 => {
                        return Err("embedded producer has runtime ELF dependencies".into())
                    }
                    _ => {}
                }
            }
            if !terminated {
                return Err("unterminated ELF dynamic range".into());
            }
        }
    }
    if !executable_entry {
        return Err("producer entry is not file-backed executable code".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> Vec<u8> {
        let mut image = vec![0; 256];
        image[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        image[16..18].copy_from_slice(&3u16.to_le_bytes());
        image[18..20].copy_from_slice(&62u16.to_le_bytes());
        image[20..24].copy_from_slice(&1u32.to_le_bytes());
        image[24..32].copy_from_slice(&0x10f8u64.to_le_bytes());
        image[32..40].copy_from_slice(&64u64.to_le_bytes());
        image[52..54].copy_from_slice(&64u16.to_le_bytes());
        image[54..56].copy_from_slice(&56u16.to_le_bytes());
        image[56..58].copy_from_slice(&1u16.to_le_bytes());
        image[64..68].copy_from_slice(&1u32.to_le_bytes());
        image[68..72].copy_from_slice(&5u32.to_le_bytes());
        image[80..88].copy_from_slice(&0x1000u64.to_le_bytes());
        image[96..104].copy_from_slice(&256u64.to_le_bytes());
        image[104..112].copy_from_slice(&256u64.to_le_bytes());
        image
    }

    #[test]
    fn supported_machines_and_all_truncations_are_checked() {
        let mut bytes = image();
        for target in ["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"] {
            validate(&bytes, target).unwrap();
        }
        for end in 0..bytes.len() {
            assert!(validate(&bytes[..end], "x86_64-unknown-linux-gnu").is_err());
        }
        assert!(validate(&bytes, "aarch64-unknown-linux-gnu").is_err());
        bytes[18..20].copy_from_slice(&183u16.to_le_bytes());
        validate(&bytes, "aarch64-unknown-linux-gnu").unwrap();
        assert!(validate(&bytes, "i686-unknown-linux-musl").is_err());
    }

    #[test]
    fn interpreter_dependencies_search_paths_and_missing_termination_refuse() {
        let mut bytes = image();
        bytes[56..58].copy_from_slice(&2u16.to_le_bytes());
        bytes[120..124].copy_from_slice(&2u32.to_le_bytes());
        bytes[128..136].copy_from_slice(&192u64.to_le_bytes());
        bytes[152..160].copy_from_slice(&32u64.to_le_bytes());
        validate(&bytes, "x86_64-unknown-linux-gnu").unwrap();
        for tag in [1u64, 15, 29] {
            bytes[192..200].copy_from_slice(&tag.to_le_bytes());
            assert!(validate(&bytes, "x86_64-unknown-linux-gnu").is_err());
        }
        bytes[192..200].copy_from_slice(&5u64.to_le_bytes());
        bytes[208..216].copy_from_slice(&6u64.to_le_bytes());
        assert!(validate(&bytes, "x86_64-unknown-linux-gnu").is_err());
        bytes[120..124].copy_from_slice(&3u32.to_le_bytes());
        assert!(validate(&bytes, "x86_64-unknown-linux-gnu").is_err());
    }

    #[test]
    fn malformed_counts_ranges_osabi_and_entry_are_total() {
        for offset in [7usize, 8, 52, 54, 56] {
            let mut bytes = image();
            bytes[offset] = 255;
            assert!(validate(&bytes, "x86_64-unknown-linux-gnu").is_err());
        }
        for offset in [24usize, 32, 72, 80, 96, 104] {
            let mut bytes = image();
            bytes[offset..offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
            assert!(validate(&bytes, "x86_64-unknown-linux-gnu").is_err());
        }
    }
}
