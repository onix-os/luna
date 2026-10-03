#![allow(dead_code)]

use luna::{Context, Error, ExternError, Lua as InnerLua};

pub struct Lua {
    inner: InnerLua,
    #[cfg(feature = "jit")]
    preparation: PreparationReport,
    #[cfg(feature = "jit")]
    expected_resource_refusal: Option<&'static str>,
}

#[cfg(feature = "jit")]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PreparationReport {
    pub calls: u64,
    pub installed_regions: u64,
    pub empty_batches: u64,
    pub resource_refusals: u64,
    pub unsupported_skips: u64,
    pub last_resource_refusal: Option<&'static str>,
}

impl std::ops::Deref for Lua {
    type Target = InnerLua;
    fn deref(&self) -> &InnerLua {
        &self.inner
    }
}

impl std::ops::DerefMut for Lua {
    fn deref_mut(&mut self) -> &mut InnerLua {
        &mut self.inner
    }
}

impl Lua {
    pub fn enter<F, T>(&mut self, f: F) -> T
    where
        F: for<'gc> FnOnce(Context<'gc>) -> T,
    {
        let result = self.inner.enter(f);
        self.prepare();
        result
    }

    pub fn try_enter<F, T>(&mut self, f: F) -> Result<T, ExternError>
    where
        F: for<'gc> FnOnce(Context<'gc>) -> Result<T, Error<'gc>>,
    {
        let result = self.inner.try_enter(f);
        self.prepare();
        result
    }

    fn prepare(&mut self) {
        #[cfg(feature = "jit")]
        if std::env::var("LUNA_TEST_JIT_MODE").as_deref() == Ok("force") {
            if !self.inner.jit_capabilities().supported_target {
                if self.preparation.unsupported_skips == 0 {
                    eprintln!("JIT_FORCE_EXCLUDED reason=unsupported_target");
                }
                self.preparation.unsupported_skips += 1;
                return;
            }
            self.preparation.calls += 1;
            let before = self.inner.jit_stats().installed_regions;
            let result = self.inner.prepare_jit();
            self.preparation.installed_regions += self.inner.jit_stats().installed_regions - before;
            match result {
                Ok(0) => self.preparation.empty_batches += 1,
                Ok(_) => {}
                Err(luna::JitError::ResourceLimit(reason)) => {
                    self.preparation.resource_refusals += 1;
                    self.preparation.last_resource_refusal = Some(reason);
                    assert_eq!(
                        self.expected_resource_refusal,
                        Some(reason),
                        "unexpected forced preparation refusal: {reason}"
                    );
                    eprintln!("JIT_FORCE_EXCLUDED reason={reason:?}");
                }
                Err(error) => panic!("forced preparation failed: {error}"),
            }
        }
    }

    #[cfg(feature = "jit")]
    pub fn preparation_report(&self) -> PreparationReport {
        self.preparation
    }

    #[cfg(feature = "jit")]
    pub fn allow_jit_resource_refusal(&mut self, reason: &'static str) {
        self.expected_resource_refusal = Some(reason);
    }
}

pub fn empty() -> Lua {
    configure(InnerLua::empty())
}

pub fn core() -> Lua {
    configure(InnerLua::core())
}

pub fn full() -> Lua {
    configure(InnerLua::full())
}

pub fn default() -> Lua {
    configure(InnerLua::default())
}

fn configure(lua: InnerLua) -> Lua {
    #[cfg(feature = "jit")]
    {
        use luna::{JitConfig, JitMode};
        let mut lua = lua;
        let mut config = JitConfig::default();
        match std::env::var("LUNA_TEST_JIT_MODE")
            .as_deref()
            .unwrap_or("off")
        {
            "off" => {}
            "auto" => config.mode = JitMode::Auto,
            "force" => {
                config.mode = JitMode::Auto;
                config.hot_threshold = 1;
            }
            other => panic!("invalid LUNA_TEST_JIT_MODE: {other}"),
        }
        lua.set_jit_config(config).unwrap();
        Lua {
            inner: lua,
            preparation: PreparationReport::default(),
            expected_resource_refusal: None,
        }
    }
    #[cfg(not(feature = "jit"))]
    Lua { inner: lua }
}
