use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::{Callback, CallbackReturn, Context, Error, IntoValue, String, Table, Value};

/// When this process started, for `os.clock`.
pub(super) fn process_start() -> Instant {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// Days since the epoch to a civil (year, month, day).
///
/// Hinnant's algorithm, shifted to treat March as the first month so that the leap day lands at
/// the end of the cycle and needs no special case. Written out here rather than taken from a date
/// crate: it is fixed arithmetic that will never need updating, and luna's dependency list is
/// deliberately short.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The inverse of [`civil_from_days`].
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// A broken-down time, always UTC.
struct Broken {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    min: u32,
    sec: u32,
    wday: u32,
    yday: u32,
}

fn break_down(timestamp: i64) -> Broken {
    let days = timestamp.div_euclid(86_400);
    let secs = timestamp.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Broken {
        year,
        month,
        day,
        hour: (secs / 3600) as u32,
        min: (secs % 3600 / 60) as u32,
        sec: (secs % 60) as u32,
        // 1970-01-01 was a Thursday, and Lua counts Sunday as 1.
        wday: ((days + 4).rem_euclid(7) + 1) as u32,
        yday: (days - days_from_civil(year, 1, 1) + 1) as u32,
    }
}

const DAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// A `strftime` subset over a broken-down time.
fn format_date(format: &str, t: &Broken) -> std::string::String {
    let mut out = std::string::String::new();
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            None => out.push('%'),
            Some(d) => {
                let day = DAY_NAMES[(t.wday - 1) as usize];
                let month = MONTH_NAMES[(t.month - 1) as usize];
                match d {
                    'a' => out.push_str(&day[..3]),
                    'A' => out.push_str(day),
                    'b' | 'h' => out.push_str(&month[..3]),
                    'B' => out.push_str(month),
                    'c' => out.push_str(&format!(
                        "{} {} {:2} {:02}:{:02}:{:02} {}",
                        &day[..3],
                        &month[..3],
                        t.day,
                        t.hour,
                        t.min,
                        t.sec,
                        t.year
                    )),
                    'd' => out.push_str(&format!("{:02}", t.day)),
                    'e' => out.push_str(&format!("{:2}", t.day)),
                    'H' => out.push_str(&format!("{:02}", t.hour)),
                    'I' => out.push_str(&format!(
                        "{:02}",
                        if t.hour % 12 == 0 { 12 } else { t.hour % 12 }
                    )),
                    'j' => out.push_str(&format!("{:03}", t.yday)),
                    'm' => out.push_str(&format!("{:02}", t.month)),
                    'M' => out.push_str(&format!("{:02}", t.min)),
                    'n' => out.push('\n'),
                    'p' => out.push_str(if t.hour < 12 { "AM" } else { "PM" }),
                    'S' => out.push_str(&format!("{:02}", t.sec)),
                    't' => out.push('\t'),
                    'w' => out.push_str(&format!("{}", t.wday - 1)),
                    'x' => {
                        out.push_str(&format!("{:02}/{:02}/{:02}", t.month, t.day, t.year % 100))
                    }
                    'X' => out.push_str(&format!("{:02}:{:02}:{:02}", t.hour, t.min, t.sec)),
                    'y' => out.push_str(&format!("{:02}", t.year.rem_euclid(100))),
                    'Y' => out.push_str(&format!("{}", t.year)),
                    // Always UTC — see `load_os`.
                    'Z' => out.push_str("UTC"),
                    '%' => out.push('%'),
                    other => {
                        out.push('%');
                        out.push(other);
                    }
                }
            }
        }
    }
    out
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Loads the `os` library.
///
/// **Times are UTC.** luna has no time-zone database and will not link one, so `os.date("%H")`
/// and `os.date("*t")` report UTC rather than local time, and the `!` prefix is accepted and
/// ignored. This is a deliberate limit, not an oversight: getting local time right means shipping
/// or reading a zone database, and a wrong local time is worse than a documented UTC one.
pub fn load_os<'gc>(ctx: Context<'gc>) {
    // Take the start time now, so `os.clock` measures from library load rather than first call.
    let _ = process_start();

    let os = Table::new(&ctx);

    os.set_field(
        ctx,
        "time",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let arg: Option<Value> = stack.consume(ctx)?;
            let seconds = match arg {
                None | Some(Value::Nil) => now_seconds(),
                Some(Value::Table(t)) => {
                    // PUC-Rio's fields are C ints and it rejects anything that does not fit with
                    // "field 'x' is out-of-bound". Bounding them here is also what keeps the day
                    // and second arithmetic below inside i64: a year of 1e18 otherwise overflows
                    // `era * 146_097`, which panics wherever overflow checks are on and silently
                    // wraps where they are not.
                    let field = |name: &'static str, default: i64| -> Result<i64, Error<'gc>> {
                        let value = t.get_value(ctx, name.into_value(ctx));
                        if value.is_nil() {
                            return Ok(default);
                        }
                        match value.to_integer() {
                            Some(i) if i32::try_from(i).is_ok() => Ok(i),
                            _ => Err(format!("field '{name}' is out-of-bound")
                                .into_value(ctx)
                                .into()),
                        }
                    };
                    let days = days_from_civil(
                        field("year", 1970)?,
                        field("month", 1)? as u32,
                        field("day", 1)? as u32,
                    );
                    days * 86_400
                        + field("hour", 12)? * 3600
                        + field("min", 0)? * 60
                        + field("sec", 0)?
                }
                Some(_) => {
                    return Err("bad argument #1 to 'time' (table expected)"
                        .into_value(ctx)
                        .into())
                }
            };
            stack.replace(ctx, seconds);
            Ok(CallbackReturn::Return)
        }),
    );

    os.set_field(
        ctx,
        "clock",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            stack.replace(ctx, process_start().elapsed().as_secs_f64());
            Ok(CallbackReturn::Return)
        }),
    );

    os.set_field(
        ctx,
        "date",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (format, time): (Option<String>, Option<i64>) = stack.consume(ctx)?;
            let format = format
                .map(|f| f.display_lossy().to_string())
                .unwrap_or_else(|| "%c".to_owned());
            let timestamp = time.unwrap_or_else(now_seconds);

            // `!` asks for UTC, which is all luna has; it is accepted so scripts written for it
            // keep working.
            let format = format.strip_prefix('!').unwrap_or(&format).to_owned();
            let broken = break_down(timestamp);

            if format == "*t" {
                let t = Table::new(&ctx);
                t.set_field(ctx, "year", broken.year);
                t.set_field(ctx, "month", broken.month as i64);
                t.set_field(ctx, "day", broken.day as i64);
                t.set_field(ctx, "hour", broken.hour as i64);
                t.set_field(ctx, "min", broken.min as i64);
                t.set_field(ctx, "sec", broken.sec as i64);
                t.set_field(ctx, "wday", broken.wday as i64);
                t.set_field(ctx, "yday", broken.yday as i64);
                t.set_field(ctx, "isdst", false);
                stack.replace(ctx, t);
            } else {
                let formatted = format_date(&format, &broken);
                stack.replace(ctx, ctx.intern(formatted.as_bytes()));
            }
            Ok(CallbackReturn::Return)
        }),
    );

    os.set_field(
        ctx,
        "difftime",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (t2, t1): (f64, Option<f64>) = stack.consume(ctx)?;
            stack.replace(ctx, t2 - t1.unwrap_or(0.0));
            Ok(CallbackReturn::Return)
        }),
    );

    os.set_field(
        ctx,
        "getenv",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let name: String = stack.consume(ctx)?;
            match std::env::var(name.display_lossy().to_string()) {
                Ok(value) => stack.replace(ctx, ctx.intern(value.as_bytes())),
                Err(_) => stack.replace(ctx, Value::Nil),
            }
            Ok(CallbackReturn::Return)
        }),
    );

    os.set_field(
        ctx,
        "remove",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let path: String = stack.consume(ctx)?;
            let path = path.display_lossy().to_string();
            match std::fs::remove_file(&path) {
                Ok(()) => stack.replace(ctx, true),
                Err(err) => {
                    let msg = ctx.intern(format!("{path}: {err}").as_bytes());
                    stack.replace(ctx, (Value::Nil, msg));
                }
            }
            Ok(CallbackReturn::Return)
        }),
    );

    os.set_field(
        ctx,
        "rename",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (from, to): (String, String) = stack.consume(ctx)?;
            let (from, to) = (
                from.display_lossy().to_string(),
                to.display_lossy().to_string(),
            );
            match std::fs::rename(&from, &to) {
                Ok(()) => stack.replace(ctx, true),
                Err(err) => {
                    let msg = ctx.intern(format!("{from} -> {to}: {err}").as_bytes());
                    stack.replace(ctx, (Value::Nil, msg));
                }
            }
            Ok(CallbackReturn::Return)
        }),
    );

    os.set_field(
        ctx,
        "tmpname",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            // Nothing is created, matching PUC-Rio, so the name is only useful to a caller that
            // goes on to create the file itself.
            let unique = process_start().elapsed().as_nanos();
            let path = std::env::temp_dir().join(format!("lua_{unique:x}"));
            stack.replace(ctx, ctx.intern(path.display().to_string().as_bytes()));
            Ok(CallbackReturn::Return)
        }),
    );

    // Pure Rust via `std::process::Command` — this is a sandbox *policy* decision, not a
    // capability limit. A host that does not want scripts spawning processes loads `os` and then
    // removes this field, the same way it would remove `getenv`.
    os.set_field(
        ctx,
        "execute",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let command: Option<String> = stack.consume(ctx)?;
            let Some(command) = command else {
                // With no argument, report whether a shell is available at all.
                stack.replace(ctx, true);
                return Ok(CallbackReturn::Return);
            };
            match std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(command.display_lossy().to_string())
                .status()
            {
                Ok(status) if status.success() => {
                    stack.replace(ctx, (true, ctx.intern(b"exit"), 0));
                }
                Ok(status) => {
                    let code = status.code().unwrap_or(-1) as i64;
                    stack.replace(ctx, (Value::Nil, ctx.intern(b"exit"), code));
                }
                Err(err) => {
                    let msg = ctx.intern(err.to_string().as_bytes());
                    stack.replace(ctx, (Value::Nil, msg));
                }
            }
            Ok(CallbackReturn::Return)
        }),
    );

    os.set_field(
        ctx,
        "exit",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let code: Option<Value> = stack.consume(ctx)?;
            let code = match code {
                None | Some(Value::Nil) | Some(Value::Boolean(true)) => 0,
                Some(Value::Boolean(false)) => 1,
                Some(v) => v.to_integer().unwrap_or(0) as i32,
            };
            std::process::exit(code);
        }),
    );

    ctx.set_global("os", os);
}
