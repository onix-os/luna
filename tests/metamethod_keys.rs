use luna::{IntoValue, Lua, MetaMethod, Table, Value};
use ottavino_gc_arena::Gc;

const METHODS: [MetaMethod; 29] = [
    MetaMethod::Close,
    MetaMethod::Gc,
    MetaMethod::Mode,
    MetaMethod::Metatable,
    MetaMethod::Name,
    MetaMethod::Len,
    MetaMethod::Index,
    MetaMethod::NewIndex,
    MetaMethod::Call,
    MetaMethod::Pairs,
    MetaMethod::ToString,
    MetaMethod::Eq,
    MetaMethod::Add,
    MetaMethod::Sub,
    MetaMethod::Mul,
    MetaMethod::Div,
    MetaMethod::Mod,
    MetaMethod::Pow,
    MetaMethod::Unm,
    MetaMethod::IDiv,
    MetaMethod::BAnd,
    MetaMethod::BOr,
    MetaMethod::BXor,
    MetaMethod::BNot,
    MetaMethod::Shl,
    MetaMethod::Shr,
    MetaMethod::Concat,
    MetaMethod::Lt,
    MetaMethod::Le,
];

#[test]
fn method_keys_preserve_static_identity_across_collection() {
    let mut lua = Lua::empty();
    let addresses = lua.enter(|ctx| {
        METHODS.map(|method| {
            let ordinary = ctx.intern_static(method.name().as_bytes());
            let Value::String(key) = method.into_value(ctx) else {
                panic!("not a string")
            };
            assert_eq!(key.as_bytes(), method.name().as_bytes());
            assert!(Gc::ptr_eq(ordinary.into_inner(), key.into_inner()));
            Gc::as_ptr(key.into_inner()) as usize
        })
    });
    for _ in 0..4 {
        lua.gc_collect();
        lua.enter(|ctx| {
            let before = ctx.mutation().metrics().total_allocation();
            for (method, address) in METHODS.into_iter().zip(addresses) {
                let Value::String(key) = method.into_value(ctx) else {
                    panic!("not a string")
                };
                assert_eq!(Gc::as_ptr(key.into_inner()) as usize, address);
                assert_eq!(key.as_bytes(), method.name().as_bytes());
                let ordinary = ctx.intern_static(method.name().as_bytes());
                assert!(Gc::ptr_eq(ordinary.into_inner(), key.into_inner()));
            }
            assert_eq!(ctx.mutation().metrics().total_allocation(), before);
        });
    }
}

#[test]
fn method_keys_observe_mutation_and_removal_after_collection() {
    let mut lua = Lua::empty();
    let table = lua.enter(|ctx| {
        let table = Table::new(&ctx);
        for (index, method) in METHODS.into_iter().enumerate() {
            table
                .set_raw(
                    &ctx,
                    ctx.intern(method.name().as_bytes()).into(),
                    (index as i64).into(),
                )
                .unwrap();
        }
        ctx.stash(table)
    });
    for round in 0..4 {
        lua.gc_collect();
        lua.enter(|ctx| {
            let table = ctx.fetch(&table);
            for (index, method) in METHODS.into_iter().enumerate() {
                let expected = if round == 0 { index as i64 } else { -(index as i64) - 1 };
                assert!(matches!(table.get_value(ctx, method), Value::Integer(value) if value == expected));
                let key = ctx.intern(method.name().as_bytes()).into();
                table.set_raw(&ctx, key, Value::Nil).unwrap();
                assert!(table.get_value(ctx, method).is_nil());
                table.set_raw(&ctx, key, (-(index as i64) - 1).into()).unwrap();
            }
        });
    }
}

#[test]
fn method_keys_are_owned_by_each_arena() {
    let mut first = Lua::empty();
    let mut second = Lua::empty();
    let addresses = |lua: &mut Lua| {
        lua.enter(|ctx| {
            METHODS.map(|method| {
                let Value::String(key) = method.into_value(ctx) else {
                    panic!("not a string")
                };
                Gc::as_ptr(key.into_inner()) as usize
            })
        })
    };
    let a = addresses(&mut first);
    let b = addresses(&mut second);
    assert!(a.into_iter().zip(b).all(|(a, b)| a != b));
    assert_eq!(a, addresses(&mut first));
    assert_eq!(b, addresses(&mut second));
}
