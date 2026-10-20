use std::cell::Cell;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
}

pub(in crate::jit) fn enabled() -> bool {
    ENABLED.get()
}

pub(in crate::jit) fn with_enabled<R>(enabled: bool, run: impl FnOnce() -> R) -> R {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            ENABLED.set(self.0);
        }
    }
    let _reset = Reset(ENABLED.replace(enabled));
    run()
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod timing;
