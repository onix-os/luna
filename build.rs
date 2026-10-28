fn main() {
    println!("cargo::rustc-check-cfg=cfg(luna_jit_embedded_worker)");
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_JIT_WORKER_EMBEDDED").is_none()
        || std::env::var_os("CARGO_CFG_MIRI").is_some()
        || std::env::var_os("MIRI_SYSROOT").is_some()
    {
        return;
    }
    let target = std::env::var("TARGET").unwrap();
    if !matches!(
        target.as_str(),
        "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl" | "aarch64-unknown-linux-gnu"
    ) {
        return;
    }
    println!("cargo::rerun-if-changed=src");
    println!("cargo::rerun-if-env-changed=LUNA_WORKER_GLIBC_STATIC");
    for (output, input) in [
        ("LUNA_EMBED_TARGET", "TARGET"),
        ("LUNA_EMBED_HOST", "HOST"),
        ("LUNA_EMBED_RUSTC", "RUSTC"),
        ("LUNA_EMBED_LINKER", "RUSTC_LINKER"),
        ("LUNA_EMBED_GLIBC_STATIC", "LUNA_WORKER_GLIBC_STATIC"),
    ] {
        println!(
            "cargo::rustc-env={output}={}",
            std::env::var(input).unwrap_or_default()
        );
    }
    println!("cargo::rustc-cfg=luna_jit_embedded_worker");
}
