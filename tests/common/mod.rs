#![allow(dead_code)]

use luna::{Context, Error, ExternError, Lua as InnerLua};

pub struct Lua(InnerLua);

impl std::ops::Deref for Lua {
    type Target = InnerLua;
    fn deref(&self) -> &InnerLua {
        &self.0
    }
}

impl std::ops::DerefMut for Lua {
    fn deref_mut(&mut self) -> &mut InnerLua {
        &mut self.0
    }
}

impl Lua {
    pub fn enter<F, T>(&mut self, f: F) -> T
    where
        F: for<'gc> FnOnce(Context<'gc>) -> T,
    {
        let result = self.0.enter(f);
        self.prepare();
        result
    }

    pub fn try_enter<F, T>(&mut self, f: F) -> Result<T, ExternError>
    where
        F: for<'gc> FnOnce(Context<'gc>) -> Result<T, Error<'gc>>,
    {
        let result = self.0.try_enter(f);
        self.prepare();
        result
    }

    fn prepare(&mut self) {
        #[cfg(feature = "jit")]
        if std::env::var("LUNA_TEST_JIT_MODE").as_deref() == Ok("force")
            && self.0.jit_capabilities().supported_target
        {
            if let Err(luna::JitError::Compilation(error)) = self.0.prepare_jit() {
                panic!("forced preparation failed: {error}");
            }
        }
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
        Lua(lua)
    }
    #[cfg(not(feature = "jit"))]
    Lua(lua)
}
