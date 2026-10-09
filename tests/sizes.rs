use std::mem;

use luna::{opcode::OpCode, Callback, Closure, Constant, String, Table, Thread, UserData, Value};

#[test]
fn test_sizes() {
    assert!(mem::size_of::<OpCode>() <= 4);

    let ptr_size = mem::size_of::<*const ()>();
    assert_eq!(mem::size_of::<String>(), ptr_size);
    assert_eq!(mem::size_of::<Table>(), ptr_size);
    assert_eq!(mem::size_of::<Closure>(), ptr_size);
    assert_eq!(mem::size_of::<Callback>(), ptr_size);
    assert_eq!(mem::size_of::<Thread>(), ptr_size);
    assert_eq!(mem::size_of::<UserData>(), ptr_size);
    let tagged_payload = mem::size_of::<(usize, i64)>();
    for (name, size) in [
        ("Value", mem::size_of::<Value>()),
        ("Constant<String>", mem::size_of::<Constant<String>>()),
        ("Option<Value>", mem::size_of::<Option<Value>>()),
        (
            "Option<Constant<String>>",
            mem::size_of::<Option<Constant<String>>>(),
        ),
    ] {
        assert!(
            size <= tagged_payload,
            "{name}: {size} bytes exceeds {tagged_payload}"
        );
    }
}
