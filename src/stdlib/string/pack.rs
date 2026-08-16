//! `string.pack`, `string.unpack` and `string.packsize`.
//!
//! Pure byte manipulation over luna's byte strings — nothing here needs a C layout, only agreement
//! about widths and endianness.

/// One item in a format string.
pub enum Item {
    /// A signed or unsigned integer of `size` bytes.
    Int {
        size: usize,
        signed: bool,
    },
    Float,
    Double,
    /// A string with a `size`-byte length prefix.
    LenString {
        size: usize,
    },
    /// A zero-terminated string.
    ZeroString,
    /// Exactly `size` bytes.
    FixedString {
        size: usize,
    },
    /// One padding byte.
    Padding,
}

pub struct Format {
    pub items: Vec<Item>,
    pub little_endian: bool,
}

/// Native size for `l`/`L`/`j`/`J`/`T`, which luna fixes at 8 rather than reading the host's.
const NATIVE_SIZE: usize = 8;

pub fn parse_format(format: &[u8]) -> Result<Format, std::string::String> {
    let mut items = Vec::new();
    // PUC-Rio defaults to native endianness; every platform luna targets is little-endian, and
    // fixing it keeps packed data portable between them.
    let mut little_endian = true;
    let mut i = 0;

    // An optional decimal size directly after an option letter.
    fn read_size(
        format: &[u8],
        i: &mut usize,
        default: usize,
    ) -> Result<usize, std::string::String> {
        let start = *i;
        while *i < format.len() && format[*i].is_ascii_digit() {
            *i += 1;
        }
        if start == *i {
            return Ok(default);
        }
        std::str::from_utf8(&format[start..*i])
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n >= 1 && *n <= 16)
            .ok_or_else(|| "integral size out of limits".to_owned())
    }

    while i < format.len() {
        let c = format[i];
        i += 1;
        match c {
            b' ' => {}
            b'<' => little_endian = true,
            b'>' => little_endian = false,
            b'=' | b'!' => {
                // Native endianness and native alignment. luna packs without padding, so the
                // alignment request is accepted and has no effect.
                let _ = read_size(format, &mut i, 0)?;
            }
            b'b' => items.push(Item::Int {
                size: 1,
                signed: true,
            }),
            b'B' => items.push(Item::Int {
                size: 1,
                signed: false,
            }),
            b'h' => items.push(Item::Int {
                size: 2,
                signed: true,
            }),
            b'H' => items.push(Item::Int {
                size: 2,
                signed: false,
            }),
            b'i' => {
                let size = read_size(format, &mut i, 4)?;
                items.push(Item::Int { size, signed: true });
            }
            b'I' => {
                let size = read_size(format, &mut i, 4)?;
                items.push(Item::Int {
                    size,
                    signed: false,
                });
            }
            b'l' | b'j' => items.push(Item::Int {
                size: NATIVE_SIZE,
                signed: true,
            }),
            b'L' | b'J' | b'T' => items.push(Item::Int {
                size: NATIVE_SIZE,
                signed: false,
            }),
            b'f' => items.push(Item::Float),
            b'd' | b'n' => items.push(Item::Double),
            b's' => {
                let size = read_size(format, &mut i, NATIVE_SIZE)?;
                items.push(Item::LenString { size });
            }
            b'z' => items.push(Item::ZeroString),
            b'c' => {
                let size = read_size(format, &mut i, usize::MAX)?;
                if size == usize::MAX {
                    return Err("missing size for format option 'c'".to_owned());
                }
                items.push(Item::FixedString { size });
            }
            b'x' => items.push(Item::Padding),
            b'X' => {}
            other => return Err(format!("invalid format option '{}'", other as char)),
        }
    }

    Ok(Format {
        items,
        little_endian,
    })
}

/// The packed size of a format, for `packsize`. Errors on the variable-length options, as Lua does.
pub fn packed_size(format: &Format) -> Result<usize, std::string::String> {
    let mut total = 0;
    for item in &format.items {
        total += match item {
            Item::Int { size, .. } => *size,
            Item::Float => 4,
            Item::Double => 8,
            Item::FixedString { size } => *size,
            Item::Padding => 1,
            Item::LenString { .. } | Item::ZeroString => {
                return Err("variable-size format in packsize".to_owned())
            }
        };
    }
    Ok(total)
}

pub fn write_int(out: &mut Vec<u8>, value: i64, size: usize, little_endian: bool) {
    let bytes = value.to_le_bytes();
    let mut buf = vec![if value < 0 { 0xff } else { 0 }; size];
    for (k, slot) in buf.iter_mut().enumerate().take(size.min(8)) {
        *slot = bytes[k];
    }
    if !little_endian {
        buf.reverse();
    }
    out.extend_from_slice(&buf);
}

pub fn read_int(bytes: &[u8], size: usize, signed: bool, little_endian: bool) -> i64 {
    let mut buf = bytes[..size].to_vec();
    if !little_endian {
        buf.reverse();
    }
    let mut value: u64 = 0;
    for (k, b) in buf.iter().enumerate().take(8) {
        value |= (*b as u64) << (k * 8);
    }
    if signed && size < 8 {
        // Sign-extend from the top bit of the packed width.
        let sign_bit = 1u64 << (size * 8 - 1);
        if value & sign_bit != 0 {
            value |= !((1u64 << (size * 8)) - 1);
        }
    }
    value as i64
}
