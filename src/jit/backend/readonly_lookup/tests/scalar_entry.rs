use super::*;
use crate::jit::readonly_lookup::{binding::View, scalar_entry::Source};
use crate::jit::resources::LedgerRef;

fn hostless<'gc>(
    ctx: crate::Context<'gc>,
    closure: Closure<'gc>,
    ordinary: &Code,
    read: &ReadCode,
    binding: Value<'gc>,
    pc: usize,
    budget: u32,
    initial: &[Value<'gc>],
) -> (
    abi::Exit,
    std::vec::Vec<Value<'gc>>,
    std::vec::Vec<(u64, u64)>,
    [u64; 8],
) {
    let mut values = initial.to_vec();
    let mut slots: std::vec::Vec<_> = initial.iter().copied().map(Slot::from_value).collect();
    let (exit, marker) = read
        .plan
        .with_binding(closure, &ordinary.entries, |bound| {
            let mut view = View {
                binding: read.plan.binding(),
                slot: Slot::from_value(binding),
                executed: 0,
            };
            let mut exit = abi::Exit::default();
            unsafe {
                (read.entry)(
                    slots.as_mut_ptr(),
                    pc as u64,
                    budget.min(64),
                    &mut exit,
                    std::ptr::null_mut(),
                    &mut view,
                );
            }
            let crate::closure::UpValueState::Closed(Value::Table(table)) = closure.upvalues()
                [((read.plan.binding()[1] >> 8) & 255) as usize]
                .get()
                .get()
            else {
                panic!()
            };
            assert!(table.into_inner().try_borrow_mut(&ctx).is_err());
            assert!(view.executed <= 1);
            assert!(exit.instructions <= budget.min(64));
            if view.executed == 1 {
                assert!(exit.instructions > 0);
                values[usize::from(read.plan.destination())] = binding;
            }
            drop(bound);
            (exit, view.executed)
        })
        .unwrap();
    for (slot, value) in slots.iter().zip(&mut values) {
        slot.write_back(value);
    }
    (
        exit,
        values,
        slots.iter().map(|s| (s.tag, s.bits)).collect(),
        [0, 0, 0, marker, 0, marker, 0, 0],
    )
}

fn pair<'gc>(
    ctx: crate::Context<'gc>,
    closure: Closure<'gc>,
) -> (Snapshot, Code, ReadCode, MappingCounter, LedgerRef) {
    let source = Snapshot::new(&closure.prototype(), 4096, 8 << 20).unwrap();
    let maps = MappingCounter::new(Ledger::new(usize::MAX));
    let ledger = Ledger::new(8 << 20);
    let ordinary = super::super::super::compile_in(
        &source,
        maps.clone(),
        8 << 20,
        BudgetAllocator(ledger.clone()),
        (&JitConfig::default()).into(),
        Failure::None,
    )
    .unwrap();
    let read = compile(
        &source,
        maps.clone(),
        8 << 20,
        BudgetAllocator(ledger.clone()),
        (&JitConfig::default()).into(),
    )
    .unwrap();
    let crate::closure::UpValueState::Closed(Value::Table(env)) = closure.upvalues()
        [((read.plan.binding()[1] >> 8) & 255) as usize]
        .get()
        .get()
    else {
        panic!()
    };
    let Constant::String(key) =
        closure.prototype().constants[(read.plan.binding()[1] & 255) as usize]
    else {
        panic!()
    };
    env.get_canonical_string(
        ctx,
        key,
        ordinary.entries.hint(read.plan.helper_pc() as usize),
    );
    (source, ordinary, read, maps, ledger)
}

#[test]
fn framed_and_null_host_entries_match_every_scalar_prefix_without_helpers() {
    Lua::empty().enter(|ctx| {
        let env = Table::new(&ctx);
        let callback: Value<'_> =
            Callback::from_fn(&ctx, |_, _, _| panic!("native entry called a user")).into();
        env.set_field(ctx, "host_increment", callback);
        let bindings = [
            callback,
            Closure::load(ctx, None, b"return 13").unwrap().into(),
            Value::Integer(77),
            Value::Boolean(false),
            Value::Number(f64::from_bits(0xfff8_0000_0000_5678)),
            Value::String(ctx.intern(b"captured-value")),
            Value::Table(Table::new(&ctx)),
            Value::Thread(crate::Thread::new(ctx)),
            Value::UserData(crate::UserData::new_static(&ctx, 42u32)),
        ];
        let scripts: &[&[u8]] = &[
            SOURCE,
            b"local sum=0 for i=5,1,-1 do sum=sum+host_increment(i) end return sum",
            b"local x=1 while x do local y=host_increment(x) x=false end return x",
            b"local x=host_increment(false) return x",
        ];
        let scalar = [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(-1),
            Value::Integer(i64::MAX),
            Value::Integer(i64::MIN),
            Value::Number(-0.0),
            Value::Number(1.5),
            Value::Number(f64::INFINITY),
            Value::Number(f64::from_bits(0xfff8_0000_0000_1234)),
        ];
        let mut comparisons = 0;
        let mut reads = 0;
        for script in scripts {
            let closure = Closure::load_with_env(ctx, None, script, env).unwrap();
            let (source, ordinary, read, maps, ledger) = pair(ctx, closure);
            let proof = Source::admit(&source).unwrap();
            assert_eq!(proof.plan(), read.plan);
            for binding in bindings {
                env.set_field(ctx, "host_increment", binding);
                for value in scalar {
                    let mut initial = vec![value; source.registers];
                    if source.registers >= 7 && matches!(value, Value::Integer(0)) {
                        initial[2] = Value::Integer(3);
                        initial[3] = Value::Integer(1);
                        initial[4] = Value::Integer(1);
                    }
                    let slots: std::vec::Vec<_> =
                        initial.iter().copied().map(Slot::from_value).collect();
                    assert!(proof.fits(&slots, &initial));
                    for pc in (0..=source.operations.len() + 1).chain([usize::MAX]) {
                        for budget in (0..=65).chain([u32::MAX]) {
                            let expected =
                                execute(ctx, closure, &ordinary, Some(&read), pc, budget, &initial);
                            let actual = hostless(
                                ctx, closure, &ordinary, &read, binding, pc, budget, &initial,
                            );
                            assert_eq!(
                                (actual.0.pc, actual.0.instructions, actual.0.reason),
                                (expected.0.pc, expected.0.instructions, expected.0.reason),
                                "pc={pc} budget={budget} initial={initial:?}"
                            );
                            assert_eq!(actual.2, expected.2);
                            assert_eq!(actual.3, expected.3);
                            for (&a, &b) in actual.1.iter().zip(&expected.1) {
                                same(a, b);
                            }
                            reads += actual.3[3];
                            comparisons += 1;
                        }
                    }
                }
            }
            drop((ordinary, read));
            assert_eq!(maps.load(Ordering::Relaxed), 0);
            assert_eq!(maps.requested(), 0);
            assert_eq!(ledger.current(), 0);
        }
        assert!(comparisons > 10000 && reads > 100);
        eprintln!("hostless_scalar_native_pairs={comparisons} logical_reads={reads}");
    });
}

#[test]
fn reference_input_guard_is_required_for_the_existing_reference_move_helper() {
    Lua::empty().enter(|ctx| {
        let env = Table::new(&ctx);
        let callback: Value<'_> =
            Callback::from_fn(&ctx, |_, _, _| panic!("native entry called a user")).into();
        env.set_field(ctx, "host_increment", callback);
        let closure = Closure::load_with_env(ctx, None, SOURCE, env).unwrap();
        let (source, ordinary, read, maps, ledger) = pair(ctx, closure);
        let proof = Source::admit(&source).unwrap();
        let pc = read.plan.helper_pc() as usize;
        let Operation::Move { source: input, .. } = source.operations[pc + 1] else {
            panic!()
        };
        let mut initial = vec![Value::Integer(1); source.registers];
        initial[usize::from(input.0)] = Value::Table(env);
        let slots: std::vec::Vec<_> = initial.iter().copied().map(Slot::from_value).collect();
        assert!(!proof.fits(&slots, &initial));
        let framed = execute(ctx, closure, &ordinary, Some(&read), pc, 64, &initial);
        let declined = hostless(ctx, closure, &ordinary, &read, callback, pc, 64, &initial);
        assert!(framed.3[0] > 0 && framed.3[1] > 0);
        assert_eq!(declined.3[0], 0);
        assert_ne!(
            (framed.0.pc, framed.0.instructions, framed.0.reason),
            (declined.0.pc, declined.0.instructions, declined.0.reason)
        );
        drop((ordinary, read));
        assert_eq!(
            (
                maps.load(Ordering::Relaxed),
                maps.requested(),
                ledger.current()
            ),
            (0, 0, 0)
        );
    });
}
