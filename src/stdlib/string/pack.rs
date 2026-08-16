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

/// The widest integer any option may name, as PUC's `MAXINTSIZE`.
const MAX_INT_SIZE: usize = 16;

/// Read the optional decimal count after an option letter, or `None` when there is none.
fn read_count(format: &[u8], i: &mut usize) -> Option<Result<usize, std::string::String>> {
    let start = *i;
    while *i < format.len() && format[*i].is_ascii_digit() {
        *i += 1;
    }
    if start == *i {
        return None;
    }
    // A count far past the string cap can only fail later, and parsing it into a `usize` first
    // would overflow, so both are the same error here.
    let mut n: usize = 0;
    for &c in &format[start..*i] {
        n = match n
            .checked_mul(10)
            .and_then(|n| n.checked_add((c - b'0') as usize))
        {
            Some(n) if n <= crate::string::MAX_STRING_LENGTH => n,
            _ => return Some(Err("format result too large".to_owned())),
        };
    }
    Some(Ok(n))
}

pub fn parse_format(format: &[u8]) -> Result<Format, std::string::String> {
    let mut items = Vec::new();
    // PUC-Rio defaults to native endianness; every platform luna targets is little-endian, and
    // fixing it keeps packed data portable between them.
    let mut little_endian = true;
    let mut i = 0;

    // The size of an integer-shaped option, which has a real upper bound.
    fn read_size(
        format: &[u8],
        i: &mut usize,
        default: usize,
    ) -> Result<usize, std::string::String> {
        match read_count(format, i) {
            None => Ok(default),
            Some(Ok(n)) if (1..=MAX_INT_SIZE).contains(&n) => Ok(n),
            Some(_) => Err("integral size out of limits".to_owned()),
        }
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
                // `c` is the one variable-width option: PUC's `getnum` puts no limit on it beyond
                // what can actually be built, so here the limit is luna's string length cap.
                let size = match read_count(format, &mut i) {
                    None => return Err("missing size for format option 'c'".to_owned()),
                    Some(size) => size?,
                };
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
        let size = match item {
            Item::Int { size, .. } => *size,
            Item::Float => 4,
            Item::Double => 8,
            Item::FixedString { size } => *size,
            Item::Padding => 1,
            Item::LenString { .. } | Item::ZeroString => {
                return Err("variable-length format".to_owned())
            }
        };
        total = room(total, size)?;
    }
    Ok(total)
}

/// Grow a packed length by `add`, refusing anything past the string length cap.
///
/// A single `c` option is capped when the format is parsed, but a format may repeat it, so the
/// running total needs the same bound — and it has to be applied before the bytes are allocated.
pub fn room(total: usize, add: usize) -> Result<usize, std::string::String> {
    match total.checked_add(add) {
        Some(t) if t <= crate::string::MAX_STRING_LENGTH => Ok(t),
        _ => Err("format result too large".to_owned()),
    }
}

/// Refuse a value that an option narrower than a Lua integer cannot hold, as PUC's `str_pack` does.
///
/// Without this the extra bits are simply dropped, so `string.pack("i1", 300)` answers `"\44"` and
/// the corruption only surfaces wherever the bytes are read back. An option 8 bytes or wider has
/// nothing to check: every Lua integer fits.
pub fn check_int_range(value: i64, size: usize, signed: bool) -> Result<(), std::string::String> {
    if size >= NATIVE_SIZE {
        return Ok(());
    }
    if signed {
        let lim = 1i64 << (size * 8 - 1);
        if -lim <= value && value < lim {
            return Ok(());
        }
        Err("integer overflow".to_owned())
    } else {
        // Unsigned takes the whole width, but reads the value as unsigned first, so a negative
        // one becomes enormous and fails.
        if (value as u64) < 1u64 << (size * 8) {
            return Ok(());
        }
        Err("unsigned overflow".to_owned())
    }
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

pub fn read_int(
    bytes: &[u8],
    size: usize,
    signed: bool,
    little_endian: bool,
) -> Result<i64, std::string::String> {
    // Byte `k` counts from the least significant end, whichever end of the buffer that is.
    let byte = |k: usize| bytes[if little_endian { k } else { size - 1 - k }];

    let mut value: u64 = 0;
    for k in 0..size.min(8) {
        value |= (byte(k) as u64) << (k * 8);
    }

    if size < 8 {
        if signed {
            // Sign-extend from the top bit of the packed width.
            let mask = 1u64 << (size * 8 - 1);
            value = (value ^ mask).wrapping_sub(mask);
        }
    } else if size > 8 {
        // The bytes above the low eight are unread, so they have to be a pure sign extension of
        // them; anything else names a value with no Lua integer.
        let fill = if signed && (value as i64) < 0 {
            0xff
        } else {
            0
        };
        for k in 8..size {
            if byte(k) != fill {
                return Err(format!(
                    "{}-byte integer does not fit into Lua Integer",
                    size
                ));
            }
        }
    }
    Ok(value as i64)
}
