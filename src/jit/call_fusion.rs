use crate::Fuel;

pub(in crate::jit) mod site;

const VM_LIMIT: u32 = 64;
const ATOMIC_REGION: u32 = 7;
const CALL_FUEL: i32 = 5;
const RETURN_FUEL: i32 = 4;
const STEP_FUEL: i32 = 4;
const EXTRA_FUEL: i32 = CALL_FUEL + RETURN_FUEL + STEP_FUEL;

struct Permit {
    budget: u32,
    cost_bound: i32,
}

impl Permit {
    fn new(fuel: &Fuel, deferred: u32, requested: u32) -> Option<Self> {
        let budget = requested.min(VM_LIMIT.checked_sub(deferred)?);
        if budget < ATOMIC_REGION || !fuel.should_continue() {
            return None;
        }
        let cost_bound = budget as i32 + EXTRA_FUEL + STEP_FUEL;
        let available = fuel.remaining().saturating_sub(deferred as i32);
        (available > cost_bound).then_some(Self { budget, cost_bound })
    }
}

#[test]
fn ordinary_leaf_call_exposes_transition_fuel_and_physical_frames() {
    use crate::{opcode::Operation, Closure, Executor, JitConfig, JitMode, Lua};
    let mut lua = Lua::empty();
    lua.set_jit_config(JitConfig {
        mode: JitMode::Off,
        ..Default::default()
    })
    .unwrap();
    let (executor, call_pc, function_register, callee_width, get_register, add_register) = lua
        .enter(|ctx| {
            let closure = Closure::load(
                ctx,
                Some("fusion-fuel-oracle"),
                b"local sum=11 local function add(v) sum=sum+v end add(7) return sum",
            )
            .unwrap();
            let prototype = closure.prototype();
            let (pc, function) = prototype
                .opcodes
                .iter()
                .enumerate()
                .find_map(|(pc, opcode)| match opcode.decode() {
                    Operation::Call {
                        func,
                        args,
                        returns,
                    } => {
                        assert_eq!(args.to_constant(), Some(1));
                        assert_eq!(returns.to_constant(), Some(0));
                        Some((pc, usize::from(func.0)))
                    }
                    _ => None,
                })
                .unwrap();
            let child = prototype.prototypes[0];
            assert_eq!(child.opcodes.len(), 4);
            let Operation::GetUpValue { dest: get, .. } = child.opcodes[0].decode() else {
                panic!("leaf must load its captured sum");
            };
            let Operation::Add { dest: add, .. } = child.opcodes[1].decode() else {
                panic!("leaf must add its argument");
            };
            assert!(matches!(
                child.opcodes[2].decode(),
                Operation::SetUpValue { .. }
            ));
            assert!(matches!(
                child.opcodes[3].decode(),
                Operation::Return { count, .. } if count.to_constant() == Some(0)
            ));
            (
                ctx.stash(Executor::start(ctx, closure.into(), ())),
                pc,
                function,
                usize::from(child.stack_size),
                usize::from(get.0),
                usize::from(add.0),
            )
        });
    let mut call_fuel = Fuel::with(1);
    let before = lua.enter(|ctx| {
        let executor = ctx.fetch(&executor);
        assert!(!executor.step(ctx, &mut call_fuel).unwrap());
        executor.test_frame_snapshot()
    });
    assert_eq!(
        call_fuel.remaining(),
        1 - call_pc as i32 - CALL_FUEL - STEP_FUEL
    );
    assert_eq!(before.0.len(), 1);
    assert_eq!(before.0[0].len(), 2);
    assert!(before.0[0][1].starts_with("lua:0:"));
    lua.gc_collect();
    let mut return_fuel = Fuel::with(1);
    let after = lua.enter(|ctx| {
        let executor = ctx.fetch(&executor);
        assert_eq!(executor.test_frame_snapshot(), before);
        assert!(!executor.step(ctx, &mut return_fuel).unwrap());
        executor.test_frame_snapshot()
    });
    assert_eq!(return_fuel.remaining(), 1 - 3 - RETURN_FUEL - STEP_FUEL);
    let region_pc = call_pc - 2;
    let region_cost =
        (1 - call_fuel.remaining()) + (1 - return_fuel.remaining()) - region_pc as i32;
    assert_eq!(region_cost, 22);
    assert_eq!(region_cost, 5 + EXTRA_FUEL + STEP_FUEL);
    let (header, pending) = before.0[0][0].rsplit_once(':').unwrap();
    assert_eq!(
        pending,
        format!("Some(Normal({:?}))", crate::types::VarCount::constant(0))
    );
    assert_eq!(after.0, vec![vec![format!("{header}:None")]]);
    let base: usize = after.0[0][0].split(':').nth(2).unwrap().parse().unwrap();
    let mut callee = vec!["nil".to_owned(); callee_width];
    callee[0] = "number:7".to_owned();
    callee[get_register] = "number:11".to_owned();
    callee[add_register] = "number:18".to_owned();
    let bottom = base + function_register;
    let mut expected = before.1[0].clone();
    expected.resize(after.1[0].len(), "nil".to_owned());
    expected[base] = "number:18".to_owned();
    for (i, value) in expected[bottom..].iter_mut().enumerate() {
        *value = callee.get(i).map_or("nil", String::as_str).to_owned();
    }
    assert_eq!(after.1[0], expected);
    assert_eq!(lua.execute::<i64>(&executor).unwrap(), 18);
    assert_eq!(lua.jit_stats().native_instructions, 0);
}

#[test]
fn single_region_debit_includes_caller_moves_and_both_executor_steps() {
    let leaf = CALL_FUEL + 3 + RETURN_FUEL + 2 * STEP_FUEL;
    let ordinary = 2 + leaf;
    let fused = 5 + EXTRA_FUEL + STEP_FUEL;
    assert_eq!(leaf, 20);
    assert_eq!(ordinary, 22);
    assert_eq!(ordinary, fused);
    assert_ne!(ordinary, 5 + 11 + STEP_FUEL);
}

#[test]
fn permit_preserves_positive_fuel_at_every_bounded_prefix() {
    for deferred in 0..=VM_LIMIT {
        for requested in 0..=128 {
            let budget = requested.min(VM_LIMIT - deferred);
            let bound = budget as i32 + EXTRA_FUEL + STEP_FUEL;
            assert!(
                Permit::new(&Fuel::with(bound + deferred as i32), deferred, requested).is_none()
            );
            let permitted = Permit::new(
                &Fuel::with(bound + deferred as i32 + 1),
                deferred,
                requested,
            );
            if budget < ATOMIC_REGION {
                assert!(permitted.is_none());
                continue;
            }
            let permit = permitted.unwrap();
            assert_eq!(permit.budget, budget);
            assert_eq!(permit.cost_bound, bound);
            for prefix in 0..=budget {
                let cost = prefix as i32 + CALL_FUEL + RETURN_FUEL + 2 * STEP_FUEL;
                assert!(permit.cost_bound + 1 - cost > 0);
            }
        }
    }
}

#[test]
fn permit_refuses_interrupts_and_invalid_pending_work() {
    let mut interrupted = Fuel::with(i32::MAX);
    interrupted.interrupt();
    assert!(Permit::new(&interrupted, 0, VM_LIMIT).is_none());
    for remaining in [i32::MIN, -1, 0, 1] {
        assert!(Permit::new(&Fuel::with(remaining), 0, VM_LIMIT).is_none());
    }
    assert!(Permit::new(&Fuel::with(i32::MAX), VM_LIMIT + 1, u32::MAX).is_none());
    assert!(Permit::new(&Fuel::with(i32::MAX), u32::MAX, u32::MAX).is_none());
    assert_eq!(
        Permit::new(&Fuel::with(i32::MAX), 0, u32::MAX)
            .unwrap()
            .budget,
        VM_LIMIT
    );
}
