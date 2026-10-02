#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    luna_fuzz::check(&luna_fuzz::scalar(data), data, false);
});
