use std::{error::Error, hint::black_box};

use luna::{Callback, CallbackReturn, Closure, Executor, Lua, StashedClosure};

#[path = "jit_support/workloads.rs"]
#[allow(dead_code)]
mod workloads;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn setup(mode: &str) -> Result<(Lua, StashedClosure)> {
    let mut lua = Lua::core();
    #[cfg(feature = "jit")]
    if mode == "auto" {
        lua.set_jit_config(luna::JitConfig {
            mode: luna::JitMode::Auto,
            ..Default::default()
        })?;
    }
    #[cfg(not(feature = "jit"))]
    if mode != "off" {
        return Err("Auto requires the jit feature".into());
    }
    lua.enter(|ctx| {
        let increment = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let value: i64 = stack.consume(ctx)?;
            stack.replace(ctx, value.wrapping_add(1));
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("host_increment", increment);
    });
    let closure = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("oslo-row-predicate"), workloads::PREDICATE_SOURCE)?;
        Ok(ctx.stash(closure))
    })?;
    Ok((lua, closure))
}

#[inline(never)]
fn rows(lua: &mut Lua, closure: &StashedClosure) -> Result<usize> {
    let mut selected = 0;
    for row in 0..10000 {
        let executor = lua.enter(|ctx| {
            ctx.set_global(
                "free",
                if row % 2 == 0 {
                    500000000i64
                } else {
                    1500000000i64
                },
            );
            ctx.stash(Executor::start(ctx, ctx.fetch(closure).into(), ()))
        });
        selected += usize::from(lua.execute::<bool>(&executor)?);
    }
    Ok(selected)
}

fn options(args: &[String]) -> Result<(&str, usize)> {
    if args.len() != 2 || !matches!(args[0].as_str(), "off" | "auto") {
        return Err("usage: jit_oslo_profile off|auto ITERATIONS".into());
    }
    let iterations = args[1].parse()?;
    if !(1..=1000).contains(&iterations) {
        return Err("iterations must be 1..=1000".into());
    }
    Ok((&args[0], iterations))
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let (mode, iterations) = options(&args)?;
    let (mut lua, closure) = setup(mode)?;
    for pass in 0..iterations + 2 {
        assert_eq!(black_box(rows(&mut lua, &closure)?), 5000);
        println!(
            "pass={pass} rows=10000 selected=5000 warmup={}",
            usize::from(pass < 2)
        );
    }
    #[cfg(feature = "jit")]
    {
        let stats = lua.jit_stats();
        if mode == "off" {
            assert_eq!(stats.compilation_requests, 0);
            assert_eq!(stats.native_entries, 0);
            assert_eq!(stats.native_instructions, 0);
        } else {
            assert!(stats.native_instructions > 0);
        }
        println!("stats={stats:?}");
    }
    println!("oslo_profile_protocol=1 jit_compiled={} mode={mode} iterations={iterations} warmups=2 verified=1 elapsed_acceptance=false", usize::from(cfg!(feature = "jit")));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_batch_matches_cost_predicate() {
        let (mut lua, closure) = setup("off").unwrap();
        assert_eq!(rows(&mut lua, &closure).unwrap(), 5000);
        assert_eq!(rows(&mut lua, &closure).unwrap(), 5000);
        #[cfg(feature = "jit")]
        assert_eq!(lua.jit_stats().native_instructions, 0);
    }

    #[test]
    fn options_reject_invalid_modes_counts_and_extra_arguments() {
        for args in [
            vec![],
            vec!["off"],
            vec!["force", "3"],
            vec!["off", "0"],
            vec!["off", "1001"],
            vec!["off", "x"],
            vec!["off", "3", "extra"],
        ] {
            assert!(options(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err());
        }
        for mode in ["off", "auto"] {
            for count in [1, 1000] {
                let args = [mode.to_owned(), count.to_string()];
                assert_eq!(options(&args).unwrap(), (mode, count));
            }
        }
    }

    #[cfg(feature = "jit")]
    #[test]
    fn auto_rows_execute_native_code() {
        let (mut lua, closure) = setup("auto").unwrap();
        assert_eq!(rows(&mut lua, &closure).unwrap(), 5000);
        assert!(lua.jit_stats().native_instructions > 0);
    }
}
