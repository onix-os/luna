//! The `os` library. Times are UTC throughout — luna ships no time-zone database.

use luna::{Closure, Executor, ExternError};

fn eval(source: &str) -> Result<bool, ExternError> {
    let mut lua = common::full();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<bool>(&executor)
}

mod common;

#[test]
fn tmpname_reserves_distinct_empty_files() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local a, b = os.tmpname(), os.tmpname()
        local fa, fb = io.open(a, "r"), io.open(b, "r")
        local valid = a ~= b and fa ~= nil and fb ~= nil
        if fa then valid = valid and fa:read("a") == ""; fa:close() end
        if fb then valid = valid and fb:read("a") == ""; fb:close() end
        os.remove(a)
        os.remove(b)
        return valid
        "#
    )?);
    Ok(())
}

#[test]
fn tmpname_is_distinct_across_concurrent_states() {
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut lua = common::full();
                let mut paths = Vec::new();
                barrier.wait();
                for _ in 0..32 {
                    let executor = lua
                        .try_enter(|ctx| {
                            let closure = Closure::load(ctx, None, &b"return os.tmpname()"[..])?;
                            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
                        })
                        .unwrap();
                    paths.push(lua.execute::<String>(&executor).unwrap());
                }
                paths
            })
        })
        .collect();
    let paths: Vec<_> = workers
        .into_iter()
        .flat_map(|worker| worker.join().unwrap())
        .collect();
    let unique = paths.iter().collect::<std::collections::HashSet<_>>().len();
    let reserved = paths.iter().all(|path| std::fs::metadata(path).is_ok());
    for path in &paths {
        std::fs::remove_file(path).unwrap();
    }
    assert_eq!(unique, 256);
    assert!(reserved);
}

#[test]
fn time_round_trips_through_a_table() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        -- 2000-01-01 00:00:00 UTC is 946684800.
        local t = os.time{ year = 2000, month = 1, day = 1, hour = 0, min = 0, sec = 0 }
        return t == 946684800
    "#
    )?);
    Ok(())
}

#[test]
fn date_formats_a_known_timestamp() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return os.date("!%Y-%m-%d %H:%M:%S", 946684800) == "2000-01-01 00:00:00"
    "#
    )?);
    Ok(())
}

/// A leap day, which is where naive date arithmetic usually breaks.
#[test]
fn date_handles_leap_years() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local leap = os.time{ year = 2024, month = 2, day = 29, hour = 0, min = 0, sec = 0 }
        return os.date("!%Y-%m-%d", leap) == "2024-02-29"
            and os.date("!%j", leap) == "060"
    "#
    )?);
    Ok(())
}

/// A date before the epoch, where the arithmetic has to handle negative days.
#[test]
fn date_handles_pre_epoch_times() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local t = os.time{ year = 1969, month = 7, day = 20, hour = 20, min = 17, sec = 0 }
        return t < 0 and os.date("!%Y-%m-%d %H:%M", t) == "1969-07-20 20:17"
    "#
    )?);
    Ok(())
}

#[test]
fn date_returns_a_table_for_star_t() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        local t = os.date("*t", 946684800)
        return t.year == 2000 and t.month == 1 and t.day == 1
            and t.hour == 0 and t.min == 0 and t.sec == 0
            and t.wday == 7 and t.yday == 1 and t.isdst == false
    "#
    )?);
    Ok(())
}

#[test]
fn weekday_names_line_up() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        -- 2000-01-01 was a Saturday.
        return os.date("!%A", 946684800) == "Saturday"
            and os.date("!%a", 946684800) == "Sat"
            and os.date("!%B", 946684800) == "January"
    "#
    )?);
    Ok(())
}

#[test]
fn getenv_reads_the_environment() -> Result<(), ExternError> {
    std::env::set_var("LUNA_OS_TEST", "present");
    assert!(eval(
        r#"
        return os.getenv("LUNA_OS_TEST") == "present"
            and os.getenv("LUNA_OS_TEST_MISSING_VAR") == nil
    "#
    )?);
    Ok(())
}

#[test]
fn clock_and_difftime_work() -> Result<(), ExternError> {
    assert!(eval(
        r#"
        return type(os.clock()) == "number"
            and os.difftime(10, 4) == 6
            and type(os.time()) == "number"
    "#
    )?);
    Ok(())
}

#[test]
fn remove_and_rename_touch_the_filesystem() -> Result<(), ExternError> {
    let dir = std::env::temp_dir().join("luna_os_test");
    std::fs::create_dir_all(&dir).unwrap();
    let a = dir.join("a.txt").display().to_string();
    let b = dir.join("b.txt").display().to_string();
    std::fs::write(&a, "x").unwrap();

    assert!(eval(&format!(
        r#"
        local ok = os.rename("{a}", "{b}")
        local removed = os.remove("{b}")
        local missing, err = os.remove("{b}")
        return ok == true and removed == true and missing == nil and type(err) == "string"
    "#
    ))?);

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
