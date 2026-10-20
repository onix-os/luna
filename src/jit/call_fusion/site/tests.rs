use super::*;
use crate::{Closure, Executor, Fuel, JitConfig, JitMode, Lua};

struct Sources {
    parent: Snapshot,
    child: Snapshot,
    captures: Vec<UpValueDescriptor>,
    params: u8,
    varargs: bool,
}

impl Sources {
    fn new(source: &[u8]) -> Self {
        Lua::empty().enter(|ctx| {
            let closure = Closure::load(ctx, None, source).unwrap();
            Self::from_prototype(&closure.prototype())
        })
    }

    fn from_prototype(parent: &crate::FunctionPrototype<'_>) -> Self {
        let child = parent.prototypes[0];
        Self {
            parent: Snapshot::new(parent, 256, 1024 * 1024).unwrap(),
            child: Snapshot::new(&child, 256, 1024 * 1024).unwrap(),
            captures: child.upvalues.to_vec(),
            params: child.fixed_params,
            varargs: child.has_varargs,
        }
    }

    fn leaf(&self) -> Option<Leaf> {
        Leaf::recognize(&self.child, self.params, self.varargs, &self.captures)
    }

    fn site(&self) -> Option<Site> {
        Site::recognize(&self.parent, self.leaf()?, 0)
    }
}

const SIMPLE: &[u8] = b"local sum=11 local function add(v) sum=sum+v end add(7) return sum";
const LOOP: &[u8] =
    b"local sum=0 local function add(v) sum=sum+v end for i=1,10000 do add(i) end return sum";

fn pairs(slots: &[abi::Slot]) -> Vec<(u64, u64)> {
    slots.iter().map(|s| (s.tag, s.bits)).collect()
}

fn snapshot_slots(values: &[String]) -> Vec<abi::Slot> {
    values
        .iter()
        .map(|value| match value.as_str() {
            "nil:nil" => abi::Slot {
                tag: abi::NIL,
                bits: 0,
            },
            "function" => abi::Slot {
                tag: abi::REFERENCE,
                bits: 0,
            },
            value => abi::Slot {
                tag: abi::INTEGER,
                bits: value
                    .strip_prefix("number:")
                    .unwrap_or_else(|| panic!("unexpected snapshot value {value:?}"))
                    .parse::<i64>()
                    .unwrap() as u64,
            },
        })
        .collect()
}

#[test]
fn recognizes_constant_register_and_loop_regions() {
    for source in [
        SIMPLE,
        LOOP,
        b"local amount=7 local sum=11 local function add(v) sum=sum+v end add(amount) return sum"
            .as_slice(),
        b"local sum=11 local function add(v) sum=sum+v end add(sum) return sum".as_slice(),
    ] {
        let sources = Sources::new(source);
        let site = sources.site().unwrap();
        assert_eq!(site.return_pc - site.region_pc, 3);
        assert!(site.holder < site.temporary);
        assert!(site.leaf.parent < site.temporary);
    }
}

#[test]
fn materialization_matches_every_real_caller_slot_after_collection() {
    let cases = [
        (0, 7),
        (11, 7),
        (-11, -7),
        (i64::MAX, 1),
        (i64::MIN, -1),
        (i64::MAX, i64::MAX),
        (i64::MIN, i64::MIN),
    ];
    for (initial, argument) in cases {
        for kind in 0..3 {
            let initial = initial as i64;
            let argument = argument as i64;
            let source = match kind {
                0 => format!("local sum=0x{:016x} local function add(v) sum=sum+v end add(0x{:016x}) return sum", initial as u64, argument as u64),
                1 => format!("local amount=0x{:016x} local sum=0x{:016x} local function add(v) sum=sum+v end add(amount) return sum", argument as u64, initial as u64),
                _ => format!("local sum=0x{:016x} local function add(v) sum=sum+v end add(sum) return sum", initial as u64),
            };
            let mut lua = Lua::empty();
            lua.set_jit_config(JitConfig {
                mode: JitMode::Off,
                ..Default::default()
            })
            .unwrap();
            let (executor, site) = lua.enter(|ctx| {
                let closure =
                    Closure::load(ctx, Some("fusion-materialization"), source.as_bytes()).unwrap();
                let site = Sources::from_prototype(&closure.prototype())
                    .site()
                    .unwrap();
                (ctx.stash(Executor::start(ctx, closure.into(), ())), site)
            });
            let before = lua.enter(|ctx| {
                let executor = ctx.fetch(&executor);
                assert!(!executor.step(ctx, &mut Fuel::with(1)).unwrap());
                executor.test_frame_snapshot()
            });
            assert_eq!(before.0[0].len(), 2);
            let base: usize = before.0[0][0].split(':').nth(2).unwrap().parse().unwrap();
            let mut model = snapshot_slots(&before.1[0][base..]);
            model.resize(
                usize::from(site.registers),
                abi::Slot {
                    tag: abi::NIL,
                    bits: 0,
                },
            );
            let expected = initial.wrapping_add(if kind == 2 { initial } else { argument });
            assert_eq!(site.materialize(&mut model), Some(expected));
            lua.gc_collect();
            let mut fuel = Fuel::with(1);
            let after = lua.enter(|ctx| {
                let executor = ctx.fetch(&executor);
                assert_eq!(executor.test_frame_snapshot(), before);
                assert!(!executor.step(ctx, &mut fuel).unwrap());
                executor.test_frame_snapshot()
            });
            assert_eq!(fuel.remaining(), -10);
            assert_eq!(after.0[0].len(), 1);
            let (header, _) = before.0[0][0].rsplit_once(':').unwrap();
            assert_eq!(after.0[0][0], format!("{header}:None"));
            assert_eq!(pairs(&model), pairs(&snapshot_slots(&after.1[0][base..])));
            assert_eq!(lua.execute::<i64>(&executor).unwrap(), expected);
            assert_eq!(lua.jit_stats().native_instructions, 0);
        }
    }
}

#[test]
fn rejects_leaf_signature_effect_and_alias_mutations() {
    let mut sources = Sources::new(SIMPLE);
    let baseline = sources.leaf().unwrap();
    for params in [0, 2, 255] {
        assert!(Leaf::recognize(&sources.child, params, false, &sources.captures).is_none());
    }
    assert!(Leaf::recognize(&sources.child, 1, true, &sources.captures).is_none());
    for captures in [
        vec![],
        vec![UpValueDescriptor::Environment],
        vec![UpValueDescriptor::Outer(crate::types::UpValueIndex(0))],
        vec![sources.captures[0]; 2],
    ] {
        assert!(Leaf::recognize(&sources.child, 1, false, &captures).is_none());
    }
    let changes = [
        (
            0,
            Operation::GetUpValue {
                dest: RegisterIndex(0),
                source: crate::types::UpValueIndex(0),
            },
        ),
        (
            1,
            Operation::Sub {
                dest: RegisterIndex(baseline.result),
                left: crate::opcode::RCIndex::Register(RegisterIndex(baseline.read)),
                right: crate::opcode::RCIndex::Register(RegisterIndex(0)),
            },
        ),
        (
            2,
            Operation::SetUpValue {
                dest: crate::types::UpValueIndex(0),
                source: RegisterIndex(0),
            },
        ),
        (
            3,
            Operation::Return {
                start: RegisterIndex(0),
                count: crate::types::VarCount::constant(1),
            },
        ),
    ];
    for (pc, change) in changes {
        let old = sources.child.operations[pc];
        sources.child.operations[pc] = change;
        assert!(sources.leaf().is_none(), "pc={pc}");
        sources.child.operations[pc] = old;
    }
    assert_eq!(sources.leaf(), Some(baseline));
}

#[test]
fn rejects_parent_region_holder_control_and_effect_mutations() {
    let mut sources = Sources::new(SIMPLE);
    let site = sources.site().unwrap();
    let call = site.return_pc as usize - 1;
    let changes = [
        (
            site.region_pc as usize,
            Operation::Move {
                dest: RegisterIndex(site.temporary),
                source: RegisterIndex(site.leaf.parent),
            },
        ),
        (
            call - 1,
            Operation::Move {
                dest: RegisterIndex(site.temporary),
                source: RegisterIndex(site.leaf.parent),
            },
        ),
        (
            call,
            Operation::Call {
                func: RegisterIndex(site.temporary),
                args: crate::types::VarCount::constant(0),
                returns: crate::types::VarCount::constant(0),
            },
        ),
        (
            call,
            Operation::Call {
                func: RegisterIndex(site.temporary),
                args: crate::types::VarCount::constant(1),
                returns: crate::types::VarCount::constant(1),
            },
        ),
        (
            0,
            Operation::LoadNil {
                dest: RegisterIndex(site.holder),
                count: 1,
            },
        ),
        (
            0,
            Operation::Jump {
                offset: (call - 2) as i16,
                close_upvalues: crate::types::Opt254::none(),
            },
        ),
        (
            0,
            Operation::Jump {
                offset: (call - 1) as i16,
                close_upvalues: crate::types::Opt254::none(),
            },
        ),
        (
            0,
            Operation::NewTable {
                dest: RegisterIndex(site.leaf.parent),
                array_size: 0,
                map_size: 0,
            },
        ),
    ];
    for (pc, change) in changes {
        let old = sources.parent.operations[pc];
        sources.parent.operations[pc] = change;
        assert!(sources.site().is_none(), "pc={pc} change={change:?}");
        sources.parent.operations[pc] = old;
    }
    for parent in [site.holder, site.temporary, 255] {
        let mut leaf = site.leaf;
        leaf.parent = parent;
        assert!(Site::recognize(&sources.parent, leaf, 0).is_none());
    }
    assert!(Site::recognize(&sources.parent, site.leaf, 1).is_none());
    assert_eq!(sources.site(), Some(site));
}

#[test]
fn dynamic_type_and_width_refusals_are_atomic() {
    let sources = Sources::new(
        b"local amount=7 local sum=11 local function add(v) sum=sum+v end add(amount) return sum",
    );
    let site = sources.site().unwrap();
    let mut valid = vec![
        abi::Slot {
            tag: abi::INTEGER,
            bits: 7
        };
        usize::from(site.registers)
    ];
    valid[usize::from(site.holder)] = abi::Slot {
        tag: abi::REFERENCE,
        bits: 0,
    };
    let Argument::Register(argument) = site.argument else {
        panic!("register fixture");
    };
    for index in [site.holder, site.leaf.parent, argument] {
        for tag in [
            abi::NIL,
            abi::BOOLEAN,
            abi::INTEGER,
            abi::NUMBER,
            abi::REFERENCE,
            99,
        ] {
            if (index == site.holder && tag == abi::REFERENCE)
                || (index != site.holder && tag == abi::INTEGER)
            {
                continue;
            }
            let mut slots = valid.clone();
            slots[usize::from(index)] = abi::Slot { tag, bits: 0 };
            let before = pairs(&slots);
            assert_eq!(site.materialize(&mut slots), None);
            assert_eq!(pairs(&slots), before);
        }
    }
    for width in [0, valid.len() - 1, valid.len() + 1] {
        let mut slots = valid.clone();
        slots.resize(
            width,
            abi::Slot {
                tag: abi::NIL,
                bits: 0,
            },
        );
        let before = pairs(&slots);
        assert_eq!(site.materialize(&mut slots), None);
        assert_eq!(pairs(&slots), before);
    }
    let mut slots = valid.clone();
    slots[usize::from(site.holder)].bits = 1;
    let before = pairs(&slots);
    assert_eq!(site.materialize(&mut slots), None);
    assert_eq!(pairs(&slots), before);
}

#[test]
fn materialization_tracks_callee_register_aliases_and_return_height() {
    use crate::{
        compiler,
        opcode::{OpCode, RCIndex},
        types::{UpValueIndex, VarCount},
        FunctionPrototype,
    };
    for (child_width, read, result) in [
        (2, 1, 0),
        (2, 1, 1),
        (3, 1, 2),
        (3, 2, 1),
        (4, 3, 0),
        (8, 7, 6),
        (256, 255, 254),
    ] {
        for caller_width in [4, 5, 12] {
            let mut interner = compiler::interning::BasicInterner::default();
            let parsed = compiler::parse_chunk(SIMPLE, &mut interner).unwrap();
            let mut compiled = compiler::compile_chunk(&parsed, &mut interner).unwrap();
            compiled.stack_size = caller_width;
            let child = &mut compiled.prototypes[0];
            child.stack_size = child_width;
            child.opcodes = vec![
                OpCode::encode(Operation::GetUpValue {
                    dest: RegisterIndex(read),
                    source: UpValueIndex(0),
                }),
                OpCode::encode(Operation::Add {
                    dest: RegisterIndex(result),
                    left: RCIndex::Register(RegisterIndex(read)),
                    right: RCIndex::Register(RegisterIndex(0)),
                }),
                OpCode::encode(Operation::SetUpValue {
                    dest: UpValueIndex(0),
                    source: RegisterIndex(result),
                }),
                OpCode::encode(Operation::Return {
                    start: RegisterIndex(0),
                    count: VarCount::constant(0),
                }),
            ];
            let mut lua = Lua::empty();
            lua.set_jit_config(JitConfig {
                mode: JitMode::Off,
                ..Default::default()
            })
            .unwrap();
            let (executor, site) = lua.enter(|ctx| {
                let compiled = compiled.map_strings(|s| ctx.intern(s.as_ref()));
                let prototype = FunctionPrototype::from_compiled(
                    &ctx,
                    ctx.intern(b"fusion-register-layout"),
                    &compiled,
                    false,
                );
                let closure = Closure::new(&ctx, prototype, Some(ctx.globals())).unwrap();
                let site = Sources::from_prototype(&closure.prototype())
                    .site()
                    .unwrap();
                (ctx.stash(Executor::start(ctx, closure.into(), ())), site)
            });
            let before = lua.enter(|ctx| {
                let executor = ctx.fetch(&executor);
                assert!(!executor.step(ctx, &mut Fuel::with(1)).unwrap());
                executor.test_frame_snapshot()
            });
            let base: usize = before.0[0][0].split(':').nth(2).unwrap().parse().unwrap();
            let mut model = snapshot_slots(&before.1[0][base..]);
            model.resize(
                usize::from(site.registers),
                abi::Slot {
                    tag: abi::NIL,
                    bits: 0,
                },
            );
            assert_eq!(site.materialize(&mut model), Some(18));
            lua.gc_collect();
            let after = lua.enter(|ctx| {
                let executor = ctx.fetch(&executor);
                assert_eq!(executor.test_frame_snapshot(), before);
                let mut fuel = Fuel::with(1);
                assert!(!executor.step(ctx, &mut fuel).unwrap());
                assert_eq!(fuel.remaining(), -10);
                executor.test_frame_snapshot()
            });
            assert_eq!(
                pairs(&model),
                pairs(&snapshot_slots(&after.1[0][base..])),
                "caller={caller_width} child={child_width} read={read} result={result}"
            );
            assert_eq!(lua.execute::<i64>(&executor).unwrap(), 18);
        }
    }
}

#[test]
fn rejects_oversized_and_unreferenced_metadata_before_recognition() {
    let mut sources = Sources::new(SIMPLE);
    let leaf = sources.leaf().unwrap();
    sources.parent.operations.try_reserve(257).unwrap();
    while sources.parent.operations.len() <= 256 {
        sources.parent.operations.push(Operation::Return {
            start: RegisterIndex(0),
            count: crate::types::VarCount::constant(0),
        });
    }
    assert!(Site::recognize(&sources.parent, leaf, 0).is_none());
    let mut sources = Sources::new(SIMPLE);
    sources.parent.constants.try_reserve(257).unwrap();
    sources.parent.constants.resize(
        257,
        abi::Slot {
            tag: abi::NIL,
            bits: 0,
        },
    );
    assert!(Site::recognize(&sources.parent, leaf, 0).is_none());
    let mut sources = Sources::new(SIMPLE);
    sources.child.prototypes = 1;
    assert!(sources.leaf().is_none());
    sources.child.prototypes = 0;
    sources.child.upvalues = 2;
    assert!(sources.leaf().is_none());
    sources.child.upvalues = 1;
    sources.child.registers = 257;
    assert!(sources.leaf().is_none());
}
