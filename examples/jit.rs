use luna::{Closure, Executor, JitConfig, JitMode, Lua};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut lua = Lua::core();
    let supported_target = lua.jit_capabilities().supported_target;
    if supported_target {
        lua.set_jit_config(JitConfig {
            mode: JitMode::Auto,
            ..JitConfig::default()
        })?;
    }
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(
            ctx,
            Some("native-example"),
            b"local sum=0 for i=1,100000 do sum=sum+i end return sum",
        )?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    let prepared = lua.prepare_jit()?;
    let result: i64 = lua.execute(&executor)?;
    assert_eq!(result, 5_000_050_000);
    let stats = lua.jit_stats();
    if supported_target {
        assert!(prepared > 0);
        assert!(stats.native_instructions > 0);
        assert!(stats.code_bytes > 0);
        assert!(stats.code_requested_bytes > 0);
        assert!(stats.code_requested_bytes <= stats.code_bytes);
    } else {
        assert_eq!(prepared, 0);
        assert_eq!(stats.native_instructions, 0);
        assert_eq!(stats.code_bytes, 0);
        assert_eq!(stats.code_requested_bytes, 0);
    }
    println!(
        "result={result} prepared={prepared} native_instructions={} native_memory_bytes={} native_requested_bytes={}",
        stats.native_instructions, stats.code_bytes, stats.code_requested_bytes
    );
    Ok(())
}
