#![cfg(feature = "jit")]

mod common;

use luna::{Closure, Executor, JitMode};

#[test]
fn mode_workers_enforce_force_refusals_and_report_exclusions() {
    for (mode, case) in [
        ("off", "normal"),
        ("auto", "normal"),
        ("force", "normal"),
        ("force", "empty"),
        ("force", "same_entry"),
        ("force", "unexpected"),
        ("force", "allowed"),
        ("force", "wrong_reason"),
    ] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "preparation_worker",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("LUNA_TEST_JIT_MODE", mode)
            .env("LUNA_PREPARATION_CASE", case)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let supported = luna::JitCapabilities::current().supported_target;
        let refused = supported && ["unexpected", "wrong_reason"].contains(&case);
        assert_eq!(output.status.success(), !refused, "{mode}/{case}: {text}");
        if refused {
            assert!(
                text.contains("unexpected forced preparation refusal: native image size"),
                "{text}"
            );
        } else {
            assert!(text.contains("PREPARATION_VERIFIED"), "{text}");
        }
        if !supported && mode == "force" {
            assert!(
                text.contains("JIT_FORCE_EXCLUDED reason=unsupported_target"),
                "{text}"
            );
        } else if case == "allowed" {
            assert!(
                text.contains("JIT_FORCE_EXCLUDED reason=\"native image size\""),
                "{text}"
            );
        }
    }
}

#[test]
fn preparation_worker() {
    let Ok(case) = std::env::var("LUNA_PREPARATION_CASE") else {
        return;
    };
    let mode = std::env::var("LUNA_TEST_JIT_MODE").unwrap();
    let mut lua = common::empty();
    let supported = lua.jit_capabilities().supported_target;
    if ["empty", "same_entry"].contains(&case.as_str()) {
        let value = lua.enter(|ctx| {
            if case == "empty" {
                return 42;
            }
            let closure = Closure::load(ctx, None, b"return 42").unwrap();
            let executor = Executor::start(ctx, closure.into(), ());
            while !executor.step(ctx, &mut luna::Fuel::with(100)).unwrap() {}
            executor.take_result::<i64>(ctx).unwrap().unwrap()
        });
        assert_eq!(value, 42);
        assert_eq!(lua.jit_stats().native_instructions, 0);
        let report = lua.preparation_report();
        assert_eq!(
            report.installed_regions,
            u64::from(supported && case == "same_entry")
        );
        assert_eq!(
            report.empty_batches,
            u64::from(supported && case == "empty")
        );
        assert_eq!(report.unsupported_skips, u64::from(!supported));
        println!("PREPARATION_VERIFIED {report:?}");
        return;
    }
    if case != "normal" {
        let mut config = lua.jit_config();
        config.max_code_bytes = 1;
        config.max_compile_attempts = 1;
        lua.set_jit_config(config).unwrap();
    }
    if case == "allowed" {
        lua.allow_jit_resource_refusal("native image size");
    } else if case == "wrong_reason" {
        lua.allow_jit_resource_refusal("prototype snapshot");
    }
    let executor = lua.enter(|ctx| {
        let closure = Closure::load(ctx, None, b"local x=40 return x+2").unwrap();
        ctx.stash(Executor::start(ctx, closure.into(), ()))
    });
    let report = lua.preparation_report();
    if mode == "force" && supported {
        assert_eq!(report.calls, 1);
        if case == "allowed" {
            assert_eq!(report.resource_refusals, 1);
            assert_eq!(report.last_resource_refusal, Some("native image size"));
            assert_eq!(report.installed_regions, 0);
        } else {
            assert_eq!(report.installed_regions, 1);
            assert_eq!(report.resource_refusals, 0);
        }
    } else if mode == "force" {
        assert_eq!(report.unsupported_skips, 1);
        assert_eq!(report.calls, 0);
        assert_eq!(report.installed_regions, 0);
    } else {
        assert_eq!(report, common::PreparationReport::default());
    }
    assert_eq!(lua.execute::<i64>(&executor).unwrap(), 42);
    assert_eq!(
        lua.jit_stats().native_instructions > 0,
        mode == "force" && supported && case == "normal"
    );
    assert_eq!(lua.jit_config().mode == JitMode::Off, mode == "off");
    lua.enter(|_| ());
    if mode == "force" && supported {
        assert_eq!(lua.preparation_report().empty_batches, 1);
    }
    println!("PREPARATION_VERIFIED {report:?}");
}
