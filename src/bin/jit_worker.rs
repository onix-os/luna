#[cfg(luna_jit_native)]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(luna::jit::__worker_bootstrap_main())
}

#[cfg(not(luna_jit_native))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(64)
}
