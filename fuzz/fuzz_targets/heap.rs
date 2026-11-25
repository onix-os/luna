#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    luna_fuzz::check(&luna_fuzz::heap(data), data, true);
});
