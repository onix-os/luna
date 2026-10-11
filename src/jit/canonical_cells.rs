use ottavino_gc_arena::{Collect, Collection};

use crate::{Callback, Closure, Function, String, Table, Thread, UserData, Value};

#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Nil = 0,
    Boolean = 1,
    Integer = 2,
    Number = 3,
    String = 4,
    Table = 5,
    Closure = 6,
    Callback = 7,
    Thread = 8,
    UserData = 9,
}

#[repr(C)]
#[derive(Clone, Copy)]
union Payload<'gc> {
    bits: u64,
    integer: i64,
    number: f64,
    string: String<'gc>,
    table: Table<'gc>,
    closure: Closure<'gc>,
    callback: Callback<'gc>,
    thread: Thread<'gc>,
    userdata: UserData<'gc>,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Cell<'gc> {
    kind: Kind,
    payload: Payload<'gc>,
}

impl<'gc> Cell<'gc> {
    fn new(value: Value<'gc>) -> Self {
        let (kind, payload) = match value {
            Value::Nil => (Kind::Nil, Payload { bits: 0 }),
            Value::Boolean(value) => (
                Kind::Boolean,
                Payload {
                    bits: u64::from(value),
                },
            ),
            Value::Integer(value) => (Kind::Integer, Payload { integer: value }),
            Value::Number(value) => (Kind::Number, Payload { number: value }),
            Value::String(value) => (Kind::String, Payload { string: value }),
            Value::Table(value) => (Kind::Table, Payload { table: value }),
            Value::Function(Function::Closure(value)) => {
                (Kind::Closure, Payload { closure: value })
            }
            Value::Function(Function::Callback(value)) => {
                (Kind::Callback, Payload { callback: value })
            }
            Value::Thread(value) => (Kind::Thread, Payload { thread: value }),
            Value::UserData(value) => (Kind::UserData, Payload { userdata: value }),
        };
        Self { kind, payload }
    }

    fn value(self) -> Value<'gc> {
        unsafe {
            match self.kind {
                Kind::Nil => Value::Nil,
                Kind::Boolean => Value::Boolean(self.payload.bits != 0),
                Kind::Integer => Value::Integer(self.payload.integer),
                Kind::Number => Value::Number(self.payload.number),
                Kind::String => Value::String(self.payload.string),
                Kind::Table => Value::Table(self.payload.table),
                Kind::Closure => Value::Function(Function::Closure(self.payload.closure)),
                Kind::Callback => Value::Function(Function::Callback(self.payload.callback)),
                Kind::Thread => Value::Thread(self.payload.thread),
                Kind::UserData => Value::UserData(self.payload.userdata),
            }
        }
    }

    fn scalar(self) -> Option<(usize, u64)> {
        let bits = unsafe {
            match self.kind {
                Kind::Nil => 0,
                Kind::Boolean => self.payload.bits,
                Kind::Integer => self.payload.integer as u64,
                Kind::Number => self.payload.number.to_bits(),
                _ => return None,
            }
        };
        Some((self.kind as usize, bits))
    }

    fn set_scalar(&mut self, tag: usize, bits: u64) -> bool {
        let value = match tag {
            0 => Value::Nil,
            1 if bits <= 1 => Value::Boolean(bits != 0),
            2 => Value::Integer(bits as i64),
            3 => Value::Number(f64::from_bits(bits)),
            _ => return false,
        };
        *self = Self::new(value);
        true
    }
}

unsafe impl Collect for Cell<'_> {
    fn trace(&self, cc: &Collection) {
        self.value().trace(cc);
    }
}

#[cfg(test)]
#[path = "canonical_cells_tests.rs"]
mod tests;

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[path = "canonical_cells_native.rs"]
mod native;
