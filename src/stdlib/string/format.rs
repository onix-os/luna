// string.format implementation for ottavino
// Implements Lua 5.4 string.format semantics

use ottavino_gc_arena::Gc;

use crate::{Function, Value};

/// Returns the arg indices (1-based, matching args slice) that correspond to `%s` specifiers.
/// Used by mod.rs to pre-convert table/userdata args via __tostring.
pub fn find_string_arg_positions(fmt_bytes: &[u8]) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut pos = 0;
    let mut arg_index = 1usize;

    while pos < fmt_bytes.len() {
        while pos < fmt_bytes.len() && fmt_bytes[pos] != b'%' {
            pos += 1;
        }
        if pos >= fmt_bytes.len() {
            break;
        }
        pos += 1;
        if pos >= fmt_bytes.len() {
            break;
        }
        if fmt_bytes[pos] == b'%' {
            pos += 1;
            continue;
        }
        // skip flags
        while pos < fmt_bytes.len() && matches!(fmt_bytes[pos], b'-' | b'+' | b' ' | b'#' | b'0') {
            pos += 1;
        }
        // skip width
        while pos < fmt_bytes.len() && fmt_bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        // skip precision
        if pos < fmt_bytes.len() && fmt_bytes[pos] == b'.' {
            pos += 1;
            while pos < fmt_bytes.len() && fmt_bytes[pos].is_ascii_digit() {
                pos += 1;
            }
        }
        if pos < fmt_bytes.len() {
            let spec = fmt_bytes[pos];
            pos += 1;
            match spec {
                b'q' | b'p' => {
                    arg_index += 1;
                } // these handle any type, skip
                b's' => {
                    positions.push(arg_index);
                    arg_index += 1;
                }
                _ => {
                    arg_index += 1;
                }
            }
        }
    }
    positions
}

pub fn format_value<'gc>(
    fmt_bytes: &[u8],
    args: &[Value<'gc>],
    ctx: crate::Context<'gc>,
) -> Result<Vec<u8>, std::string::String> {
    let mut result = Vec::with_capacity(fmt_bytes.len() + 16);
    let mut pos = 0;
    let mut arg_index = 1usize; // 1-indexed (0 is the format string)

    while pos < fmt_bytes.len() {
        // Copy literal text up to next '%'
        let start = pos;
        while pos < fmt_bytes.len() && fmt_bytes[pos] != b'%' {
            pos += 1;
        }
        result.extend_from_slice(&fmt_bytes[start..pos]);
        if pos >= fmt_bytes.len() {
            break;
        }

        // Skip '%'
        pos += 1;
        if pos >= fmt_bytes.len() {
            return Err("incomplete format string".to_string());
        }

        // '%%' literal percent
        if fmt_bytes[pos] == b'%' {
            result.push(b'%');
            pos += 1;
            continue;
        }

        // Parse flags: [-+ #0]
        let flags_start = pos;
        while pos < fmt_bytes.len() && matches!(fmt_bytes[pos], b'-' | b'+' | b' ' | b'#' | b'0') {
            pos += 1;
        }

        // Parse width
        let width_start = pos;
        while pos < fmt_bytes.len() && fmt_bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        let width_end = pos;

        // Parse precision
        let mut precision: Option<usize> = None;
        if pos < fmt_bytes.len() && fmt_bytes[pos] == b'.' {
            pos += 1;
            let prec_start = pos;
            while pos < fmt_bytes.len() && fmt_bytes[pos].is_ascii_digit() {
                pos += 1;
            }
            let prec_str = std::str::from_utf8(&fmt_bytes[prec_start..pos]).unwrap_or("");
            precision = Some(prec_str.parse().unwrap_or(0));
        }

        let flags_bytes = &fmt_bytes[flags_start..width_start];
        let width_bytes = &fmt_bytes[width_start..width_end];

        // Check format spec length limits
        let spec_len = pos - flags_start + 2; // +2 for '%' and spec char
        if spec_len > 200 {
            return Err("invalid format (too long)".to_string());
        }

        // Validate width and precision
        let width_str = std::str::from_utf8(width_bytes).unwrap_or("");
        let width: Option<usize> = if width_bytes.is_empty() {
            None
        } else {
            let w: usize = width_str.parse().unwrap_or(0);
            if w > 99 {
                return Err("invalid format (invalid conversion)".to_string());
            }
            Some(w)
        };
        if let Some(prec) = precision {
            if prec > 99 {
                return Err("invalid format (invalid conversion)".to_string());
            }
        }

        if pos >= fmt_bytes.len() {
            return Err("incomplete format string".to_string());
        }

        let fmt_char = fmt_bytes[pos] as char;
        pos += 1;

        let flags_str = std::str::from_utf8(flags_bytes).unwrap_or("");
        let has_minus = flags_str.contains('-');
        let has_plus = flags_str.contains('+');
        let has_space = flags_str.contains(' ');
        let has_zero = flags_str.contains('0');
        let has_hash = flags_str.contains('#');

        // Validate flags per format specifier
        match fmt_char {
            'q' => {
                if !flags_str.is_empty() || width.is_some() || precision.is_some() {
                    return Err(
                        "specifier '%q' cannot have modifiers (width, precision, flags)"
                            .to_string(),
                    );
                }
            }
            'c' => {
                if precision.is_some() || has_zero {
                    return Err("invalid format (invalid conversion)".to_string());
                }
            }
            's' => {
                if has_zero {
                    return Err("invalid format (invalid conversion)".to_string());
                }
            }
            'd' | 'i' => {
                if has_hash {
                    return Err("invalid format (invalid conversion)".to_string());
                }
            }
            'p' => {
                if precision.is_some() {
                    return Err("invalid format (invalid conversion)".to_string());
                }
            }
            'F' => {
                return Err("invalid format (invalid conversion)".to_string());
            }
            _ => {}
        }

        // Get the argument (except for %q which may handle all types)
        let arg = if fmt_char == 'q' {
            args.get(arg_index).copied().unwrap_or(Value::Nil)
        } else {
            let a = args
                .get(arg_index)
                .copied()
                .ok_or_else(|| format!("bad argument #{} to 'format' (no value)", arg_index + 1))?;
            a
        };
        arg_index += 1;

        match fmt_char {
            'd' | 'i' => {
                let n = value_to_integer(arg)?;
                let s = format_integer(
                    n, width, precision, has_minus, has_plus, has_space, has_zero,
                );
                result.extend_from_slice(s.as_bytes());
            }
            'u' => {
                let n = value_to_integer(arg)?;
                // Special case: %.u with 0 = ""
                if precision == Some(0) && n == 0 {
                    apply_width(&mut result, b"", width, has_minus, b' ');
                } else {
                    let s = format!("{}", n as u64);
                    let s = apply_precision_digits(&s, precision);
                    let s = apply_width_str(
                        &s,
                        width,
                        has_minus,
                        has_zero && precision.is_none(),
                        false,
                        None,
                    );
                    result.extend_from_slice(s.as_bytes());
                }
            }
            'o' => {
                let n = value_to_integer(arg)?;
                let s = format!("{:o}", n as u64);
                let s = if has_hash && n != 0 && !s.starts_with('0') {
                    format!("0{}", s)
                } else {
                    s
                };
                let s = apply_precision_digits(&s, precision);
                let s = apply_width_str(
                    &s,
                    width,
                    has_minus,
                    has_zero && precision.is_none(),
                    false,
                    None,
                );
                result.extend_from_slice(s.as_bytes());
            }
            'x' => {
                let n = value_to_integer(arg)?;
                let s = format!("{:x}", n as u64);
                let prefix = if has_hash && n != 0 { "0x" } else { "" };
                let s = apply_precision_digits(&s, precision);
                let padchar = if has_zero && precision.is_none() {
                    b'0'
                } else {
                    b' '
                };
                let s = apply_width_with_prefix(&s, prefix, width, has_minus, padchar);
                result.extend_from_slice(s.as_bytes());
            }
            'X' => {
                let n = value_to_integer(arg)?;
                let s = format!("{:X}", n as u64);
                let prefix = if has_hash && n != 0 { "0X" } else { "" };
                let s = apply_precision_digits(&s, precision);
                let padchar = if has_zero && precision.is_none() {
                    b'0'
                } else {
                    b' '
                };
                let s = apply_width_with_prefix(&s, prefix, width, has_minus, padchar);
                result.extend_from_slice(s.as_bytes());
            }
            'f' => {
                let n = value_to_float(arg)?;
                let prec = precision.unwrap_or(6);
                let s = format_float_f(n, prec, has_plus, has_space, has_zero, has_hash, width);
                result.extend_from_slice(s.as_bytes());
            }
            'e' => {
                let n = value_to_float(arg)?;
                let prec = precision.unwrap_or(6);
                let s = format_float_e(n, prec, false, has_plus, has_space, has_zero, width);
                result.extend_from_slice(s.as_bytes());
            }
            'E' => {
                let n = value_to_float(arg)?;
                let prec = precision.unwrap_or(6);
                let s = format_float_e(n, prec, true, has_plus, has_space, has_zero, width);
                result.extend_from_slice(s.as_bytes());
            }
            'g' | 'G' => {
                let n = value_to_float(arg)?;
                let prec = precision.unwrap_or(6);
                let upper = fmt_char == 'G';
                let s = format_float_g(
                    n, prec, upper, has_plus, has_space, has_zero, has_hash, width,
                );
                result.extend_from_slice(s.as_bytes());
            }
            'a' | 'A' => {
                let n = value_to_float(arg)?;
                let upper = fmt_char == 'A';
                let s = format_hex_float(n, precision, upper, has_plus, has_space, has_zero, width);
                result.extend_from_slice(s.as_bytes());
            }
            'c' => {
                let n = value_to_integer(arg)?;
                if !(0..=127).contains(&n) {
                    return Err(format!(
                        "bad argument #{} to 'format' (value out of range for %%c)",
                        arg_index
                    ));
                }
                let ch = n as u8;
                apply_width(&mut result, &[ch], width, has_minus, b' ');
            }
            's' => {
                // Convert arg to string
                let s_bytes = value_to_string(arg, ctx)?;
                // Apply precision (max length)
                let s_bytes = if let Some(prec) = precision {
                    if prec < s_bytes.len() {
                        // Lua: %.Ns truncates to N chars. But if string has NUL, error
                        if s_bytes[..prec].contains(&0) {
                            return Err(format!(
                                "bad argument #{} to 'format' (string contains zeros)",
                                arg_index
                            ));
                        }
                        &s_bytes[..prec]
                    } else {
                        // check for NUL only if precision is None
                        s_bytes
                    }
                } else {
                    // Check for NUL in string for %s without precision
                    if width.is_some() && s_bytes.contains(&0) {
                        return Err(format!(
                            "bad argument #{} to 'format' (string contains zeros)",
                            arg_index
                        ));
                    }
                    s_bytes
                };
                apply_width(&mut result, s_bytes, width, has_minus, b' ');
            }
            'q' => {
                format_quoted_value(arg, &mut result)?;
            }
            'p' => {
                let ptr_str = format_pointer(arg);
                let null = "(null)";
                let s = if ptr_str == null {
                    null.to_string()
                } else {
                    ptr_str
                };
                apply_width(&mut result, s.as_bytes(), width, has_minus, b' ');
            }
            _ => {
                return Err(format!(
                    "invalid option '%{}' to 'format' (invalid conversion)",
                    fmt_char
                ));
            }
        }
    }

    Ok(result)
}

fn value_to_integer<'gc>(v: Value<'gc>) -> Result<i64, std::string::String> {
    match v {
        Value::Integer(i) => Ok(i),
        Value::Number(n) => {
            if n.fract() == 0.0 && n.is_finite() {
                Ok(n as i64)
            } else {
                Err("number has no integer representation".to_string())
            }
        }
        _ => Err(format!(
            "bad argument to 'format' (number expected, got {})",
            v.type_name()
        )),
    }
}

fn value_to_float<'gc>(v: Value<'gc>) -> Result<f64, std::string::String> {
    match v {
        Value::Integer(i) => Ok(i as f64),
        Value::Number(n) => Ok(n),
        _ => Err(format!(
            "bad argument to 'format' (number expected, got {})",
            v.type_name()
        )),
    }
}

fn value_to_string<'gc>(
    v: Value<'gc>,
    ctx: crate::Context<'gc>,
) -> Result<&'gc [u8], std::string::String> {
    match v {
        Value::String(s) => Ok(s.as_bytes()),
        Value::Integer(i) => {
            let s = i.to_string();
            let interned = ctx.intern(s.as_bytes());
            Ok(interned.as_bytes())
        }
        Value::Number(n) => {
            let s = format_number(n);
            let interned = ctx.intern(s.as_bytes());
            Ok(interned.as_bytes())
        }
        Value::Nil => Ok(ctx.intern(b"nil").as_bytes()),
        Value::Boolean(b) => Ok(ctx.intern(if b { b"true" } else { b"false" }).as_bytes()),
        _ => Err(format!(
            "bad argument to 'format' (string expected, got {})",
            v.type_name()
        )),
    }
}

fn format_number(n: f64) -> std::string::String {
    if n.is_nan() {
        return "-nan".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        };
    }
    // Use %g style: 14 significant digits
    let s = format!("{:.14}", n);
    // Remove trailing zeros (but keep at least one digit after decimal)
    if s.contains('.') && !s.contains('e') {
        let trimmed = s.trim_end_matches('0');
        let trimmed = trimmed.trim_end_matches('.');
        if trimmed.contains('.') {
            trimmed.to_string()
        } else {
            format!("{}.0", trimmed)
        }
    } else {
        s
    }
}

fn apply_precision_digits(s: &str, precision: Option<usize>) -> std::string::String {
    if let Some(prec) = precision {
        if s.len() < prec {
            format!("{:0>width$}", s, width = prec)
        } else {
            s.to_string()
        }
    } else {
        s.to_string()
    }
}

fn apply_width(result: &mut Vec<u8>, s: &[u8], width: Option<usize>, left_align: bool, pad: u8) {
    let len = s.len();
    if let Some(w) = width {
        if w > len {
            if left_align {
                result.extend_from_slice(s);
                result.extend(std::iter::repeat(pad).take(w - len));
            } else {
                result.extend(std::iter::repeat(pad).take(w - len));
                result.extend_from_slice(s);
            }
        } else {
            result.extend_from_slice(s);
        }
    } else {
        result.extend_from_slice(s);
    }
}

fn apply_width_str(
    s: &str,
    width: Option<usize>,
    left_align: bool,
    zero_pad: bool,
    _has_plus: bool,
    _sign: Option<char>,
) -> std::string::String {
    let len = s.len();
    if let Some(w) = width {
        if w > len {
            let pad = if zero_pad { '0' } else { ' ' };
            if left_align {
                format!(
                    "{}{}",
                    s,
                    std::iter::repeat(pad)
                        .take(w - len)
                        .collect::<std::string::String>()
                )
            } else {
                format!(
                    "{}{}",
                    std::iter::repeat(pad)
                        .take(w - len)
                        .collect::<std::string::String>(),
                    s
                )
            }
        } else {
            s.to_string()
        }
    } else {
        s.to_string()
    }
}

fn apply_width_with_prefix(
    s: &str,
    prefix: &str,
    width: Option<usize>,
    left_align: bool,
    pad: u8,
) -> std::string::String {
    let total = prefix.len() + s.len();
    if let Some(w) = width {
        if w > total {
            let padding = w - total;
            if left_align {
                format!(
                    "{}{}{}",
                    prefix,
                    s,
                    std::iter::repeat(pad as char)
                        .take(padding)
                        .collect::<std::string::String>()
                )
            } else if pad == b'0' {
                format!(
                    "{}{}{}",
                    prefix,
                    std::iter::repeat('0')
                        .take(padding)
                        .collect::<std::string::String>(),
                    s
                )
            } else {
                format!(
                    "{}{}{}",
                    std::iter::repeat(pad as char)
                        .take(padding)
                        .collect::<std::string::String>(),
                    prefix,
                    s
                )
            }
        } else {
            format!("{}{}", prefix, s)
        }
    } else {
        format!("{}{}", prefix, s)
    }
}

fn format_integer(
    n: i64,
    width: Option<usize>,
    precision: Option<usize>,
    left_align: bool,
    has_plus: bool,
    has_space: bool,
    zero_pad: bool,
) -> std::string::String {
    let is_neg = n < 0;
    let abs_val = if is_neg {
        format!("{}", n.wrapping_neg() as u64)
    } else {
        format!("{}", n as u64)
    };

    let sign = if is_neg {
        "-"
    } else if has_plus {
        "+"
    } else if has_space {
        " "
    } else {
        ""
    };

    // Apply precision (minimum digit count)
    let digits = if let Some(prec) = precision {
        if abs_val.len() < prec {
            format!("{:0>width$}", abs_val, width = prec)
        } else {
            abs_val.clone()
        }
    } else {
        abs_val.clone()
    };

    // Special case: precision 0 with value 0 = ""
    if precision == Some(0) && n == 0 {
        let s = format!("{}", sign);
        return apply_width_str(&s, width, left_align, false, false, None);
    }

    let full = format!("{}{}", sign, digits);
    let use_zero = zero_pad && precision.is_none();

    if let Some(w) = width {
        let len = full.len();
        if w > len {
            let padding = w - len;
            if left_align {
                format!(
                    "{}{}",
                    full,
                    std::iter::repeat(' ')
                        .take(padding)
                        .collect::<std::string::String>()
                )
            } else if use_zero {
                // zero padding goes between sign and digits
                format!(
                    "{}{}{}",
                    sign,
                    std::iter::repeat('0')
                        .take(padding)
                        .collect::<std::string::String>(),
                    digits
                )
            } else {
                format!(
                    "{}{}",
                    std::iter::repeat(' ')
                        .take(padding)
                        .collect::<std::string::String>(),
                    full
                )
            }
        } else {
            full
        }
    } else {
        full
    }
}

fn format_float_f(
    n: f64,
    prec: usize,
    has_plus: bool,
    has_space: bool,
    zero_pad: bool,
    has_hash: bool,
    width: Option<usize>,
) -> std::string::String {
    if n.is_nan() {
        let s = if n.is_sign_negative() { "-nan" } else { "nan" };
        return apply_width_str(s, width, false, false, false, None);
    }
    if n.is_infinite() {
        let s = if n > 0.0 {
            if has_plus {
                "+inf"
            } else {
                "inf"
            }
        } else {
            "-inf"
        };
        return apply_width_str(s, width, false, false, false, None);
    }
    let sign = if n < 0.0 {
        "-"
    } else if has_plus {
        "+"
    } else if has_space {
        " "
    } else {
        ""
    };
    let abs_n = n.abs();
    let mut digits = format!("{:.prec$}", abs_n, prec = prec);
    // With # flag and prec==0, always include a decimal point
    if has_hash && prec == 0 && !digits.contains('.') {
        digits.push('.');
    }
    let full = format!("{}{}", sign, digits);
    if let Some(w) = width {
        let len = full.len();
        if w > len {
            let padding = w - len;
            if zero_pad {
                format!(
                    "{}{}{}",
                    sign,
                    std::iter::repeat('0')
                        .take(padding)
                        .collect::<std::string::String>(),
                    digits
                )
            } else {
                format!(
                    "{}{}",
                    std::iter::repeat(' ')
                        .take(padding)
                        .collect::<std::string::String>(),
                    full
                )
            }
        } else {
            full
        }
    } else {
        full
    }
}

fn format_float_e(
    n: f64,
    prec: usize,
    upper: bool,
    has_plus: bool,
    has_space: bool,
    zero_pad: bool,
    width: Option<usize>,
) -> std::string::String {
    if n.is_nan() {
        let s = if upper { "NAN" } else { "nan" };
        return apply_width_str(s, width, false, false, false, None);
    }
    if n.is_infinite() {
        let s = if n > 0.0 {
            if upper {
                if has_plus {
                    "+INF"
                } else {
                    "INF"
                }
            } else {
                if has_plus {
                    "+inf"
                } else {
                    "inf"
                }
            }
        } else {
            if upper {
                "-INF"
            } else {
                "-inf"
            }
        };
        return apply_width_str(s, width, false, false, false, None);
    }
    // Format using the C-style %e format
    let sign = if n < 0.0 {
        "-"
    } else if has_plus {
        "+"
    } else if has_space {
        " "
    } else {
        ""
    };
    // Use the scientific notation format
    let formatted = format_scientific(n.abs(), prec, upper);
    let full = format!("{}{}", sign, formatted);
    if let Some(w) = width {
        let len = full.len();
        if w > len {
            let padding = w - len;
            if zero_pad {
                format!(
                    "{}{}{}",
                    sign,
                    std::iter::repeat('0')
                        .take(padding)
                        .collect::<std::string::String>(),
                    formatted
                )
            } else {
                format!(
                    "{}{}",
                    std::iter::repeat(' ')
                        .take(padding)
                        .collect::<std::string::String>(),
                    full
                )
            }
        } else {
            full
        }
    } else {
        full
    }
}

fn format_scientific(n: f64, prec: usize, upper: bool) -> std::string::String {
    if n == 0.0 {
        let exp_str = if upper { "E+00" } else { "e+00" };
        if prec == 0 {
            return format!("0{}", exp_str);
        }
        return format!("0.{:0>prec$}{}", "", exp_str, prec = prec);
    }
    let exp = n.log10().floor() as i32;
    let mantissa = n / 10f64.powi(exp);
    let mantissa_str = format!("{:.prec$}", mantissa, prec = prec);
    let exp_char = if upper { 'E' } else { 'e' };
    if exp >= 0 {
        format!("{}{}+{:02}", mantissa_str, exp_char, exp)
    } else {
        format!("{}{}-{:02}", mantissa_str, exp_char, -exp)
    }
}

fn format_float_g(
    n: f64,
    prec: usize,
    upper: bool,
    has_plus: bool,
    has_space: bool,
    zero_pad: bool,
    has_hash: bool,
    width: Option<usize>,
) -> std::string::String {
    if n.is_nan() {
        let s = if upper { "NAN" } else { "nan" };
        return apply_width_str(s, width, false, false, false, None);
    }
    if n.is_infinite() {
        let s = if n > 0.0 {
            if upper {
                if has_plus {
                    "+INF"
                } else {
                    "INF"
                }
            } else {
                if has_plus {
                    "+inf"
                } else {
                    "inf"
                }
            }
        } else {
            if upper {
                "-INF"
            } else {
                "-inf"
            }
        };
        return apply_width_str(s, width, false, false, false, None);
    }

    let prec = if prec == 0 { 1 } else { prec };
    let sign = if n < 0.0 {
        "-"
    } else if has_plus {
        "+"
    } else if has_space {
        " "
    } else {
        ""
    };
    let abs_n = n.abs();

    // Determine if we should use %e or %f style
    let exp = if abs_n == 0.0 {
        0
    } else {
        abs_n.log10().floor() as i32
    };
    let use_exp = exp < -4 || exp >= prec as i32;

    let formatted = if use_exp {
        let e_prec = if prec > 1 { prec - 1 } else { 0 };
        let s = format_scientific(abs_n, e_prec, upper);
        if !has_hash {
            remove_trailing_zeros_exp(&s)
        } else {
            s
        }
    } else {
        let f_prec = if prec > (exp + 1) as usize {
            prec - (exp + 1) as usize
        } else {
            0
        };
        let s = format!("{:.prec$}", abs_n, prec = f_prec);
        if !has_hash {
            remove_trailing_zeros_f(&s)
        } else {
            s
        }
    };

    let full = format!("{}{}", sign, formatted);

    if let Some(w) = width {
        let len = full.len();
        if w > len {
            let padding = w - len;
            if zero_pad {
                format!(
                    "{}{}{}",
                    sign,
                    std::iter::repeat('0')
                        .take(padding)
                        .collect::<std::string::String>(),
                    formatted
                )
            } else {
                format!(
                    "{}{}",
                    std::iter::repeat(' ')
                        .take(padding)
                        .collect::<std::string::String>(),
                    full
                )
            }
        } else {
            full
        }
    } else {
        full
    }
}

fn remove_trailing_zeros_f(s: &str) -> std::string::String {
    if s.contains('.') {
        let s = s.trim_end_matches('0');
        let s = s.trim_end_matches('.');
        s.to_string()
    } else {
        s.to_string()
    }
}

fn remove_trailing_zeros_exp(s: &str) -> std::string::String {
    // Find the 'e' or 'E'
    if let Some(e_pos) = s.find(|c| c == 'e' || c == 'E') {
        let (mantissa, exp) = s.split_at(e_pos);
        let mantissa = if mantissa.contains('.') {
            let m = mantissa.trim_end_matches('0');
            m.trim_end_matches('.')
        } else {
            mantissa
        };
        format!("{}{}", mantissa, exp)
    } else {
        s.to_string()
    }
}

/// Format a float as hexadecimal (%a/%A)
fn format_hex_float(
    n: f64,
    precision: Option<usize>,
    upper: bool,
    has_plus: bool,
    has_space: bool,
    zero_pad: bool,
    width: Option<usize>,
) -> std::string::String {
    if n.is_nan() {
        let s = if upper { "NAN" } else { "nan" };
        return apply_width_str(s, width, false, false, false, None);
    }
    if n.is_infinite() {
        let s = if n > 0.0 {
            if upper {
                if has_plus {
                    "+INF"
                } else {
                    "INF"
                }
            } else {
                if has_plus {
                    "+inf"
                } else {
                    "inf"
                }
            }
        } else {
            if upper {
                "-INF"
            } else {
                "-inf"
            }
        };
        return apply_width_str(s, width, false, false, false, None);
    }

    let sign = if n < 0.0 || n.is_sign_negative() {
        "-"
    } else if has_plus {
        "+"
    } else if has_space {
        " "
    } else {
        ""
    };

    let bits = n.abs().to_bits();

    // Extract biased exponent and mantissa
    let biased_exp = ((bits >> 52) & 0x7FF) as i32;
    let mantissa_bits = bits & 0x000FFFFF_FFFFFFFF;

    let (hex_mant, exp) = if biased_exp == 0 {
        // Subnormal
        (mantissa_bits, -1022i32)
    } else {
        // Normal: leading 1
        (mantissa_bits | 0x0010000000000000, biased_exp - 1023)
    };

    let (prefix0, prefix1) = if upper { ("0X", "P") } else { ("0x", "p") };

    if n.abs() == 0.0 {
        let zero_part = if let Some(prec) = precision {
            if prec == 0 {
                format!("{}{}0{}+0", sign, prefix0, prefix1)
            } else {
                format!(
                    "{}{}0.{:0>prec$}{}+0",
                    sign,
                    prefix0,
                    "",
                    prefix1,
                    prec = prec
                )
            }
        } else {
            format!("{}{}0{}+0", sign, prefix0, prefix1)
        };
        return apply_width_str(&zero_part, width, false, zero_pad, false, None);
    }

    // The mantissa has an implicit leading 1 bit (for normals).
    // We need 13 hex digits for 52 bits of mantissa.
    let full_hex = format!("{:013x}", hex_mant);

    // The leading digit
    let lead_digit = &full_hex[0..1];
    let rest_digits = &full_hex[1..];

    let mantissa_str = if let Some(prec) = precision {
        if prec == 0 {
            lead_digit.to_string()
        } else if prec <= 13 {
            format!("{}.{}", lead_digit, &rest_digits[..prec])
        } else {
            format!(
                "{}.{}{}",
                lead_digit,
                rest_digits,
                std::iter::repeat('0')
                    .take(prec - 13)
                    .collect::<std::string::String>()
            )
        }
    } else {
        // Default: trim trailing zeros from the fractional part
        let trimmed = rest_digits.trim_end_matches('0');
        if trimmed.is_empty() {
            lead_digit.to_string()
        } else {
            format!("{}.{}", lead_digit, trimmed)
        }
    };

    let exp_str = if exp >= 0 {
        format!("{}+{}", prefix1, exp)
    } else {
        format!("{}-{}", prefix1, -exp)
    };

    let hex_part = if upper {
        format!(
            "{}{}{}{}",
            sign,
            prefix0,
            mantissa_str.to_uppercase(),
            exp_str
        )
    } else {
        format!("{}{}{}{}", sign, prefix0, mantissa_str, exp_str)
    };

    apply_width_str(&hex_part, width, false, zero_pad, false, None)
}

/// Format a value with %q (quoted)
fn format_quoted_value<'gc>(
    v: Value<'gc>,
    result: &mut Vec<u8>,
) -> Result<(), std::string::String> {
    match v {
        Value::String(s) => {
            format_quoted_string(s.as_bytes(), result);
        }
        Value::Integer(n) => {
            if n == i64::MIN {
                result.extend_from_slice(b"(math.mininteger)");
            } else {
                result.extend_from_slice(n.to_string().as_bytes());
            }
        }
        Value::Number(n) => {
            if n.is_nan() {
                result.extend_from_slice(b"(0/0)");
            } else if n.is_infinite() {
                if n > 0.0 {
                    result.extend_from_slice(b"1e9999");
                } else {
                    result.extend_from_slice(b"-1e9999");
                }
            } else {
                // Use enough precision to reproduce the exact float (%q format)
                let s = format_float_g(n, 17, false, false, false, false, false, None);
                // Verify round-trip; fall back to full precision if needed
                let out = if s.parse::<f64>().map_or(false, |x| x == n) {
                    s
                } else {
                    format!("{:.17}", n)
                };
                result.extend_from_slice(out.as_bytes());
            }
        }
        Value::Boolean(b) => {
            result.extend_from_slice(if b { b"true" } else { b"false" });
        }
        Value::Nil => {
            result.extend_from_slice(b"nil");
        }
        _ => {
            return Err("no literal for this value type".to_string());
        }
    }
    Ok(())
}

fn format_quoted_string(s: &[u8], result: &mut Vec<u8>) {
    result.push(b'"');
    let mut i = 0;
    while i < s.len() {
        let next_is_digit = i + 1 < s.len() && s[i + 1].is_ascii_digit();
        match s[i] {
            b'"' => {
                result.extend_from_slice(b"\\\"");
            }
            b'\\' => {
                result.extend_from_slice(b"\\\\");
            }
            b'\n' => {
                result.extend_from_slice(b"\\n");
            }
            b'\r' => {
                result.extend_from_slice(b"\\r");
            }
            b'\0' => {
                if next_is_digit {
                    result.extend_from_slice(b"\\000");
                } else {
                    result.extend_from_slice(b"\\0");
                }
            }
            c if c < 32 || c == 127 => {
                if next_is_digit {
                    result.extend_from_slice(format!("\\{:03}", c).as_bytes());
                } else {
                    result.extend_from_slice(format!("\\{}", c).as_bytes());
                }
            }
            c => {
                result.push(c);
            }
        }
        i += 1;
    }
    result.push(b'"');
}

/// Format a pointer value
pub fn format_pointer<'gc>(v: Value<'gc>) -> std::string::String {
    match v {
        Value::String(s) => {
            format!("0x{:x}", s.as_bytes().as_ptr() as usize)
        }
        Value::Table(t) => {
            format!("0x{:x}", Gc::as_ptr(t.into_inner()) as usize)
        }
        Value::Function(f) => match f {
            Function::Closure(c) => format!("0x{:x}", Gc::as_ptr(c.into_inner()) as usize),
            Function::Callback(c) => format!("0x{:x}", Gc::as_ptr(c.into_inner()) as usize),
        },
        Value::Thread(t) => {
            format!("0x{:x}", Gc::as_ptr(t.into_inner()) as usize)
        }
        Value::UserData(u) => {
            format!("0x{:x}", Gc::as_ptr(u.into_inner()) as usize)
        }
        _ => "(null)".to_string(),
    }
}
