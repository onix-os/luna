#[cfg(test)]
use crate::Value;
use crate::{opcode::Operation, types::RegisterIndex};

#[cfg(test)]
use super::projection::Origin;
use super::{abi, ir::Snapshot};

pub(super) const VERSION: u64 = 0x4c55_4e41_4345_4c31;

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
    #[cfg(all(test, not(miri)))]
    pub(super) fn permits_cell_entry(self, pc: usize, slots: &[abi::Slot]) -> bool {
        pc != 2
            || slots
                .get(usize::from(self.result.0))
                .copied()
                .is_some_and(scalar)
    }

    pub(super) fn recognize(snapshot: &Snapshot) -> Option<Self> {
        snapshot.verify().ok()?;
        Self::from_operations(&snapshot.operations)
    }

    pub(super) fn from_operations(operations: &[Operation]) -> Option<Self> {
        use crate::opcode::RCIndex;
        use Operation::*;

        let [GetUpValue {
            dest: read,
            source: upvalue,
        }, math, SetUpValue {
            dest: write,
            source,
        }, Return { .. }] = operations
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

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(super) type CellEntry =
    unsafe extern "C" fn(*mut abi::Slot, u64, u32, *mut View) -> abi::return_words::Words;

#[cfg(test)]
#[derive(Clone, Copy)]
enum Target {
    Upper(usize),
    Register(usize),
}

#[cfg(test)]
pub(super) struct Binding {
    target: Target,
    value: abi::Slot,
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Error {
    ViewMismatch,
    NonScalar,
    TargetRange,
}

#[cfg(test)]
pub(super) struct Delta {
    pub reads: u32,
    pub writes: u32,
    upper: Option<(usize, abi::Slot)>,
}

#[cfg(test)]
fn scalar(slot: abi::Slot) -> bool {
    match slot.tag {
        abi::NIL => slot.bits == 0,
        abi::BOOLEAN => slot.bits <= 1,
        abi::INTEGER | abi::NUMBER => true,
        _ => false,
    }
}

#[cfg(test)]
impl Binding {
    #[cfg(test)]
    pub(super) fn integer_activation(&self, pattern: Pattern, right: abi::Slot) -> bool {
        let right = if matches!(pattern.right, Operand::Register(index) if index == pattern.read) {
            self.value
        } else {
            right
        };
        self.value.tag == abi::INTEGER && right.tag == abi::INTEGER
    }

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

#[cfg(test)]
impl Delta {
    pub(super) fn apply_registers(
        &self,
        registers: &mut crate::thread::LuaRegisters<'_, '_>,
    ) -> Result<(), Error> {
        if let Some((index, value)) = self.upper {
            if registers.projection_read(true, index).is_none() {
                return Err(Error::TargetRange);
            }
            registers.projection_write(true, index, value.value(Value::Nil));
        }
        Ok(())
    }

    #[cfg(test)]
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
    fn integer_activation_uses_post_read_alias_and_rejects_other_tags() {
        let slots = [abi::Slot {
            tag: abi::NIL,
            bits: 0,
        }];
        let binding = Binding::from_origin(Origin::Upper(0, Value::Integer(2)), &slots).unwrap();
        let mut pattern = Pattern {
            read: RegisterIndex(0),
            result: RegisterIndex(0),
            upvalue: 0,
            arithmetic: Arithmetic::Add,
            right: Operand::Register(RegisterIndex(0)),
        };
        assert!(binding.integer_activation(pattern, slots[0]));
        pattern.right = Operand::Register(RegisterIndex(1));
        for value in [Value::Nil, Value::Boolean(true), Value::Number(2.0)] {
            assert!(!binding.integer_activation(pattern, abi::Slot::from_value(value)));
        }
        assert!(binding.integer_activation(pattern, abi::Slot::from_value(Value::Integer(2))));
        let floating = Binding::from_origin(Origin::Upper(0, Value::Number(2.0)), &slots).unwrap();
        assert!(!floating.integer_activation(pattern, abi::Slot::from_value(Value::Integer(2))));
    }

    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    mod native {
        use super::*;

        #[cfg(not(miri))]
        fn with_closure(test: impl for<'gc> FnOnce(crate::Context<'gc>, crate::Closure<'gc>)) {
            let mut lua = crate::Lua::empty();
            let executor = lua.enter(|ctx| {
                let closure = crate::Closure::load(
                    ctx,
                    None,
                    b"local sum=0 return function(v) sum=sum+v end",
                )
                .unwrap();
                ctx.stash(crate::Executor::start(ctx, closure.into(), ()))
            });
            lua.finish(&executor).unwrap();
            lua.enter(|ctx| {
                let closure = ctx
                    .fetch(&executor)
                    .take_result::<crate::Closure>(ctx)
                    .unwrap()
                    .unwrap();
                test(ctx, closure);
            });
        }

        #[cfg(not(miri))]
        #[test]
        fn generated_leaf_preserves_current_aliases_and_every_budget_cut() {
            use crate::jit::{backend, helpers, resources, work, JitConfig};
            use crate::thread::LuaRegisters;
            use std::sync::atomic::Ordering;
            with_closure(|ctx, closure| {
                let snapshot = Snapshot::new(&closure.prototype(), 64, 1024 * 1024).unwrap();
                let pattern = Pattern::recognize(&snapshot).unwrap();
                let metadata_ledger = resources::Ledger::new(2 * 1024 * 1024);
                let total = resources::MappingCounter::new(resources::Ledger::new(2 * 1024 * 1024));
                let code = backend::compile_leaf_in(
                    &snapshot,
                    total.clone(),
                    128 * 1024,
                    resources::BudgetAllocator(metadata_ledger.clone()),
                    work::Limits::from(&JitConfig::default()),
                    backend::Failure::None,
                )
                .unwrap();
                assert!(!code.projected_upvalues);
                for budget in [0, 1, 2, 3, 64] {
                    let mut canonical = vec![Value::Nil; code.registers];
                    canonical[0] = Value::Integer(2);
                    let mut pc = 0;
                    LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                        let cell = registers.projection_open_at(ctx, 0);
                        closure.set_upvalue(&ctx, usize::from(pattern.upvalue), cell);
                        let mut scratch: Vec<_> = registers
                            .stack_frame
                            .iter()
                            .copied()
                            .map(abi::Slot::from_value)
                            .collect();
                        let binding = Binding::from_origin(
                            registers.projection_origin(cell).unwrap(),
                            &scratch,
                        )
                        .unwrap();
                        let mut frame = helpers::Frame {
                            ctx,
                            closure,
                            registers: &mut registers,
                            count: helpers::Counts::default(),
                            slot_count: code.registers,
                            panic: None,
                            projection: None,
                        };
                        let (exit, delta) = binding
                            .with_native(&mut scratch, |slots, view| {
                                let mut host = abi::Host {
                                    data: std::ptr::addr_of_mut!(frame).cast(),
                                    projection: view.cast(),
                                };
                                unsafe { code.invoke_raw(slots, 0, budget, &mut host) }
                            })
                            .unwrap();
                        let completed = budget.min(3);
                        assert_eq!(
                            (exit.pc, exit.instructions),
                            (u64::from(completed), completed)
                        );
                        assert_eq!(
                            (delta.reads, delta.writes),
                            (u32::from(budget >= 1), u32::from(budget >= 3))
                        );
                        assert_eq!((frame.count.calls, frame.count.completed), (0, 0));
                        assert!(frame.panic.is_none());
                        for (slot, value) in
                            scratch.iter().zip(frame.registers.stack_frame.iter_mut())
                        {
                            slot.write_back(value);
                        }
                        delta.apply_upper(&mut []).unwrap();
                        assert_eq!(scratch[0].bits, if budget >= 3 { 4 } else { 2 });
                        if budget >= 1 {
                            assert_eq!(
                                scratch[usize::from(pattern.read.0)].bits,
                                if budget >= 2 { 4 } else { 2 }
                            );
                        }
                    });
                }
                let mut canonical = vec![Value::Nil; code.registers];
                canonical[0] = Value::Integer(2);
                let mut pc = 0;
                LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                    let cell = registers.projection_open_at(ctx, 0);
                    closure.set_upvalue(&ctx, usize::from(pattern.upvalue), cell);
                    let mut reads = 0;
                    let mut writes = 0;
                    for entry in 0..3 {
                        let mut scratch: Vec<_> = registers
                            .stack_frame
                            .iter()
                            .copied()
                            .map(abi::Slot::from_value)
                            .collect();
                        let binding = Binding::from_origin(
                            registers.projection_origin(cell).unwrap(),
                            &scratch,
                        )
                        .unwrap();
                        let mut frame = helpers::Frame {
                            ctx,
                            closure,
                            registers: &mut registers,
                            count: helpers::Counts::default(),
                            slot_count: code.registers,
                            panic: None,
                            projection: None,
                        };
                        let (exit, delta) = binding
                            .with_native(&mut scratch, |slots, view| {
                                let mut host = abi::Host {
                                    data: std::ptr::addr_of_mut!(frame).cast(),
                                    projection: view.cast(),
                                };
                                unsafe { code.invoke_raw(slots, entry, 1, &mut host) }
                            })
                            .unwrap();
                        assert_eq!((exit.pc, exit.instructions), (entry as u64 + 1, 1));
                        assert_eq!(frame.count.calls, 0);
                        reads += delta.reads;
                        writes += delta.writes;
                        for (slot, value) in
                            scratch.iter().zip(frame.registers.stack_frame.iter_mut())
                        {
                            slot.write_back(value);
                        }
                        delta.apply_upper(&mut []).unwrap();
                        *frame.registers.pc = exit.pc as usize;
                    }
                    assert_eq!((reads, writes), (1, 1));
                    assert!(matches!(registers.stack_frame[0], Value::Integer(4)));
                });
                drop(code);
                assert_eq!(total.load(Ordering::Relaxed), 0);
                assert_eq!(metadata_ledger.current(), 0);
            });
        }

        #[test]
        fn scalar_kernel_uses_only_cell_views_at_every_budget_and_entry() {
            use crate::jit::{backend, resources, work, JitConfig};
            use std::sync::atomic::Ordering;
            with_closure(|_ctx, closure| {
                let snapshot = Snapshot::new(&closure.prototype(), 64, 1024 * 1024).unwrap();
                let pattern = Pattern::recognize(&snapshot).unwrap();
                let ledger = resources::Ledger::new(2 * 1024 * 1024);
                let total = resources::MappingCounter::new(resources::Ledger::new(2 * 1024 * 1024));
                let code = backend::compile_leaf_kernel_in(
                    &snapshot,
                    total.clone(),
                    128 * 1024,
                    resources::BudgetAllocator(ledger.clone()),
                    work::Limits::from(&JitConfig::default()),
                    backend::Failure::None,
                )
                .unwrap();
                assert!(!code.projected_upvalues);
                for entry in 0..=3 {
                    for budget in [0, 1, 2, 3, 64, u32::MAX] {
                        for current in [false, true] {
                            let mut scratch =
                                vec![abi::Slot::from_value(Value::Nil); code.registers];
                            scratch[0] = abi::Slot::from_value(Value::Integer(2));
                            if entry != 0 {
                                scratch[usize::from(pattern.read.0)] =
                                    abi::Slot::from_value(Value::Integer(if entry == 1 {
                                        2
                                    } else {
                                        4
                                    }));
                            }
                            let binding = Binding::from_origin(
                                if current {
                                    Origin::Register(0, Value::Integer(2))
                                } else {
                                    Origin::Upper(0, Value::Integer(2))
                                },
                                &scratch,
                            )
                            .unwrap();
                            let (exit, delta) = binding
                                .with_native(&mut scratch, |slots, view| unsafe {
                                    code.invoke_cell_raw(slots, entry, budget, view)
                                })
                                .unwrap();
                            let completed = budget.min(3 - entry as u32);
                            assert_eq!(
                                (exit.pc, exit.instructions),
                                (entry as u64 + u64::from(completed), completed)
                            );
                            let read = u32::from(entry == 0 && completed >= 1);
                            let write = u32::from(entry <= 2 && entry as u32 + completed >= 3);
                            assert_eq!((delta.reads, delta.writes), (read, write));
                            let mut upper = [Value::Integer(2)];
                            delta.apply_upper(&mut upper).unwrap();
                            assert!(
                                matches!(upper[0], Value::Integer(value) if value == if !current && write != 0 { 4 } else { 2 })
                            );
                            assert_eq!(scratch[0].bits, if current && write != 0 { 4 } else { 2 });
                        }
                    }
                }
                drop(code);
                assert_eq!(total.load(Ordering::Relaxed), 0);
                assert_eq!(ledger.current(), 0);
            });
        }

        #[test]
        fn scalar_kernel_declines_bad_views_and_reference_cells_without_a_host() {
            use crate::jit::{backend, resources, work, JitConfig};
            with_closure(|_ctx, closure| {
                let snapshot = Snapshot::new(&closure.prototype(), 64, 1024 * 1024).unwrap();
                let code = backend::compile_leaf_kernel_in(
                    &snapshot,
                    resources::MappingCounter::new(resources::Ledger::new(2 * 1024 * 1024)),
                    128 * 1024,
                    resources::BudgetAllocator(resources::Ledger::new(2 * 1024 * 1024)),
                    work::Limits::from(&JitConfig::default()),
                    backend::Failure::None,
                )
                .unwrap();
                for fault in 0..7 {
                    let mut scratch =
                        vec![abi::Slot::from_value(Value::Integer(2)); code.registers];
                    let before = scratch.clone();
                    let mut cell = abi::Slot::from_value(Value::Integer(7));
                    let mut view = View {
                        version: VERSION,
                        cell: &mut cell,
                        reads: 0,
                        writes: 0,
                        dirty: 0,
                    };
                    match fault {
                        1 => view.version ^= 1,
                        2 => view.cell = std::ptr::null_mut(),
                        3 => view.reads = 1,
                        4 => view.writes = 1,
                        5 => view.dirty = 1,
                        6 => cell.tag = abi::REFERENCE,
                        _ => {}
                    }
                    let pointer = if fault == 0 {
                        std::ptr::null_mut()
                    } else {
                        &mut view
                    };
                    let exit =
                        unsafe { code.invoke_cell_raw(scratch.as_mut_ptr(), 0, 64, pointer) };
                    assert_eq!(
                        (exit.pc, exit.instructions, exit.reason),
                        (0, 0, crate::jit::exits::Kind::Interpreter as u32)
                    );
                    for (actual, expected) in scratch.iter().zip(&before) {
                        assert_eq!((actual.tag, actual.bits), (expected.tag, expected.bits));
                    }
                    assert_eq!(cell.bits, 7);
                }
            });
        }

        #[test]
        fn scalar_kernel_failures_release_resources_and_preserve_live_code() {
            use crate::jit::{backend, resources, work, JitConfig};
            use std::sync::atomic::Ordering;
            with_closure(|_ctx, closure| {
                let snapshot = Snapshot::new(&closure.prototype(), 64, 1024 * 1024).unwrap();
                let metadata = resources::Ledger::new(2 * 1024 * 1024);
                let total = resources::MappingCounter::new(resources::Ledger::new(2 * 1024 * 1024));
                let limits = work::Limits::from(&JitConfig::default());
                let compile = |failure, limits, image_limit| {
                    backend::compile_leaf_kernel_in(
                        &snapshot,
                        total.clone(),
                        image_limit,
                        resources::BudgetAllocator(metadata.clone()),
                        limits,
                        failure,
                    )
                };
                let peer = compile(backend::Failure::None, limits, 128 * 1024).unwrap();
                let before = (
                    metadata.current(),
                    total.load(Ordering::Relaxed),
                    snapshot.operations.allocator().0.current(),
                );
                for (index, failure) in [
                    backend::Failure::Allocate,
                    backend::Failure::Protect,
                    backend::Failure::ProtectAfterFirst,
                    backend::Failure::RefuseSignatures,
                    backend::Failure::RefuseRelocationStorage(false),
                    backend::Failure::RefuseRelocationStorage(true),
                    backend::Failure::RefuseRelocationCopy,
                    backend::Failure::CorruptInlineName,
                    backend::Failure::CorruptInlineSignature,
                ]
                .into_iter()
                .enumerate()
                {
                    let result = compile(failure, limits, 128 * 1024);
                    assert_eq!(
                        result.is_ok(),
                        peer.relocations == 0
                            && matches!(failure, backend::Failure::RefuseRelocationStorage(_)),
                        "injection {index}: {:?}",
                        result.as_ref().err()
                    );
                    drop(result);
                    snapshot.operations.allocator().0.set_limit(1024 * 1024);
                    snapshot.operations.allocator().0.fail_after(usize::MAX);
                    assert_eq!(
                        (
                            metadata.current(),
                            total.load(Ordering::Relaxed),
                            snapshot.operations.allocator().0.current()
                        ),
                        before
                    );
                    let mut scratch = vec![abi::Slot::from_value(Value::Nil); peer.registers];
                    let exit = unsafe {
                        peer.invoke_cell_raw(scratch.as_mut_ptr(), 0, 0, std::ptr::null_mut())
                    };
                    assert_eq!((exit.pc, exit.instructions), (0, 0));
                }
                for limited in [
                    work::Limits {
                        instructions: 0,
                        ..limits
                    },
                    work::Limits {
                        blocks: 0,
                        ..limits
                    },
                    work::Limits {
                        relocations: 0,
                        ..limits
                    },
                ] {
                    let result = compile(backend::Failure::None, limited, 128 * 1024);
                    assert_eq!(
                        result.is_ok(),
                        limited.relocations == 0 && peer.relocations == 0
                    );
                    drop(result);
                    assert_eq!(
                        (
                            metadata.current(),
                            total.load(Ordering::Relaxed),
                            snapshot.operations.allocator().0.current()
                        ),
                        before
                    );
                }
                assert!(compile(backend::Failure::None, limits, 0).is_err());
                assert_eq!(
                    (
                        metadata.current(),
                        total.load(Ordering::Relaxed),
                        snapshot.operations.allocator().0.current()
                    ),
                    before
                );
                drop(peer);
                assert_eq!(metadata.current(), 0);
                assert_eq!(total.load(Ordering::Relaxed), 0);
            });
        }

        #[test]
        fn paired_kernel_owners_charge_both_entries_and_reclaim_failed_second_entries() {
            use crate::jit::{backend, resources, work, JitConfig};
            use std::sync::atomic::Ordering;
            with_closure(|ctx, closure| {
                let snapshot = Snapshot::new(&closure.prototype(), 64, 1024 * 1024).unwrap();
                let metadata = resources::Ledger::new(2 * 1024 * 1024);
                let total = resources::MappingCounter::new(resources::Ledger::new(2 * 1024 * 1024));
                let limits = work::Limits::from(&JitConfig::default());
                let ordinary = backend::compile_in(
                    &snapshot,
                    total.clone(),
                    128 * 1024,
                    resources::BudgetAllocator(metadata.clone()),
                    limits,
                    backend::Failure::None,
                )
                .unwrap();
                let ordinary_bytes = total.load(Ordering::Relaxed);
                let ordinary_metadata_limit = metadata.peak().max(
                    metadata.current()
                        + crate::jit::owner::Shared::<backend::Code>::allocation_bytes(),
                );
                drop(ordinary);
                let compile = |failure, limits, image_limit| {
                    backend::compile_leaf_pair_in(
                        &snapshot,
                        total.clone(),
                        image_limit,
                        resources::BudgetAllocator(metadata.clone()),
                        limits,
                        failure,
                    )
                };
                let initial_snapshot = snapshot.operations.allocator().0.current();
                let fallback = compile(backend::Failure::None, limits, ordinary_bytes).unwrap();
                assert!(fallback.scalar_kernel.is_none());
                assert_ordinary_cell_execution(ctx, closure, &fallback);
                drop(fallback);
                assert_eq!(metadata.current(), 0);
                assert_eq!(total.load(Ordering::Relaxed), 0);
                assert_eq!(total.requested(), 0);
                assert_eq!(
                    snapshot.operations.allocator().0.current(),
                    initial_snapshot
                );
                metadata.set_limit(ordinary_metadata_limit);
                let fallback = compile(backend::Failure::None, limits, 128 * 1024).unwrap();
                assert!(fallback.scalar_kernel.is_none());
                let fallback = crate::jit::owner::Shared::try_new(
                    fallback,
                    resources::BudgetAllocator(metadata.clone()),
                )
                .unwrap();
                assert_ordinary_cell_execution(ctx, closure, &fallback);
                drop(fallback);
                assert_eq!(metadata.current(), 0);
                assert_eq!(total.load(Ordering::Relaxed), 0);
                assert_eq!(total.requested(), 0);
                metadata.set_limit(2 * 1024 * 1024);
                let peer = compile(backend::Failure::None, limits, 128 * 1024).unwrap();
                assert!(peer.scalar_leaf.is_none());
                let kernel = peer.scalar_kernel.as_ref().unwrap();
                assert!(kernel.scalar_leaf.is_some());
                assert!(kernel.scalar_kernel.is_none());
                assert_eq!(kernel.registers, peer.registers);
                assert!(total.load(Ordering::Relaxed) > ordinary_bytes);
                let before = (
                    metadata.current(),
                    total.load(Ordering::Relaxed),
                    total.requested(),
                );
                for failure in [
                    backend::Failure::CorruptInlineName,
                    backend::Failure::CorruptInlineSignature,
                    backend::Failure::RefuseScalarOwnerStorage,
                    backend::Failure::RefuseScalarOwnerAllocation,
                ] {
                    let fallback = compile(failure, limits, 128 * 1024).unwrap();
                    assert!(fallback.scalar_kernel.is_none());
                    let owned = crate::jit::owner::Shared::try_new(
                        fallback,
                        resources::BudgetAllocator(metadata.clone()),
                    )
                    .unwrap();
                    assert_ordinary_cell_execution(ctx, closure, &owned);
                    let mut slots = vec![abi::Slot::from_value(Value::Nil); owned.registers];
                    let exit =
                        unsafe { owned.invoke_raw(slots.as_mut_ptr(), 0, 0, std::ptr::null_mut()) };
                    assert_eq!((exit.pc, exit.instructions), (0, 0));
                    drop(owned);
                    metadata.set_limit(2 * 1024 * 1024);
                    metadata.fail_after(usize::MAX);
                    assert_eq!(
                        (
                            metadata.current(),
                            total.load(Ordering::Relaxed),
                            total.requested()
                        ),
                        before
                    );
                    assert_eq!(
                        snapshot.operations.allocator().0.current(),
                        initial_snapshot
                    );
                    let mut slots = vec![abi::Slot::from_value(Value::Nil); peer.registers];
                    let exit =
                        unsafe { peer.invoke_raw(slots.as_mut_ptr(), 0, 0, std::ptr::null_mut()) };
                    assert_eq!((exit.pc, exit.instructions), (0, 0));
                    let exit = unsafe {
                        kernel.invoke_cell_raw(slots.as_mut_ptr(), 0, 0, std::ptr::null_mut())
                    };
                    assert_eq!((exit.pc, exit.instructions), (0, 0));
                }
                let expansion = work::Expansion::admit(&snapshot, limits).unwrap();
                for limited in [
                    work::Limits {
                        instructions: expansion.instructions,
                        ..limits
                    },
                    work::Limits {
                        blocks: expansion.blocks,
                        ..limits
                    },
                    work::Limits {
                        relocations: peer.relocations,
                        ..limits
                    },
                    work::Limits {
                        instructions: expansion.instructions * 2 - 1,
                        ..limits
                    },
                    work::Limits {
                        blocks: expansion.blocks * 2 - 1,
                        ..limits
                    },
                ] {
                    let fallback = compile(backend::Failure::None, limited, 128 * 1024).unwrap();
                    if limited.relocations != peer.relocations {
                        assert!(fallback.scalar_kernel.is_none());
                    }
                    assert_ordinary_cell_execution(ctx, closure, &fallback);
                    drop(fallback);
                    assert_eq!(
                        (
                            metadata.current(),
                            total.load(Ordering::Relaxed),
                            total.requested()
                        ),
                        before
                    );
                }
                drop(peer);
                let pair = compile(backend::Failure::None, limits, 128 * 1024).unwrap();
                assert!(pair.scalar_kernel.is_some());
                metadata.set_limit(
                    metadata.current()
                        + crate::jit::owner::Shared::<backend::Code>::allocation_bytes()
                        - 1,
                );
                let fallback = pair
                    .into_shared(resources::BudgetAllocator(metadata.clone()))
                    .unwrap();
                assert!(fallback.scalar_kernel.is_none());
                assert_ordinary_cell_execution(ctx, closure, &fallback);
                drop(fallback);
                assert_eq!(metadata.current(), 0);
                assert_eq!(total.load(Ordering::Relaxed), 0);
                assert_eq!(total.requested(), 0);
            });
        }

        fn assert_ordinary_cell_execution<'gc>(
            ctx: crate::Context<'gc>,
            closure: crate::Closure<'gc>,
            code: &crate::jit::backend::Code,
        ) {
            use crate::{
                closure::{UpValue, UpValueState},
                jit::helpers,
                thread::LuaRegisters,
            };
            let cell = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(7)));
            closure.set_upvalue(&ctx, 0, cell);
            let mut pc = 0;
            let mut canonical = vec![Value::Nil; code.registers];
            canonical[0] = Value::Integer(2);
            LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                let mut scratch: Vec<_> = registers
                    .stack_frame
                    .iter()
                    .copied()
                    .map(abi::Slot::from_value)
                    .collect();
                let mut frame = helpers::Frame {
                    ctx,
                    closure,
                    registers: &mut registers,
                    count: helpers::Counts::default(),
                    slot_count: code.registers,
                    panic: None,
                    projection: None,
                };
                let mut host = abi::Host {
                    data: std::ptr::addr_of_mut!(frame).cast(),
                    projection: std::ptr::null_mut(),
                };
                let exit = unsafe { code.invoke_host(&mut scratch, 0, 3, &mut host) };
                assert_eq!((exit.pc, exit.instructions), (3, 3));
                assert_eq!(
                    (frame.count.upvalue_reads, frame.count.upvalue_writes),
                    (1, 1)
                );
                assert!(frame.panic.is_none());
                assert!(matches!(
                    cell.get(),
                    UpValueState::Closed(Value::Integer(9))
                ));
            });
        }

        #[test]
        fn optional_installation_refusals_keep_original_entries_and_live_peer_leases() {
            use crate::jit::{backend, owner, JitConfig, JitMode, Runtime};
            with_closure(|ctx, closure| {
                for (failure, host_limit) in [
                    (backend::Failure::RefuseOwnerStorage, 0),
                    (backend::Failure::RefuseScalarCacheStorage, 0),
                    (backend::Failure::RefuseOwnerStorage, 64),
                    (backend::Failure::RefuseContinuationCacheStorage, 64),
                ] {
                    let closure = if host_limit > 0 {
                        let source = format!(
                            "local sum=0 return function(v) sum=sum+v local a=0 {} end",
                            "a=a+1 ".repeat(128)
                        );
                        let builder = crate::Closure::load(ctx, None, source.as_bytes()).unwrap();
                        let executor = crate::Executor::start(ctx, builder.into(), ());
                        while !executor.step(ctx, &mut crate::Fuel::with(4096)).unwrap() {}
                        executor
                            .take_result::<crate::Closure>(ctx)
                            .unwrap()
                            .unwrap()
                    } else {
                        closure
                    };
                    let runtime = Runtime::new();
                    runtime.0.borrow_mut().configure(JitConfig {
                        mode: JitMode::Auto,
                        ..Default::default()
                    });
                    let snapshot =
                        || Snapshot::new(&closure.prototype(), 512, 1024 * 1024).unwrap();
                    let mut id = 0;
                    loop {
                        id += 1;
                        runtime.0.borrow_mut().tracked.entry(id).or_default();
                        runtime.compile(id, snapshot()).unwrap();
                        let manager = runtime.0.borrow();
                        if manager.code.len() == manager.code.capacity() {
                            break;
                        }
                    }
                    let peer = runtime.lookup(1).unwrap();
                    runtime.test_scalar_kernels(true);
                    runtime.test_activation_host(host_limit);
                    runtime.0.borrow_mut().memory_failure = failure;
                    let refusals = runtime.0.borrow().metadata.0.refusals();
                    id += 1;
                    runtime.0.borrow_mut().tracked.entry(id).or_default();
                    runtime
                        .compile(id, snapshot())
                        .unwrap_or_else(|error| panic!("host_limit={host_limit}: {error}"));
                    let installed = runtime.lookup(id).unwrap();
                    assert!(installed.code.scalar_kernel.is_none());
                    assert!(installed.code.continuations.is_none());
                    assert_ordinary_cell_execution(ctx, closure, &installed.code);
                    assert_ordinary_cell_execution(ctx, closure, &peer.code);
                    assert!(owner::Shared::ptr_eq(
                        &peer.code,
                        &runtime.lookup(1).unwrap().code
                    ));
                    let manager = runtime.0.borrow();
                    assert!(manager.metadata.0.refusals() > refusals);
                    assert_eq!(manager.code.len() as u64, id);
                    assert_eq!(manager.stats.installed_regions, id);
                    assert_eq!(manager.stats.compilation_failures, 0);
                }
            });
        }

        #[cfg(not(miri))]
        #[test]
        fn generated_leaf_keeps_closed_cells_on_original_helpers() {
            use crate::jit::{backend, helpers, resources, work, JitConfig};
            use crate::{
                closure::{UpValue, UpValueState},
                thread::LuaRegisters,
            };
            with_closure(|ctx, closure| {
                let snapshot = Snapshot::new(&closure.prototype(), 64, 1024 * 1024).unwrap();
                let total = resources::MappingCounter::new(resources::Ledger::new(2 * 1024 * 1024));
                let code = backend::compile_leaf_in(
                    &snapshot,
                    total.clone(),
                    128 * 1024,
                    resources::BudgetAllocator(resources::Ledger::new(2 * 1024 * 1024)),
                    work::Limits::from(&JitConfig::default()),
                    backend::Failure::None,
                )
                .unwrap();
                let cell = UpValue::new(&ctx, UpValueState::Closed(Value::Integer(7)));
                closure.set_upvalue(&ctx, 0, cell);
                let mut canonical = vec![Value::Nil; code.registers];
                canonical[0] = Value::Integer(2);
                let mut pc = 0;
                LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                    let mut scratch: Vec<_> = registers
                        .stack_frame
                        .iter()
                        .copied()
                        .map(abi::Slot::from_value)
                        .collect();
                    assert!(Binding::from_origin(
                        registers.projection_origin(cell).unwrap(),
                        &scratch
                    )
                    .is_none());
                    let mut frame = helpers::Frame {
                        ctx,
                        closure,
                        registers: &mut registers,
                        count: helpers::Counts::default(),
                        slot_count: code.registers,
                        panic: None,
                        projection: None,
                    };
                    let mut host = abi::Host {
                        data: std::ptr::addr_of_mut!(frame).cast(),
                        projection: std::ptr::null_mut(),
                    };
                    let exit = unsafe { code.invoke_host(&mut scratch, 0, 64, &mut host) };
                    assert_eq!((exit.pc, exit.instructions), (3, 3));
                    assert_eq!(
                        (
                            frame.count.calls,
                            frame.count.completed,
                            frame.count.upvalue_reads,
                            frame.count.upvalue_writes
                        ),
                        (2, 2, 1, 1)
                    );
                    assert!(frame.panic.is_none());
                    assert!(matches!(
                        cell.get(),
                        UpValueState::Closed(Value::Integer(9))
                    ));
                });
                drop(code);
                assert_eq!(total.load(std::sync::atomic::Ordering::Relaxed), 0);
            });
        }

        #[cfg(not(miri))]
        #[test]
        fn generated_leaf_accounts_all_thunks_and_releases_failed_images() {
            use crate::jit::{backend, resources, work, JitConfig, JitError};
            use std::sync::atomic::Ordering;
            with_closure(|_ctx, closure| {
                let snapshot = Snapshot::new(&closure.prototype(), 64, 1024 * 1024).unwrap();
                let metadata_ledger = resources::Ledger::new(2 * 1024 * 1024);
                let metadata = resources::BudgetAllocator(metadata_ledger.clone());
                let total = resources::MappingCounter::new(resources::Ledger::new(2 * 1024 * 1024));
                let limits = work::Limits::from(&JitConfig::default());
                let compile = |failure, limits, image_limit| {
                    backend::compile_leaf_in(
                        &snapshot,
                        total.clone(),
                        image_limit,
                        metadata.clone(),
                        limits,
                        failure,
                    )
                };
                let peer = compile(backend::Failure::None, limits, 128 * 1024).unwrap();
                let snapshot_baseline = snapshot.operations.allocator().0.current();
                let metadata_baseline = metadata_ledger.current();
                let mapped_baseline = total.load(Ordering::Relaxed);
                for failure in [
                    backend::Failure::Allocate,
                    backend::Failure::Protect,
                    backend::Failure::ProtectAfterFirst,
                    backend::Failure::RefuseSignatures,
                    backend::Failure::RefuseRelocationStorage(false),
                    backend::Failure::RefuseRelocationStorage(true),
                    backend::Failure::RefuseRelocationCopy,
                    backend::Failure::CorruptInlineName,
                    backend::Failure::CorruptInlineSignature,
                    backend::Failure::RequireReleasedWorkspace(snapshot_baseline),
                    backend::Failure::RequireSignatures(snapshot_baseline),
                    backend::Failure::RequireRelocationCopy(snapshot_baseline),
                ] {
                    let result = compile(failure, limits, 128 * 1024);
                    assert_eq!(
                        result.is_ok(),
                        matches!(
                            failure,
                            backend::Failure::RequireReleasedWorkspace(_)
                                | backend::Failure::RequireSignatures(_)
                                | backend::Failure::RequireRelocationCopy(_)
                        )
                    );
                    drop(result);
                    snapshot.operations.allocator().0.set_limit(1024 * 1024);
                    snapshot.operations.allocator().0.fail_after(usize::MAX);
                    assert_eq!(
                        snapshot.operations.allocator().0.current(),
                        snapshot_baseline
                    );
                    assert_eq!(metadata_ledger.current(), metadata_baseline);
                    assert_eq!(total.load(Ordering::Relaxed), mapped_baseline);
                    let mut scratch = vec![abi::Slot::from_value(Value::Nil); peer.registers];
                    assert_eq!(peer.invoke(&mut scratch, 0, 0).instructions, 0);
                }
                for limits in [
                    work::Limits {
                        instructions: 0,
                        ..limits
                    },
                    work::Limits {
                        blocks: 0,
                        ..limits
                    },
                    work::Limits {
                        relocations: 0,
                        ..limits
                    },
                ] {
                    assert!(matches!(
                        compile(backend::Failure::None, limits, 128 * 1024),
                        Err(JitError::ResourceLimit(_))
                    ));
                    assert_eq!(total.load(Ordering::Relaxed), mapped_baseline);
                    assert_eq!(metadata_ledger.current(), metadata_baseline);
                    assert_eq!(
                        snapshot.operations.allocator().0.current(),
                        snapshot_baseline
                    );
                }
                assert!(matches!(
                    compile(backend::Failure::None, limits, 1),
                    Err(JitError::ResourceLimit(_))
                ));
                assert_eq!(total.load(Ordering::Relaxed), mapped_baseline);
                assert_eq!(metadata_ledger.current(), metadata_baseline);
                let ordinary = backend::compile_in(
                    &snapshot,
                    total.clone(),
                    128 * 1024,
                    metadata,
                    limits,
                    backend::Failure::None,
                )
                .unwrap();
                assert!(!ordinary.projected_upvalues);
                drop((ordinary, peer));
                assert_eq!(total.load(Ordering::Relaxed), 0);
                assert_eq!(metadata_ledger.current(), 0);
            });
        }

        #[cfg(not(miri))]
        #[test]
        fn generated_leaf_upper_payloads_commit_only_after_validated_writes() {
            use crate::jit::{backend, helpers, resources, work, JitConfig};
            use crate::thread::LuaRegisters;
            with_closure(|ctx, closure| {
                let snapshot = Snapshot::new(&closure.prototype(), 64, 1024 * 1024).unwrap();
                let pattern = Pattern::recognize(&snapshot).unwrap();
                let total = resources::MappingCounter::new(resources::Ledger::new(2 * 1024 * 1024));
                let code = backend::compile_leaf_in(
                    &snapshot,
                    total.clone(),
                    128 * 1024,
                    resources::BudgetAllocator(resources::Ledger::new(2 * 1024 * 1024)),
                    work::Limits::from(&JitConfig::default()),
                    backend::Failure::None,
                )
                .unwrap();
                for value in [
                    Value::Nil,
                    Value::Boolean(false),
                    Value::Boolean(true),
                    Value::Integer(i64::MIN),
                    Value::Integer(i64::MAX),
                    Value::Number(-0.0),
                    Value::Number(f64::from_bits(0x7ff8_1234_5678_9abc)),
                ] {
                    for entry in [0, 2] {
                        let mut canonical = vec![Value::Nil; code.registers + 1];
                        canonical[0] = if entry == 0 {
                            value
                        } else {
                            Value::Integer(42)
                        };
                        canonical[1 + usize::from(pattern.result.0)] = value;
                        let mut pc = entry;
                        LuaRegisters::projection_split_frame(
                            ctx,
                            &mut pc,
                            &mut canonical,
                            1,
                            |mut registers| {
                                let cell = registers.projection_open_at(ctx, 0);
                                closure.set_upvalue(&ctx, 0, cell);
                                let mut scratch: Vec<_> = registers
                                    .stack_frame
                                    .iter()
                                    .copied()
                                    .map(abi::Slot::from_value)
                                    .collect();
                                let binding = Binding::from_origin(
                                    registers.projection_origin(cell).unwrap(),
                                    &scratch,
                                )
                                .unwrap();
                                let mut frame = helpers::Frame {
                                    ctx,
                                    closure,
                                    registers: &mut registers,
                                    count: helpers::Counts::default(),
                                    slot_count: code.registers,
                                    panic: None,
                                    projection: None,
                                };
                                let before = frame.registers.projection_read(true, 0).unwrap();
                                let (exit, delta) = binding
                                    .with_native(&mut scratch, |slots, view| {
                                        let mut host = abi::Host {
                                            data: std::ptr::addr_of_mut!(frame).cast(),
                                            projection: view.cast(),
                                        };
                                        unsafe { code.invoke_raw(slots, entry, 1, &mut host) }
                                    })
                                    .unwrap();
                                assert_eq!((exit.pc, exit.instructions), (entry as u64 + 1, 1));
                                assert_eq!(
                                    (delta.reads, delta.writes),
                                    if entry == 0 { (1, 0) } else { (0, 1) }
                                );
                                assert_eq!(frame.count.calls, 0);
                                let unchanged = abi::Slot::from_value(
                                    frame.registers.projection_read(true, 0).unwrap(),
                                );
                                let expected_before = abi::Slot::from_value(before);
                                assert_eq!(
                                    (unchanged.tag, unchanged.bits),
                                    (expected_before.tag, expected_before.bits)
                                );
                                let mut upper = [before];
                                delta.apply_upper(&mut upper).unwrap();
                                frame.registers.projection_write(true, 0, upper[0]);
                                let actual = if entry == 0 {
                                    scratch[usize::from(pattern.read.0)]
                                } else {
                                    abi::Slot::from_value(
                                        frame.registers.projection_read(true, 0).unwrap(),
                                    )
                                };
                                let expected = abi::Slot::from_value(value);
                                assert_eq!(
                                    (actual.tag, actual.bits),
                                    (expected.tag, expected.bits)
                                );
                            },
                        );
                    }
                }
                drop(code);
                assert_eq!(total.load(std::sync::atomic::Ordering::Relaxed), 0);
            });
        }

        #[cfg(not(miri))]
        #[test]
        fn generated_leaf_guard_fallbacks_preserve_reference_writes_and_counts() {
            use crate::jit::{backend, helpers, resources, work, JitConfig};
            use crate::thread::LuaRegisters;
            with_closure(|ctx, closure| {
                let snapshot = Snapshot::new(&closure.prototype(), 64, 1024 * 1024).unwrap();
                let pattern = Pattern::recognize(&snapshot).unwrap();
                let total = resources::MappingCounter::new(resources::Ledger::new(2 * 1024 * 1024));
                let code = backend::compile_leaf_in(
                    &snapshot,
                    total.clone(),
                    128 * 1024,
                    resources::BudgetAllocator(resources::Ledger::new(2 * 1024 * 1024)),
                    work::Limits::from(&JitConfig::default()),
                    backend::Failure::None,
                )
                .unwrap();
                for corrupted in [false, true] {
                    let mut canonical = vec![Value::Nil; code.registers];
                    canonical[0] = Value::Integer(7);
                    let table = crate::Table::new(&ctx);
                    canonical[usize::from(pattern.result.0)] = Value::Table(table);
                    let mut pc = if corrupted { 0 } else { 2 };
                    LuaRegisters::with_test_frame(ctx, &mut pc, &mut canonical, |mut registers| {
                        let cell = registers.projection_open_at(ctx, 0);
                        closure.set_upvalue(&ctx, 0, cell);
                        let mut scratch: Vec<_> = registers
                            .stack_frame
                            .iter()
                            .copied()
                            .map(abi::Slot::from_value)
                            .collect();
                        let binding = Binding::from_origin(
                            registers.projection_origin(cell).unwrap(),
                            &scratch,
                        )
                        .unwrap();
                        let entry = *registers.pc;
                        let mut frame = helpers::Frame {
                            ctx,
                            closure,
                            registers: &mut registers,
                            count: helpers::Counts::default(),
                            slot_count: code.registers,
                            panic: None,
                            projection: None,
                        };
                        let (exit, delta) = binding
                            .with_native(&mut scratch, |slots, view| {
                                if corrupted {
                                    unsafe {
                                        (*view).version ^= 1;
                                    }
                                }
                                let mut host = abi::Host {
                                    data: std::ptr::addr_of_mut!(frame).cast(),
                                    projection: view.cast(),
                                };
                                let exit = unsafe { code.invoke_raw(slots, entry, 1, &mut host) };
                                if corrupted {
                                    unsafe {
                                        (*view).version ^= 1;
                                    }
                                }
                                exit
                            })
                            .unwrap();
                        assert_eq!((exit.pc, exit.instructions), (entry as u64 + 1, 1));
                        assert_eq!((delta.reads, delta.writes), (0, 0));
                        assert_eq!((frame.count.calls, frame.count.completed), (1, 1));
                        assert!(frame.panic.is_none());
                        if corrupted {
                            assert_eq!(frame.count.upvalue_reads, 1);
                            assert_eq!(scratch[usize::from(pattern.read.0)].bits, 7);
                        } else {
                            assert_eq!(frame.count.upvalue_writes, 1);
                            assert!(
                                matches!(frame.registers.stack_frame[0], Value::Table(actual) if actual == table)
                            );
                            assert_eq!(scratch[0].tag, abi::REFERENCE);
                        }
                        delta.apply_upper(&mut []).unwrap();
                    });
                }
                drop(code);
                assert_eq!(total.load(std::sync::atomic::Ordering::Relaxed), 0);
            });
        }

        #[cfg(not(miri))]
        #[test]
        fn runtime_leaf_boundary_materializes_open_and_closed_cells_and_exact_counts() {
            use crate::jit::{backend, resources, work, JitConfig, Runtime};
            use crate::{
                closure::{UpValue, UpValueState},
                thread::LuaRegisters,
            };
            with_closure(|ctx, closure| {
                let snapshot = Snapshot::new(&closure.prototype(), 64, 1024 * 1024).unwrap();
                let total = resources::MappingCounter::new(resources::Ledger::new(2 * 1024 * 1024));
                let code = backend::compile_leaf_in(
                    &snapshot,
                    total.clone(),
                    128 * 1024,
                    resources::BudgetAllocator(resources::Ledger::new(2 * 1024 * 1024)),
                    work::Limits::from(&JitConfig::default()),
                    backend::Failure::None,
                )
                .unwrap();
                assert!(code.scalar_leaf.is_some());
                for mode in 0..3 {
                    for budget in [1, 64] {
                        let runtime = Runtime::new();
                        let base = usize::from(mode == 1);
                        let mut canonical = vec![Value::Nil; code.registers + base];
                        canonical[0] = Value::Integer(7);
                        if base != 0 {
                            canonical[base] = Value::Integer(2);
                        }
                        let mut pc = 0;
                        LuaRegisters::projection_split_frame(
                            ctx,
                            &mut pc,
                            &mut canonical,
                            base,
                            |mut registers| {
                                let cell = if mode == 2 {
                                    UpValue::new(&ctx, UpValueState::Closed(Value::Integer(7)))
                                } else {
                                    registers.projection_open_at(ctx, 0)
                                };
                                closure.set_upvalue(&ctx, 0, cell);
                                while *registers.pc < 3 {
                                    let previous = *registers.pc;
                                    let completed = runtime.invoke::<256, false>(
                                        &code,
                                        ctx,
                                        closure,
                                        &mut registers,
                                        budget,
                                    );
                                    assert_eq!(
                                        completed,
                                        (3 - previous).min(budget as usize) as u32
                                    );
                                    assert_eq!(*registers.pc, previous + completed as usize);
                                }
                                let expected = if mode == 1 { 9 } else { 14 };
                                let actual = if mode == 2 {
                                    let UpValueState::Closed(value) = cell.get() else {
                                        panic!("closed cell reopened");
                                    };
                                    value
                                } else {
                                    registers.projection_read(mode == 1, 0).unwrap()
                                };
                                assert!(
                                    matches!(actual, Value::Integer(value) if value == expected)
                                );
                                let stats = runtime.0.borrow().stats;
                                assert_eq!(
                                    (stats.native_upvalue_reads, stats.native_upvalue_writes),
                                    (1, 1)
                                );
                                assert_eq!(
                                    (stats.helper_calls, stats.helper_instructions),
                                    if mode == 2 { (2, 2) } else { (0, 0) }
                                );
                                assert_eq!(stats.native_instructions, 3);
                                assert_eq!(stats.native_entries, if budget == 1 { 3 } else { 1 });
                            },
                        );
                    }
                }
                drop(code);
                assert_eq!(total.load(std::sync::atomic::Ordering::Relaxed), 0);
            });
        }

        #[cfg(not(miri))]
        #[test]
        fn runtime_leaf_arithmetic_guard_preserves_prefix_without_a_write() {
            use crate::jit::{backend, resources, work, JitConfig, Runtime};
            use crate::thread::LuaRegisters;
            with_closure(|ctx, closure| {
                let snapshot = Snapshot::new(&closure.prototype(), 64, 1024 * 1024).unwrap();
                let total = resources::MappingCounter::new(resources::Ledger::new(2 * 1024 * 1024));
                let code = backend::compile_leaf_in(
                    &snapshot,
                    total.clone(),
                    128 * 1024,
                    resources::BudgetAllocator(resources::Ledger::new(2 * 1024 * 1024)),
                    work::Limits::from(&JitConfig::default()),
                    backend::Failure::None,
                )
                .unwrap();
                let runtime = Runtime::new();
                let mut canonical = vec![Value::Nil; code.registers + 1];
                canonical[0] = Value::Integer(7);
                let table = crate::Table::new(&ctx);
                canonical[1] = Value::Table(table);
                let mut pc = 0;
                LuaRegisters::projection_split_frame(
                    ctx,
                    &mut pc,
                    &mut canonical,
                    1,
                    |mut registers| {
                        let cell = registers.projection_open_at(ctx, 0);
                        closure.set_upvalue(&ctx, 0, cell);
                        assert_eq!(
                            runtime.invoke::<256, false>(&code, ctx, closure, &mut registers, 64),
                            1
                        );
                        assert_eq!(*registers.pc, 1);
                        assert!(matches!(
                            registers.projection_read(true, 0),
                            Some(Value::Integer(7))
                        ));
                        assert!(
                            matches!(registers.stack_frame[0], Value::Table(actual) if actual == table)
                        );
                        let stats = runtime.0.borrow().stats;
                        assert_eq!(
                            (stats.native_upvalue_reads, stats.native_upvalue_writes),
                            (1, 0)
                        );
                        assert_eq!(
                            (
                                stats.helper_calls,
                                stats.native_instructions,
                                stats.guard_exits
                            ),
                            (0, 1, 1)
                        );
                    },
                );
                drop(code);
                assert_eq!(total.load(std::sync::atomic::Ordering::Relaxed), 0);
            });
        }

        struct ExecutorRun {
            result: String,
            slices: Vec<(bool, crate::ExecutorMode, i32, u64)>,
            stats: crate::JitStats,
            scalar_native_counts: (u64, u64),
        }

        fn executor_run(source: &str, native: bool, budget: i32, kernels: bool) -> ExecutorRun {
            use crate::jit::{JitConfig, JitMode};
            let mut lua = crate::Lua::core();
            lua.load_debug();
            lua.set_jit_config(JitConfig {
                mode: if native { JitMode::Auto } else { JitMode::Off },
                hot_threshold: u32::MAX,
                ..Default::default()
            })
            .unwrap();
            let executor = lua.enter(|ctx| {
                ctx.jit().0.borrow_mut().scalar_leaves = native && !kernels;
                ctx.jit().0.borrow_mut().scalar_kernels = native && kernels;
                let closure =
                    crate::Closure::load(ctx, Some("scalar-cell-executor"), source.as_bytes())
                        .unwrap();
                ctx.stash(crate::Executor::start(ctx, closure.into(), ()))
            });
            if native {
                while lua.prepare_jit().unwrap() != 0 {}
                lua.enter(|ctx| {
                    assert!(ctx.jit().0.borrow().code.values().any(|cached| if kernels {
                        cached.code.scalar_kernel.is_some()
                    } else {
                        cached.code.scalar_leaf.is_some()
                    }));
                });
            }
            let mut slices = Vec::new();
            for _ in 0..20_000 {
                let (done, mode, fuel) = lua.enter(|ctx| {
                    let executor = ctx.fetch(&executor);
                    let mut fuel = crate::Fuel::with(budget);
                    (
                        executor.step(ctx, &mut fuel).unwrap(),
                        executor.mode(),
                        fuel.remaining(),
                    )
                });
                slices.push((done, mode, fuel, lua.jit_stats().total_dispatches));
                lua.gc_collect();
                if done {
                    let result = lua
                        .try_enter(|ctx| ctx.fetch(&executor).take_result::<String>(ctx)?)
                        .unwrap();
                    let stats = lua.jit_stats();
                    let scalar_native_counts =
                        lua.enter(|ctx| ctx.jit().0.borrow().scalar_native_counts);
                    lua.clear_jit_cache();
                    assert_eq!(lua.jit_stats().code_bytes, 0);
                    return ExecutorRun {
                        result,
                        slices,
                        stats,
                        scalar_native_counts,
                    };
                }
            }
            panic!("scalar-cell executor did not finish: native={native}, budget={budget}, source={source}");
        }

        #[test]
        fn executor_leaf_preserves_fuel_gc_errors_closed_cells_and_hooks() {
            compare_executor_leaves(false);
        }

        #[test]
        fn executor_kernel_preserves_fuel_gc_errors_closed_cells_and_hooks() {
            compare_executor_leaves(true);
        }

        #[test]
        fn executor_kernel_keeps_reference_results_on_the_original_native_set_entry() {
            let source = "local sum=1 local function add(v) sum=sum+v end local t=setmetatable({}, {__add=function() return {} end}) add(t) return type(sum)";
            for budget in [1, 17, 4096] {
                let reference = executor_run(source, false, budget, true);
                let native = executor_run(source, true, budget, true);
                assert_eq!(native.result, "table");
                assert_eq!(native.result, reference.result);
                assert_eq!(native.slices, reference.slices);
                assert_eq!(native.stats.native_upvalue_reads, 3);
                assert_eq!(native.scalar_native_counts.0, 1);
                assert_eq!(native.stats.native_upvalue_writes, 1);
                assert_eq!(native.scalar_native_counts.1, 0);
            }
        }

        fn compare_executor_leaves(kernels: bool) {
            for (source, fast_writes) in [
                ("local sum=0 local function add(v) sum=sum+v end for i=1,200 do add(i) end return tostring(sum)", Some(200)),
                ("local sum=0 local function sub(v) sum=sum-v end for i=1,200 do sub(i) end return tostring(sum)", Some(200)),
                ("local sum=1 local function mul(v) sum=sum*v end for i=1,200 do mul(1) end return tostring(sum)", Some(200)),
                ("local sum=0.0 local function add(v) sum=sum+v end for i=1,200 do add(0.5) end return tostring(sum)", Some(200)),
                ("local sum=0 local function add(v) sum=sum+1 end for i=1,200 do add(i) end return tostring(sum)", Some(200)),
                ("local sum=7 local function add(v) sum=sum+v end local ok=pcall(add,{}) return tostring(sum)..':'..tostring(ok)", Some(0)),
                ("local function factory() local sum=0 return function(v) sum=sum+v end end local add=factory() for i=1,200 do add(i) end return 'closed'", Some(0)),
                ("local sum=0 local function add(v) sum=sum+v end for i=1,20 do add(i) end debug.setupvalue(add,1,1000) for i=1,20 do add(i) end return tostring(sum)", Some(40)),
                ("local sum=0 local other=1000 local function add(v) sum=sum+v end local function second(v) other=other+v end add(1) debug.upvaluejoin(add,1,second,1) for i=1,20 do add(i) end return tostring(sum)..':'..tostring(other)", Some(21)),
                ("local sum=0 local function add(v) sum=sum+v end add(1) debug.setupvalue(add,1,{}) local ok=pcall(add,1) return type(sum)..':'..tostring(ok)", Some(1)),
                ("local sum=0 local function add(v) sum=sum+v end local events=0 debug.sethook(function() events=events+1 end,'',7) for i=1,20 do add(i) end debug.sethook() return tostring(sum)..':'..tostring(events)", None),
            ] {
                for budget in [1, 17, 4096] {
                    let reference = executor_run(source, false, budget, kernels);
                    let native = executor_run(source, true, budget, kernels);
                    assert_eq!(native.result, reference.result, "result: {source}, budget={budget}");
                    assert_eq!(native.slices, reference.slices, "slices/fuel/dispatches: {source}, budget={budget}");
                    assert_eq!(native.stats.compilation_failures, 0);
                    if let Some(writes) = fast_writes {
                        assert_eq!(native.scalar_native_counts.1, writes);
                        assert!(native.stats.native_upvalue_writes >= writes);
                    } else {
                        assert!(native.stats.helper_instructions > 0);
                    }
                    assert_eq!(reference.stats.native_entries, 0);
                    assert_eq!(reference.scalar_native_counts, (0, 0));
                }
            }
        }
    }

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
    fn typed_runtime_commit_checks_upper_target_before_mutation() {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let mut scratch = [abi::Slot::from_value(Value::Nil)];
            let binding =
                Binding::from_origin(Origin::Upper(0, Value::Integer(7)), &scratch).unwrap();
            let (_, delta) = binding
                .with_native(&mut scratch, |_, view| unsafe {
                    (*view)
                        .cell
                        .write(abi::Slot::from_value(Value::Integer(42)));
                    (*view).writes = 1;
                    (*view).dirty = 1;
                })
                .unwrap();
            let mut canonical = [Value::Integer(7), Value::Integer(9)];
            let mut pc = 0;
            crate::thread::LuaRegisters::projection_split_frame(
                ctx,
                &mut pc,
                &mut canonical,
                1,
                |mut registers| {
                    delta.apply_registers(&mut registers).unwrap();
                    assert!(matches!(
                        registers.projection_read(true, 0),
                        Some(Value::Integer(42))
                    ));
                    assert!(matches!(registers.stack_frame[0], Value::Integer(9)));
                },
            );
            crate::thread::LuaRegisters::with_test_frame(
                ctx,
                &mut pc,
                &mut canonical,
                |mut registers| {
                    assert_eq!(
                        delta.apply_registers(&mut registers),
                        Err(Error::TargetRange)
                    );
                    assert!(matches!(registers.stack_frame[0], Value::Integer(42)));
                    assert!(matches!(registers.stack_frame[1], Value::Integer(9)));
                },
            );
        });
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
