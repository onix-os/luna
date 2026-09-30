use allocator_api2::SliceExt;
use luna::{
    opcode::{OpCode, Operation},
    types::{RegisterIndex, VarCount},
    Closure, Executor, ExternError, Lua,
};
use ottavino_gc_arena::allocator_api::MetricsAlloc;

#[test]
fn load_nil_can_clear_the_last_register() -> Result<(), ExternError> {
    let mut lua = Lua::empty();
    let executor = lua.try_enter(|ctx| {
        let mut proto = luna::FunctionPrototype::compile(ctx, "register-boundary", b"return 42")?;
        proto.stack_size = 256;
        let first = proto.opcodes[0].decode();
        let Operation::LoadConstant { constant, .. } = first else {
            panic!("missing numeric constant")
        };
        let operations = [
            Operation::LoadConstant {
                dest: RegisterIndex(255),
                constant,
            },
            Operation::LoadNil {
                dest: RegisterIndex(255),
                count: 1,
            },
            Operation::Return {
                start: RegisterIndex(255),
                count: VarCount::constant(1),
            },
        ];
        let opcodes: Vec<_> = operations.into_iter().map(OpCode::encode).collect();
        proto.opcodes =
            SliceExt::to_vec_in(opcodes.as_slice(), MetricsAlloc::new(&ctx)).into_boxed_slice();
        let closure = Closure::new(&ctx, proto, Some(ctx.globals())).unwrap();
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    assert_eq!(lua.execute::<Option<i64>>(&executor)?, None);
    Ok(())
}

#[test]
fn source_can_initialize_all_256_registers_and_return_no_values() -> Result<(), ExternError> {
    let names: Vec<_> = (0..254).map(|index| format!("v{index}")).collect();
    let script = format!("local {} local last_a,last_b return", names.join(","));
    for native in [false, true] {
        #[cfg(not(feature = "jit"))]
        if native {
            continue;
        }
        let mut lua = Lua::empty();
        #[cfg(feature = "jit")]
        if native {
            lua.set_jit_config(luna::JitConfig {
                mode: luna::JitMode::Auto,
                ..luna::JitConfig::default()
            })
            .unwrap();
            if !lua.jit_capabilities().supported_target {
                continue;
            }
        }
        let executor = lua.try_enter(|ctx| {
            let closure = Closure::load(ctx, Some("full-registers"), script.as_bytes())?;
            assert_eq!(closure.prototype().stack_size, 256);
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })?;
        #[cfg(feature = "jit")]
        if native {
            lua.prepare_jit().unwrap();
        }
        lua.execute::<()>(&executor)?;
        #[cfg(feature = "jit")]
        if native {
            assert!(lua.jit_stats().native_instructions >= 2);
        }
    }
    Ok(())
}
