use super::*;
use crate::{FunctionPrototype, Lua};

fn source(text: &str) -> super::super::ir::Snapshot {
    Lua::empty().enter(|ctx| {
        let prototype =
            FunctionPrototype::compile(ctx, "array-transport", text.as_bytes()).unwrap();
        super::super::ir::Snapshot::new(&prototype, 4096, 2 * 1024 * 1024).unwrap()
    })
}

#[test]
fn array_transport_recognition_preserves_source_and_charged_storage() {
    for (text, expected) in [
        ("local t={} for i=1,100 do t[i]=i end return t", true),
        (
            "local t=... local s=0 for i=100,1,-1 do s=s+t[i] end return s",
            true,
        ),
        ("local t=... for i=1,100 do t[i]=t[i]+3 end return t", true),
        (
            "sink(0) local t=... for i=1,100 do t[i]=false end return t",
            true,
        ),
        ("local t=... for i=1.0,100.0 do t[i]=i end return t", true),
        ("local t={} for i=1,100 do t[1]=i end return t", false),
        ("local t={} for i=1,100 do t[i]=i t={} end return t", false),
        ("local t,u=... for i=1,100 do t[i]=u[i] end return t", false),
        ("local t={} for i=1,100 do t[i]=f(i) end return t", false),
        (
            "local t={} for i=1,100 do t[i]='reference' end return t",
            false,
        ),
        ("local s=0 for i=1,100 do s=s+i end return s", false),
        ("return 1", false),
    ] {
        let snapshot = source(text);
        let before = format!("{:?}", snapshot.operations);
        let ledger = &snapshot.operations.allocator().0;
        let usage = (ledger.current(), ledger.peak());
        assert_eq!(has_source_plan(&snapshot).unwrap(), expected, "{text}");
        assert_eq!(usage, (ledger.current(), ledger.peak()));
        assert_eq!(format!("{:?}", snapshot.operations), before);
    }
}

#[test]
fn array_transport_rejects_malformed_source_before_planning() {
    use crate::{
        opcode::Operation,
        types::{ConstantIndex16, RegisterIndex},
    };
    for fault in 0..6 {
        let mut snapshot = source("local t={} for i=1,100 do t[i]=i end return t");
        match fault {
            0 => snapshot.registers = 257,
            1 => snapshot.registers = 0,
            2 => snapshot.constants[0].tag = 255,
            3 => {
                snapshot.operations[0] = Operation::LoadConstant {
                    dest: RegisterIndex(0),
                    constant: ConstantIndex16(u16::MAX),
                }
            }
            4 => {
                snapshot.operations[0] = Operation::NumericForLoop {
                    base: RegisterIndex(255),
                    jump: -1,
                }
            }
            _ => {
                snapshot.operations[0] = Operation::NumericForLoop {
                    base: RegisterIndex(0),
                    jump: i16::MAX,
                }
            }
        }
        assert!(has_source_plan(&snapshot).is_err());
    }
}
