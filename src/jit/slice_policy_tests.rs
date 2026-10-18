use super::{JitMode, Runtime};

fn observed(runtime: &Runtime) -> (bool, bool) {
    runtime.slice_policy()
}

#[test]
fn slice_policy_tracks_live_mode_and_pair_admission_without_effects() {
    let runtime = Runtime::new();
    let alias = runtime.clone();
    for enabled in [false, true, false, true] {
        #[cfg(all(
            not(miri),
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        runtime.test_call_pairs(enabled);
        let pairs = enabled
            && cfg!(all(
                not(miri),
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ));
        for mode in [
            JitMode::Off,
            JitMode::Auto,
            JitMode::Off,
            JitMode::Auto,
            JitMode::Off,
        ] {
            runtime.0.borrow_mut().config.mode = mode;
            let before = runtime.0.borrow().stats;
            let expected = (mode == JitMode::Auto, mode == JitMode::Auto && pairs);
            for owner in [&runtime, &alias] {
                assert_eq!(observed(owner), expected);
                let manager = owner.0.try_borrow_mut().unwrap();
                assert_eq!(manager.stats, before);
                assert_eq!(manager.config.mode, mode);
            }
        }
    }
}
