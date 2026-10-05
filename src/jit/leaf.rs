use crate::{opcode::Operation, types::RegisterIndex, Value};

use super::{abi, ir::Snapshot, projection::Origin};

const VERSION: u64 = 0x4c55_4e41_4345_4c31;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Arithmetic {
    Add,
    Sub,
    Mul,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Operand {
    Register(RegisterIndex),
    Constant(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Pattern {
    pub read: RegisterIndex,
    pub result: RegisterIndex,
    pub upvalue: u8,
    pub arithmetic: Arithmetic,
    pub right: Operand,
}

impl Pattern {
    pub(super) fn recognize(snapshot: &Snapshot) -> Option<Self> {
        use crate::opcode::RCIndex;
        use Operation::*;

        snapshot.verify().ok()?;
        let [GetUpValue {
            dest: read,
            source: upvalue,
        }, math, SetUpValue {
            dest: write,
            source,
        }, Return { .. }] = snapshot.operations.as_slice()
        else {
            return None;
        };
        let (arithmetic, result, left, right) = match math {
            Add { dest, left, right } => (Arithmetic::Add, dest, left, right),
            Sub { dest, left, right } => (Arithmetic::Sub, dest, left, right),
            Mul { dest, left, right } => (Arithmetic::Mul, dest, left, right),
            _ => return None,
        };
        if !matches!(left, RCIndex::Register(register) if register == read)
            || result != source
            || upvalue != write
        {
            return None;
        }
        Some(Self {
            read: *read,
            result: *result,
            upvalue: upvalue.0,
            arithmetic,
            right: match right {
                RCIndex::Register(register) => Operand::Register(*register),
                RCIndex::Constant(constant) => Operand::Constant(constant.0),
            },
        })
    }
}

#[repr(C)]
pub(super) struct View {
    pub version: u64,
    pub cell: *mut abi::Slot,
    pub reads: u32,
    pub writes: u32,
    pub dirty: u64,
}

#[derive(Clone, Copy)]
enum Target {
    Upper(usize),
    Register(usize),
}

pub(super) struct Binding {
    target: Target,
    value: abi::Slot,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Error {
    ViewMismatch,
    NonScalar,
    TargetRange,
}

pub(super) struct Delta {
    pub reads: u32,
    pub writes: u32,
    upper: Option<(usize, abi::Slot)>,
}

fn scalar(slot: abi::Slot) -> bool {
    match slot.tag {
        abi::NIL => slot.bits == 0,
        abi::BOOLEAN => slot.bits <= 1,
        abi::INTEGER | abi::NUMBER => true,
        _ => false,
    }
}

impl Binding {
    pub(super) fn from_origin(origin: Origin<'_>, scratch: &[abi::Slot]) -> Option<Self> {
        let (target, value) = match origin {
            Origin::Upper(index, value) => (Target::Upper(index), abi::Slot::from_value(value)),
            Origin::Register(index, _) => (Target::Register(index), *scratch.get(index)?),
            Origin::Closed(_) => return None,
        };
        scalar(value).then_some(Self { target, value })
    }

    /// Consumes the binding and calls an entry with scoped scalar and register pointers.
    pub(super) fn with_native<R>(
        mut self,
        scratch: &mut [abi::Slot],
        entry: impl FnOnce(*mut abi::Slot, *mut View) -> R,
    ) -> Result<(R, Delta), Error> {
        let pointer = scratch.as_mut_ptr();
        let cell = match self.target {
            Target::Upper(_) => std::ptr::addr_of_mut!(self.value),
            Target::Register(index) if index < scratch.len() => unsafe { pointer.add(index) },
            Target::Register(_) => return Err(Error::TargetRange),
        };
        if !scalar(unsafe { cell.read() }) {
            return Err(Error::NonScalar);
        }
        let mut view = View {
            version: VERSION,
            cell,
            reads: 0,
            writes: 0,
            dirty: 0,
        };
        let result = entry(pointer, std::ptr::addr_of_mut!(view));
        if view.version != VERSION
            || view.cell != cell
            || view.reads > 1
            || view.writes > 1
            || view.dirty != u64::from(view.writes)
        {
            return Err(Error::ViewMismatch);
        }
        let upper = if view.writes != 0 {
            let value = unsafe { cell.read() };
            if !scalar(value) {
                return Err(Error::NonScalar);
            }
            match self.target {
                Target::Upper(index) => Some((index, value)),
                Target::Register(_) => None,
            }
        } else {
            None
        };
        Ok((
            result,
            Delta {
                reads: view.reads,
                writes: view.writes,
                upper,
            },
        ))
    }
}

impl Delta {
    pub(super) fn apply_upper(&self, upper: &mut [Value<'_>]) -> Result<(), Error> {
        if let Some((index, value)) = self.upper {
            let target = upper.get_mut(index).ok_or(Error::TargetRange)?;
            value.write_back(target);
        }
        Ok(())
    }
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<View>() == 32);
    assert!(std::mem::offset_of!(View, cell) == 8);
    assert!(std::mem::offset_of!(View, reads) == 16);
    assert!(std::mem::offset_of!(View, writes) == 20);
    assert!(std::mem::offset_of!(View, dirty) == 24);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_real_leaf_bytecode_without_changing_execution() {
        for (expression, expected) in [
            ("sum+v", Some(Arithmetic::Add)),
            ("sum-v", Some(Arithmetic::Sub)),
            ("sum*v", Some(Arithmetic::Mul)),
            ("sum+1", Some(Arithmetic::Add)),
            ("sum/v", None),
            ("v-sum", None),
        ] {
            let mut lua = crate::Lua::empty();
            let executor = lua.enter(|ctx| {
                let text = format!("local sum=0 return function(v) sum={expression} end");
                let closure = crate::Closure::load(ctx, None, text.as_bytes()).unwrap();
                ctx.stash(crate::Executor::start(ctx, closure.into(), ()))
            });
            lua.finish(&executor).unwrap();
            lua.enter(|ctx| {
                let closure = ctx
                    .fetch(&executor)
                    .take_result::<crate::Closure>(ctx)
                    .unwrap()
                    .unwrap();
                let mut snapshot = Snapshot::new(&closure.prototype(), 1024, 65536).unwrap();
                let pattern = Pattern::recognize(&snapshot);
                assert_eq!(pattern.map(|p| p.arithmetic), expected);
                if let Some(pattern) = pattern {
                    assert!(usize::from(pattern.read.0) < snapshot.registers);
                    assert!(usize::from(pattern.result.0) < snapshot.registers);
                    assert_eq!(pattern.upvalue, 0);
                    if expression == "sum+1" {
                        assert!(matches!(pattern.right, Operand::Constant(_)));
                    } else {
                        assert!(matches!(pattern.right, Operand::Register(RegisterIndex(0))));
                    }
                    let original = snapshot.operations[2];
                    snapshot.operations[2] = Operation::SetUpValue {
                        dest: crate::types::UpValueIndex(pattern.upvalue),
                        source: RegisterIndex(pattern.result.0 ^ 1),
                    };
                    assert!(Pattern::recognize(&snapshot).is_none());
                    snapshot.operations[2] = original;
                    snapshot.operations.push(original);
                    assert!(Pattern::recognize(&snapshot).is_none());
                }
            });
        }
    }

    #[test]
    fn upper_writes_preserve_scalar_payloads_and_counts() {
        for value in [
            Value::Nil,
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Number(-0.0),
            Value::Number(f64::from_bits(0x7ff8_1234_5678_9abc)),
        ] {
            let mut scratch = [abi::Slot::from_value(Value::Integer(2))];
            let binding = Binding::from_origin(Origin::Upper(0, value), &scratch).unwrap();
            let expected = abi::Slot::from_value(value);
            let (_, delta) = binding
                .with_native(&mut scratch, |_, view| unsafe {
                    assert_eq!((*(*view).cell).tag, expected.tag);
                    assert_eq!((*(*view).cell).bits, expected.bits);
                    (*view).reads = 1;
                    (*view).writes = 1;
                    (*view).dirty = 1;
                })
                .unwrap();
            let mut upper = [Value::Integer(9)];
            delta.apply_upper(&mut upper).unwrap();
            let actual = abi::Slot::from_value(upper[0]);
            assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
            assert_eq!((delta.reads, delta.writes), (1, 1));
        }
    }

    #[test]
    fn current_aliases_reuse_original_scratch_provenance_and_pending_values() {
        let mut scratch = [abi::Slot::from_value(Value::Integer(3)); 2];
        let binding = Binding::from_origin(Origin::Register(0, Value::Nil), &scratch).unwrap();
        let (_, delta) = binding
            .with_native(&mut scratch, |pointer, view| unsafe {
                assert_eq!((*view).cell, pointer);
                {
                    let registers = std::slice::from_raw_parts_mut(pointer, 2);
                    registers[0].bits = 7;
                }
                assert_eq!((*(*view).cell).bits, 7);
                (*(*view).cell).bits = 11;
                assert_eq!((*pointer).bits, 11);
                (*view).reads = 1;
                (*view).writes = 1;
                (*view).dirty = 1;
            })
            .unwrap();
        let mut upper = [Value::Integer(19)];
        delta.apply_upper(&mut upper).unwrap();
        assert!(matches!(upper[0], Value::Integer(19)));
        assert_eq!(scratch[0].bits, 11);
        assert_eq!((delta.reads, delta.writes), (1, 1));
    }

    #[test]
    fn rejects_closed_reference_and_out_of_range_bindings() {
        let scalar = abi::Slot::from_value(Value::Integer(1));
        assert!(Binding::from_origin(Origin::Closed(Value::Integer(1)), &[scalar]).is_none());
        assert!(Binding::from_origin(Origin::Register(1, Value::Nil), &[scalar]).is_none());
        let reference = abi::Slot {
            tag: abi::REFERENCE,
            bits: 0,
        };
        assert!(Binding::from_origin(Origin::Register(0, Value::Nil), &[reference]).is_none());
        let binding = Binding::from_origin(Origin::Register(0, Value::Nil), &[scalar]).unwrap();
        assert!(matches!(
            binding.with_native(&mut [], |_, _| ()),
            Err(Error::TargetRange)
        ));
    }

    #[test]
    fn refuses_invalid_metadata_and_payloads_without_upper_commit() {
        for mutation in 0..9 {
            let mut scratch = [abi::Slot::from_value(Value::Integer(1))];
            let binding =
                Binding::from_origin(Origin::Upper(0, Value::Integer(5)), &scratch).unwrap();
            let result = binding.with_native(&mut scratch, |_, view| unsafe {
                (*view).writes = 1;
                (*view).dirty = 1;
                match mutation {
                    0 => (*view).version = 0,
                    1 => (*view).cell = std::ptr::null_mut(),
                    2 => (*view).reads = 2,
                    3 => (*view).writes = 2,
                    4 => (*view).dirty = 0,
                    5 => (*(*view).cell).tag = abi::REFERENCE,
                    6 => (*(*view).cell).tag = u64::MAX,
                    7 => {
                        *(*view).cell = abi::Slot {
                            tag: abi::NIL,
                            bits: 1,
                        }
                    }
                    8 => {
                        *(*view).cell = abi::Slot {
                            tag: abi::BOOLEAN,
                            bits: 2,
                        }
                    }
                    _ => unreachable!(),
                }
            });
            assert!(result.is_err());
        }
    }

    #[test]
    fn partial_and_helper_only_paths_do_not_overwrite_upper_values() {
        for reads in [0, 1] {
            let mut scratch = [abi::Slot::from_value(Value::Integer(1))];
            let binding =
                Binding::from_origin(Origin::Upper(0, Value::Integer(5)), &scratch).unwrap();
            let (_, delta) = binding
                .with_native(&mut scratch, |_, view| unsafe {
                    (*view).reads = reads;
                })
                .unwrap();
            let mut upper = [Value::Integer(13)];
            delta.apply_upper(&mut upper).unwrap();
            assert!(matches!(upper[0], Value::Integer(13)));
            assert_eq!((delta.reads, delta.writes), (reads, 0));
        }
    }

    #[test]
    fn helper_reference_writes_do_not_trigger_scalar_recommit() {
        let mut scratch = [abi::Slot::from_value(Value::Integer(1))];
        let binding = Binding::from_origin(Origin::Register(0, Value::Nil), &scratch).unwrap();
        let (_, delta) = binding
            .with_native(&mut scratch, |pointer, _| unsafe {
                pointer.write(abi::Slot {
                    tag: abi::REFERENCE,
                    bits: 0,
                });
            })
            .unwrap();
        assert_eq!((delta.reads, delta.writes), (0, 0));
        assert_eq!(scratch[0].tag, abi::REFERENCE);
        let mut upper = [Value::Integer(13)];
        delta.apply_upper(&mut upper).unwrap();
        assert!(matches!(upper[0], Value::Integer(13)));
    }

    #[test]
    fn invalid_upper_commit_targets_preserve_existing_values() {
        let mut scratch = [abi::Slot::from_value(Value::Integer(1))];
        let binding = Binding::from_origin(Origin::Upper(2, Value::Integer(5)), &scratch).unwrap();
        let (_, delta) = binding
            .with_native(&mut scratch, |_, view| unsafe {
                (*view).writes = 1;
                (*view).dirty = 1;
            })
            .unwrap();
        let mut upper = [Value::Integer(13)];
        assert_eq!(delta.apply_upper(&mut upper), Err(Error::TargetRange));
        assert!(matches!(upper[0], Value::Integer(13)));
    }
}
