use super::*;
use crate::{Closure, JitConfig, Lua};

pub(super) const SOURCE: &[u8] =
    b"local sum=0 local function add(v) sum=sum+v end for i=1,10000 do add(i) end return sum";

pub(super) fn sources(source: &[u8], test: impl FnOnce(Snapshot, Snapshot, usize)) {
    Lua::empty().enter(|ctx| {
        let closure = Closure::load(ctx, None, source).unwrap();
        let proto = closure.prototype();
        let caller = Snapshot::new(&proto, 4096, 1 << 20).unwrap();
        let callee = Snapshot::new(&proto.prototypes[0], 4096, 1 << 20).unwrap();
        let call = caller
            .operations
            .iter()
            .position(|op| matches!(op, Operation::Call { .. }))
            .unwrap();
        test(caller, callee, call);
    });
}

#[test]
fn admits_source_bound_scalar_calls_without_benchmark_constants() {
    for source in [
        SOURCE,
        &b"local x=7 local function f(v) x=x-v end for i=31,-10,-3 do f(i) end return x"[..],
        &b"local x=3 local function f() x=x*2 end for i=1,17 do f() end return x"[..],
        &b"local x=9 local function f(v) x=x*v end for i=1,3 do f(i) end return x"[..],
    ] {
        sources(source, |caller, callee, call| {
            let before = caller.operations.allocator().0.current();
            let plan = Plan::new(
                &caller,
                &callee,
                call,
                work::Limits::from(&JitConfig::default()),
            )
            .unwrap_or_else(|error| {
                panic!(
                    "{error:?}: source={} callee={:?}",
                    String::from_utf8_lossy(source),
                    callee.operations
                )
            });
            plan.verify(&caller, &callee).unwrap();
            let descriptor = plan.descriptor().unwrap();
            assert_eq!(descriptor.call, call);
            assert_eq!(descriptor.registers, caller.registers);
            assert_eq!(plan.end, call + 1);
            assert!(plan.start <= call);
            assert!(plan.base + 4 <= caller.registers);
            assert_eq!(caller.operations.allocator().0.current(), before);
        });
    }
}

#[test]
fn unsupported_effects_returns_and_shapes_remain_outside_the_region() {
    for source in [
        &b"local x=9 local function f(v) x=x+x end for i=1,3 do f(i) end return x"[..],
        &b"local x=0 local function f(v) x=x+v end f(3) return x"[..],
        &b"local x=0 local function f(v) x=x+v end for i=1,3 do f(i) x=x+1 end return x"[..],
        &b"local x=0 local function f(v) x=x+v end for i=1,3 do x=x+1 f(i) end return x"[..],
        &b"local x=0 local function f(v) x=x+v return x end for i=1,3 do f(i) end return x"[..],
        &b"local x=0 local function f(v) x=x+v end for i=1,3 do f(i,i+1) end return x"[..],
    ] {
        sources(source, |caller, callee, call| {
            assert!(Plan::new(
                &caller,
                &callee,
                call,
                work::Limits::from(&JitConfig::default())
            )
            .is_err());
        });
    }
}

#[test]
fn region_fields_and_source_owners_are_rechecked() {
    sources(SOURCE, |caller, callee, call| {
        let limits = work::Limits::from(&JitConfig::default());
        for fault in 0..4 {
            let mut plan = Plan::new(&caller, &callee, call, limits).unwrap();
            match fault {
                0 => plan.start += 1,
                1 => plan.end += 1,
                2 => plan.base += 1,
                _ => plan.call += 1,
            }
            assert!(plan.verify(&caller, &callee).is_err());
            assert!(plan.descriptor().is_err());
        }
        let plan = Plan::new(&caller, &callee, call, limits).unwrap();
        sources(SOURCE, |other_caller, other_callee, _| {
            assert!(plan.verify(&other_caller, &callee).is_err());
            assert!(plan.verify(&caller, &other_callee).is_err());
        });
    });
}

#[test]
fn aggregate_expansion_limits_are_not_relaxed_by_inlining_admission() {
    sources(SOURCE, |caller, callee, call| {
        let limits = work::Limits::from(&JitConfig::default());
        let plan = Plan::new(&caller, &callee, call, limits).unwrap();
        let exact = work::Limits {
            instructions: plan.pair.expansion.instructions,
            blocks: plan.pair.expansion.blocks,
            relocations: 3,
        };
        Plan::new(&caller, &callee, call, exact).unwrap();
        for rejected in [
            work::Limits {
                instructions: exact.instructions - 1,
                ..exact
            },
            work::Limits {
                blocks: exact.blocks - 1,
                ..exact
            },
            work::Limits {
                relocations: 2,
                ..exact
            },
        ] {
            assert!(matches!(
                Plan::new(&caller, &callee, call, rejected),
                Err(JitError::ResourceLimit(_))
            ));
        }
    });
}
