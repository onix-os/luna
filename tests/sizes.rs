use std::mem;

use luna::{opcode::OpCode, Callback, Closure, String, Table, Thread, UserData, Value};

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
    assert!(mem::size_of::<Value>() <= tagged_payload);
}
