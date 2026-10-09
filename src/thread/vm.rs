use allocator_api2::vec;
use ottavino_gc_arena::{allocator_api::MetricsAlloc, lock::Lock};

use crate::{
    compiler::LineNumber,
    meta_ops::{self, ConcatMetaResult, MetaResult},
    opcode::{Operation, RCIndex},
    table::RawTable,
    thread::thread::MetaReturn,
    types::{RegisterIndex, UpValueDescriptor, VarCount},
    Closure, Constant, Context, Function, String, Table, Value,
};

use super::{thread::LuaFrame, VMError};

#[cfg(test)]
mod constant_add;
#[cfg(test)]
mod constants;
mod dispatch;

// Runs the VM for the given number of instructions or until the current LuaFrame may have been
// changed.
//
// Returns the number of instructions that were run.
/// The source line an opcode index belongs to.
///
/// Only ever called with a hook installed, so the binary search is off the normal path entirely.
fn line_of(proto: &crate::FunctionPrototype<'_>, pc: usize) -> LineNumber {
    match proto
        .opcode_line_numbers
        .binary_search_by_key(&pc, |(opi, _)| *opi)
    {
        Ok(i) => proto.opcode_line_numbers[i].1,
        Err(0) => LineNumber(0),
        Err(i) => proto.opcode_line_numbers[i - 1].1,
    }
}

/// A catchable runtime error carrying the `chunk:line` of the instruction before `pc`.
///
/// The same position the executor attaches to a [`VMError`], for the errors the VM raises without
/// leaving the opcode loop.
///
/// Kept out of line because it is a cold path — one `format!` and an allocation per raised error —
/// inside `run_vm`, which is the interpreter's dispatch loop.
#[cold]
#[inline(never)]
fn positioned_error<'gc>(
    proto: &crate::FunctionPrototype<'gc>,
    pc: usize,
    message: &'static str,
) -> crate::Error<'gc> {
    let at = format!(
        "{}:{}",
        proto.chunk_name.display_lossy(),
        line_of(proto, pc.saturating_sub(1))
    );
    crate::RuntimeError::new(anyhow::anyhow!(message).context(at)).into()
}

#[cfg(all(
    feature = "jit",
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[inline(always)]
#[cfg(test)]
pub(super) fn try_scalar_activation<'gc>(
    ctx: Context<'gc>,
    frame: &mut LuaFrame<'gc, '_>,
    budget: u32,
) -> Option<(u32, crate::types::RegisterIndex, crate::types::VarCount)> {
    let closure = frame.closure();
    let prototype = closure.prototype();
    if prototype.opcodes.len() != 4 {
        return None;
    }
    ctx.clear_hook_at(frame.frame_depth());
    if ctx.hook_enabled() {
        return None;
    }
    let completed =
        ctx.jit()
            .try_scalar_activation(ctx, closure, &mut frame.registers(), budget)?;
    let Operation::Return { start, count } = prototype.opcodes[3].decode() else {
        panic!("scalar activation has no canonical return");
    };
    *frame.registers().pc += 1;
    Some((completed, start, count))
}

#[cfg(all(test, feature = "jit"))]
pub(crate) struct NativeResume<'gc> {
    runtime: crate::jit::Runtime,
    closure: crate::Closure<'gc>,
    source: u64,
    frame: (usize, usize),
    pc: usize,
    instructions: u32,
    code: Option<crate::jit::Prepared>,
    #[cfg(all(
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    paired: bool,
}

#[cfg(all(
    test,
    feature = "jit",
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
impl<'gc> NativeResume<'gc> {
    pub(crate) fn new(
        ctx: Context<'gc>,
        closure: crate::Closure<'gc>,
        source: u64,
        frame: (usize, usize),
        pc: usize,
        instructions: u32,
        code: crate::jit::Prepared,
    ) -> Self {
        Self {
            runtime: ctx.jit().clone(),
            closure,
            source,
            frame,
            pc,
            instructions,
            code: Some(code),
            paired: false,
        }
    }
}

#[inline(always)]
pub(super) fn run_vm<'gc>(
    ctx: Context<'gc>,
    lua_frame: LuaFrame<'gc, '_>,
    max_instructions: u32,
) -> Result<u32, VMError> {
    run_vm_slice(ctx, lua_frame, max_instructions)
}

#[cfg(all(
    test,
    feature = "jit",
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(super) fn resume_vm<'gc>(
    ctx: Context<'gc>,
    mut lua_frame: LuaFrame<'gc, '_>,
    max_instructions: u32,
    mut resume: NativeResume<'gc>,
) -> Result<u32, VMError> {
    let mut local = crate::jit::PairScope::default();
    resume.paired = lua_frame.pair_handoff.is_some();
    let scope = lua_frame.pair_handoff.take().unwrap_or(&mut local);
    assert!(scope.resume.is_none());
    assert!(scope.handoff.is_none());
    scope.resume = Some(resume);
    run_vm_slice(
        ctx,
        LuaFrame {
            pair_handoff: Some(scope),
            state: lua_frame.state,
            stack: lua_frame.stack,
            fuel: lua_frame.fuel,
        },
        max_instructions,
    )
}

fn run_vm_slice<'gc>(
    ctx: Context<'gc>,
    mut lua_frame: LuaFrame<'gc, '_>,
    max_instructions: u32,
) -> Result<u32, VMError> {
    #[cfg(all(
        test,
        feature = "jit",
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    let resume = {
        let resume = lua_frame
            .pair_handoff
            .as_mut()
            .and_then(|scope| scope.resume.take());
        if resume.as_ref().is_some_and(|resume| !resume.paired) {
            lua_frame.pair_handoff = None;
        }
        resume
    };
    #[cfg(all(
        test,
        feature = "jit",
        not(all(
            not(miri),
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))
    ))]
    let resume: Option<NativeResume<'gc>> = None;
    #[cfg(all(test, feature = "jit"))]
    assert!(resume
        .as_ref()
        .is_none_or(|resume| resume.instructions < max_instructions));
    if max_instructions == 0 {
        return Ok(0);
    }

    let current_function = lua_frame.closure();
    let current_prototype = current_function.prototype();
    let current_upvalues = current_function.upvalues();
    #[cfg(all(test, feature = "jit"))]
    let prefix_instructions = if let Some(resume) = &resume {
        assert!(crate::jit::RuntimeOwner::ptr_eq(
            &ctx.jit().0,
            &resume.runtime.0
        ));
        assert_eq!(
            (
                std::ptr::from_ref(&*lua_frame.state) as usize,
                lua_frame.frame_depth()
            ),
            resume.frame
        );
        assert!(ottavino_gc_arena::Gc::ptr_eq(
            current_function.into_inner(),
            resume.closure.into_inner()
        ));
        assert_eq!(*lua_frame.registers().pc, resume.pc);
        assert_eq!(
            ctx.jit_registry().borrow().identity(ctx, current_prototype),
            Some(resume.source)
        );
        resume.instructions
    } else {
        0
    };
    #[cfg(all(not(test), feature = "jit"))]
    let prefix_instructions = 0;
    // Suppression ends when execution returns to the depth that fired the hook, which is exactly
    // when the hook's own frames are gone. Checked once per slice, before the register borrow.
    ctx.clear_hook_at(lua_frame.frame_depth());

    // Read once per slice, not per instruction: with no hook installed this leaves a single
    // always-false branch in the loop, which is what Phase 3 set out to measure.
    let hook_enabled = ctx.hook_enabled();
    #[cfg(all(
        feature = "jit",
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    let pair_handoff = !hook_enabled && lua_frame.pair_handoff.is_some();
    let frame_depth = lua_frame.frame_depth();

    #[cfg(all(test, feature = "jit"))]
    let mock = (!hook_enabled)
        .then(|| ctx.jit().mock_snapshot(&current_prototype))
        .flatten();

    #[cfg(feature = "jit")]
    let native_id = if !hook_enabled && ctx.jit().active() {
        ctx.jit_registry().borrow().identity(ctx, current_prototype)
    } else {
        None
    };
    #[cfg(all(test, feature = "jit"))]
    let mut skip_native_first = resume.is_some();
    #[cfg(all(test, feature = "jit"))]
    let native_code = if let Some(mut resume) = resume {
        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        {
            ctx.jit()
                .resume_lease(resume.source, resume.code.take().unwrap())
                .filter(|_| !hook_enabled)
        }
        #[cfg(not(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )))]
        {
            let _ = resume.code.take();
            None
        }
    } else {
        native_id.and_then(|id| ctx.jit().lookup(id))
    };
    #[cfg(all(not(test), feature = "jit"))]
    let native_code = native_id.and_then(|id| ctx.jit().lookup(id));
    #[cfg(feature = "jit")]
    let native_dispatch = dispatch::Dispatch::new(native_id, native_code, hook_enabled);
    #[cfg(not(feature = "jit"))]
    let native_dispatch = dispatch::Dispatch::interpreted(hook_enabled);
    #[cfg(feature = "jit")]
    let mut native_instructions = prefix_instructions;
    #[cfg(feature = "jit")]
    let mut interpreter_stats = ctx.jit().interpreter_stats();
    #[cfg(feature = "jit")]
    if hook_enabled && ctx.jit().active() {
        let mut manager = ctx.jit().0.borrow_mut();
        manager.stats.hook_exits = manager.stats.hook_exits.saturating_add(1);
    }

    #[cfg(all(
        feature = "jit",
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    let observe_pairs = !hook_enabled && ctx.jit().call_pairs_enabled();

    #[cfg(all(
        feature = "jit",
        not(miri),
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    let mut pair_scope = lua_frame.pair_handoff.take();
    let mut registers = lua_frame.registers();
    #[cfg(not(feature = "jit"))]
    let mut instructions_run = 0;
    #[cfg(feature = "jit")]
    let mut instructions_run = prefix_instructions;

    #[cfg_attr(feature = "jit", inline(always))]
    fn get_rc<'gc>(
        stack_frame: &[Value<'gc>],
        constants: &[Constant<String<'gc>>],
        rc: RCIndex,
    ) -> Value<'gc> {
        match rc {
            RCIndex::Register(r) => stack_frame[r.0 as usize],
            RCIndex::Constant(c) => constants[c.0 as usize].into(),
        }
    }

    loop {
        #[cfg(feature = "jit")]
        let run_native = {
            #[cfg(test)]
            {
                !std::mem::replace(&mut skip_native_first, false)
            }
            #[cfg(not(test))]
            {
                true
            }
        };
        #[cfg(all(test, feature = "jit"))]
        if run_native {
            #[cfg(all(test, feature = "jit"))]
            if let Some(snapshot) = &mock {
                let completed = ctx.jit().run_mock(snapshot, &mut registers);
                interpreter_stats.dispatches += completed;
                instructions_run += completed;
                if instructions_run >= max_instructions {
                    break;
                }
                if completed != 0 {
                    continue;
                }
            }
        }
        match &native_dispatch {
            #[cfg(feature = "jit")]
            dispatch::Dispatch::Compiled(code) if run_native => {
                let completed = ctx.jit().run(
                    code,
                    ctx,
                    current_function,
                    &mut registers,
                    max_instructions - instructions_run,
                );
                instructions_run += completed;
                native_instructions += completed;
                if instructions_run >= max_instructions {
                    break;
                }
                #[cfg(test)]
                let transition = ctx
                    .jit()
                    .call_transition(code, *registers.pc)
                    .or_else(|| current_prototype.opcodes[*registers.pc].call_transition());
                #[cfg(not(test))]
                let transition = current_prototype.opcodes[*registers.pc].call_transition();
                if let Some(transition) = transition {
                    #[cfg(all(
                        feature = "jit",
                        not(miri),
                        target_os = "linux",
                        any(target_arch = "x86_64", target_arch = "aarch64")
                    ))]
                    if pair_handoff
                        && matches!(transition, crate::opcode::CallTransition::Call { .. })
                    {
                        if let Some(pair) = ctx.jit().prepare_call_at(
                            ctx,
                            current_function,
                            &registers,
                            *registers.pc,
                            pair_scope.as_deref_mut().unwrap(),
                        ) {
                            drop(registers);
                            if lua_frame.pair_fixed_stack() {
                                pair_scope.as_deref_mut().unwrap().handoff = Some(pair);
                                break;
                            }
                            registers = lua_frame.registers();
                        }
                    }
                    *registers.pc += 1;
                    interpreter_stats.dispatches += 1;
                    match transition {
                        crate::opcode::CallTransition::Call {
                            func,
                            args,
                            returns,
                        } => {
                            #[cfg(all(
                                feature = "jit",
                                not(miri),
                                target_os = "linux",
                                any(target_arch = "x86_64", target_arch = "aarch64")
                            ))]
                            if observe_pairs {
                                ctx.jit().observe_call(
                                    ctx,
                                    current_function,
                                    &registers,
                                    func,
                                    args,
                                    returns,
                                );
                            }
                            lua_frame.call_function(ctx, func, args, returns)?;
                        }
                        crate::opcode::CallTransition::TailCall { func, args } => {
                            lua_frame.tail_call_function(ctx, func, args)?;
                        }
                        crate::opcode::CallTransition::Return { start, count } => {
                            lua_frame.return_upper(&ctx, start, count)?;
                        }
                    }
                    break;
                }
            }
            #[cfg(feature = "jit")]
            dispatch::Dispatch::Observing(id) if run_native => ctx.jit().observe(*id),
            dispatch::Dispatch::Hooked => {
                let line = ctx
                    .hook_line()
                    .then(|| line_of(&current_prototype, *registers.pc));
                let fire_line = line.is_some_and(|line| ctx.hook_line_changed(frame_depth, line.0));
                let fire_count = ctx.hook_tick();

                if fire_line || fire_count {
                    let (event, line) = if fire_line {
                        ("line", line)
                    } else {
                        ("count", None)
                    };
                    if lua_frame.fire_hook(ctx, event, line)? {
                        // Fired: a call frame is on top, so this slice is over — the same exit a
                        // metamethod call takes.
                        break;
                    }
                    // Declined, but the frame was still borrowed mutably to find that out.
                    registers = lua_frame.registers();
                }
            }
            dispatch::Dispatch::Interpreted => {}
            #[cfg(feature = "jit")]
            _ => {}
        }

        let op = current_prototype.opcodes[*registers.pc].decode();
        #[cfg(feature = "jit")]
        {
            interpreter_stats.dispatches += 1;
        }
        *registers.pc += 1;

        match op {
            Operation::Move { dest, source } => {
                registers.stack_frame[dest.0 as usize] = registers.stack_frame[source.0 as usize];
            }

            Operation::LoadConstant { dest, constant } => {
                registers.stack_frame[dest.0 as usize] =
                    current_prototype.constants[constant.0 as usize].into();
            }

            Operation::LoadBool {
                dest,
                value,
                skip_next,
            } => {
                registers.stack_frame[dest.0 as usize] = Value::Boolean(value);
                if skip_next {
                    *registers.pc += 1;
                }
            }

            Operation::LoadNil { dest, count } => {
                for i in usize::from(dest.0)..usize::from(dest.0) + usize::from(count) {
                    registers.stack_frame[i] = Value::Nil;
                }
            }

            Operation::NewTable {
                dest,
                array_size,
                map_size,
            } => {
                let table = Table::from_parts(
                    &ctx,
                    RawTable::with_capacity(&ctx, array_size as usize, map_size as usize),
                    None,
                );
                registers.stack_frame[dest.0 as usize] = Value::Table(table);
            }

            Operation::GetTable { dest, table, key } => {
                let table = registers.stack_frame[table.0 as usize];
                let key = get_rc(&registers.stack_frame, &current_prototype.constants, key);
                match meta_ops::index(ctx, table, key)? {
                    MetaResult::Value(v) => {
                        registers.stack_frame[dest.0 as usize] = v;
                    }
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::SetTable { table, key, value } => {
                let table = registers.stack_frame[table.0 as usize];
                let key = get_rc(&registers.stack_frame, &current_prototype.constants, key);
                let value = get_rc(&registers.stack_frame, &current_prototype.constants, value);
                if let Some(call) = meta_ops::new_index(ctx, table, key, value)? {
                    lua_frame.call_meta_function(
                        ctx,
                        call.function,
                        &call.args,
                        MetaReturn::None,
                    )?;
                    break;
                }
            }

            Operation::GetUpTable { dest, table, key } => {
                let table = registers.get_upvalue(&ctx, current_upvalues[table.0 as usize].get());
                let key = get_rc(&registers.stack_frame, &current_prototype.constants, key);
                match meta_ops::index(ctx, table, key)? {
                    MetaResult::Value(v) => {
                        registers.stack_frame[dest.0 as usize] = v;
                    }
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::SetUpTable { table, key, value } => {
                let table = registers.get_upvalue(&ctx, current_upvalues[table.0 as usize].get());
                let key = get_rc(&registers.stack_frame, &current_prototype.constants, key);
                let value = get_rc(&registers.stack_frame, &current_prototype.constants, value);
                if let Some(call) = meta_ops::new_index(ctx, table, key, value)? {
                    lua_frame.call_meta_function(
                        ctx,
                        call.function,
                        &call.args,
                        MetaReturn::None,
                    )?;
                    break;
                }
            }

            Operation::SetList { base, count } => {
                lua_frame.set_table_list(&ctx, base, count)?;
                registers = lua_frame.registers();
            }

            Operation::Call {
                func,
                args,
                returns,
            } => {
                #[cfg(all(
                    feature = "jit",
                    not(miri),
                    target_os = "linux",
                    any(target_arch = "x86_64", target_arch = "aarch64")
                ))]
                if pair_handoff {
                    let pc = *registers.pc - 1;
                    if let Some(pair) = ctx.jit().prepare_call_at(
                        ctx,
                        current_function,
                        &registers,
                        pc,
                        pair_scope.as_deref_mut().unwrap(),
                    ) {
                        drop(registers);
                        if lua_frame.pair_fixed_stack() {
                            *lua_frame.registers().pc = pc;
                            interpreter_stats.dispatches -= 1;
                            pair_scope.as_deref_mut().unwrap().handoff = Some(pair);
                            break;
                        }
                        registers = lua_frame.registers();
                    }
                }
                #[cfg(all(
                    feature = "jit",
                    not(miri),
                    target_os = "linux",
                    any(target_arch = "x86_64", target_arch = "aarch64")
                ))]
                if observe_pairs {
                    ctx.jit()
                        .observe_call(ctx, current_function, &registers, func, args, returns);
                }
                lua_frame.call_function(ctx, func, args, returns)?;
                break;
            }

            Operation::TailCall { func, args } => {
                lua_frame.tail_call_function(ctx, func, args)?;
                break;
            }

            Operation::Return { start, count } => {
                lua_frame.return_upper(&ctx, start, count)?;
                break;
            }

            Operation::VarArgs { dest, count } => {
                lua_frame.varargs(dest, count)?;
                registers = lua_frame.registers();
            }

            Operation::MarkToBeClosed { source } => {
                let value = registers.stack_frame[source.0 as usize];
                // Checked here rather than at close time, so a mistake is reported where the
                // variable is declared instead of much later when the block ends.
                if !matches!(value, Value::Nil | Value::Boolean(false))
                    && meta_ops::get_metamethod(ctx, value, crate::MetaMethod::Close).is_none()
                {
                    return Err(VMError::BadCloseValue);
                }
                registers.mark_to_be_closed(source);
            }

            Operation::Jump {
                offset,
                close_upvalues,
            } => {
                *registers.pc = add_offset(*registers.pc, offset);
                if let Some(r) = close_upvalues.to_u8() {
                    registers.close_upvalues(&ctx, RegisterIndex(r));
                    // Handlers cannot run here: the VM is mid-instruction and holds the frame.
                    // Hand them to the executor as a sequence and resume at the new pc after.
                    let to_close = registers.take_to_be_closed(RegisterIndex(r));
                    if !to_close.is_empty() {
                        lua_frame.push_close_sequence(ctx, to_close);
                        break;
                    }
                }
            }

            Operation::Test { value, is_true } => {
                let value = registers.stack_frame[value.0 as usize];
                if value.to_bool() == is_true {
                    *registers.pc += 1;
                }
            }

            Operation::TestSet {
                dest,
                value,
                is_true,
            } => {
                let value = registers.stack_frame[value.0 as usize];
                if value.to_bool() == is_true {
                    *registers.pc += 1;
                } else {
                    registers.stack_frame[dest.0 as usize] = value;
                }
            }

            Operation::Closure { proto, dest } => {
                let proto = current_prototype.prototypes[proto.0 as usize];
                let mut upvalues =
                    vec::Vec::with_capacity_in(proto.upvalues.len(), MetricsAlloc::new(&ctx));
                for &desc in proto.upvalues.iter() {
                    match desc {
                        UpValueDescriptor::Environment => {
                            return Err(VMError::BadEnvUpValue.into());
                        }
                        UpValueDescriptor::ParentLocal(reg) => {
                            upvalues.push(Lock::new(registers.open_upvalue(&ctx, reg)));
                        }
                        UpValueDescriptor::Outer(uvindex) => {
                            // Its own slot, holding the same upvalue: the two closures share the
                            // variable, but repointing one slot later must not repoint the other.
                            upvalues.push(Lock::new(current_upvalues[uvindex.0 as usize].get()));
                        }
                    }
                }

                let closure = Closure::from_parts(&ctx, proto, upvalues);
                registers.stack_frame[dest.0 as usize] =
                    Value::Function(Function::Closure(closure));
            }

            Operation::NumericForPrep { base, jump } => {
                // Rejected at preparation, where PUC-Lua's `forprep` rejects it, and for both the
                // integer and the float loop: a zero step never reaches the limit, so the loop
                // would otherwise run forever.
                if registers.stack_frame[base.0 as usize + 2].to_number() == Some(0.0) {
                    let pc = *registers.pc;
                    lua_frame.raise(positioned_error(
                        &current_prototype,
                        pc,
                        "'for' step is zero",
                    ));
                    break;
                }

                registers.stack_frame[base.0 as usize] = raw_subtract(
                    registers.stack_frame[base.0 as usize],
                    registers.stack_frame[base.0 as usize + 2],
                )
                .ok_or_else(|| {
                    VMError::BadForLoopPrep(
                        registers.stack_frame[base.0 as usize].type_name(),
                        registers.stack_frame[base.0 as usize + 2].type_name(),
                    )
                })?;
                *registers.pc = add_offset(*registers.pc, jump);
            }

            Operation::NumericForLoop { base, jump } => {
                match (
                    registers.stack_frame[base.0 as usize],
                    registers.stack_frame[base.0 as usize + 1],
                    registers.stack_frame[base.0 as usize + 2],
                ) {
                    (Value::Integer(index), Value::Integer(limit), Value::Integer(step)) => {
                        let (index, overflow) = index.overflowing_add(step);
                        registers.stack_frame[base.0 as usize] = Value::Integer(index);

                        let past_end = overflow
                            || if step < 0 {
                                index < limit
                            } else {
                                index > limit
                            };
                        if !past_end {
                            *registers.pc = add_offset(*registers.pc, jump);
                            registers.stack_frame[base.0 as usize + 3] = Value::Integer(index);
                        }
                    }
                    (Value::Integer(index), limit, Value::Integer(step)) => {
                        if let Some(limit) = limit.to_number() {
                            let (index, overflow) = index.overflowing_add(step);
                            registers.stack_frame[base.0 as usize] = Value::Integer(index);

                            let past_end = overflow
                                || if step < 0 {
                                    !(index as f64 >= limit)
                                } else {
                                    !(index as f64 <= limit)
                                };
                            if !past_end {
                                *registers.pc = add_offset(*registers.pc, jump);
                                registers.stack_frame[base.0 as usize + 3] = Value::Integer(index);
                            }
                        } else {
                            return Err(VMError::BadForLoop(
                                "integer",
                                limit.type_name(),
                                "integer",
                            ));
                        }
                    }
                    (index, limit, step) => {
                        if let (Some(index), Some(limit), Some(step)) =
                            (index.to_number(), limit.to_number(), step.to_number())
                        {
                            let index = index + step;
                            registers.stack_frame[base.0 as usize] = Value::Number(index);

                            let past_end = if step < 0.0 {
                                !(index >= limit)
                            } else {
                                !(index <= limit)
                            };
                            if !past_end {
                                *registers.pc = add_offset(*registers.pc, jump);
                                registers.stack_frame[base.0 as usize + 3] = Value::Number(index);
                            }
                        } else {
                            return Err(VMError::BadForLoop(
                                index.type_name(),
                                limit.type_name(),
                                step.type_name(),
                            ));
                        }
                    }
                }
            }

            Operation::GenericForCall { base, var_count } => {
                lua_frame.call_function_keep(ctx, base, 2, VarCount::constant(var_count))?;
                break;
            }

            Operation::GenericForLoop { base, jump } => {
                if !registers.stack_frame[base.0 as usize + 1].is_nil() {
                    registers.stack_frame[base.0 as usize] =
                        registers.stack_frame[base.0 as usize + 1];
                    *registers.pc = add_offset(*registers.pc, jump);
                }
            }

            Operation::Method { base, table, key } => {
                let table = registers.stack_frame[table.0 as usize];
                let key = get_rc(&registers.stack_frame, &current_prototype.constants, key);
                registers.stack_frame[base.0 as usize + 1] = table;
                match meta_ops::index(ctx, table, key)? {
                    MetaResult::Value(v) => {
                        registers.stack_frame[base.0 as usize] = v;
                    }
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(base),
                        )?;
                        break;
                    }
                }
            }

            Operation::Concat {
                dest,
                source,
                count,
            } => {
                let base = source.0 as usize;
                let values = &registers.stack_frame[base..base + count as usize];
                match meta_ops::concat_many(ctx, values)? {
                    ConcatMetaResult::Value(v) => registers.stack_frame[dest.0 as usize] = v,
                    ConcatMetaResult::Call(func) => {
                        // This would be nice to do in-place (without copying params)
                        // but that turns out to be difficult to do without corrupting
                        // the stack, and likely isn't worth the complexity for the
                        // fallback case.
                        let args = values.to_owned();
                        lua_frame.call_meta_function(
                            ctx,
                            func,
                            &args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::GetUpValue { source, dest } => {
                registers.stack_frame[dest.0 as usize] =
                    registers.get_upvalue(&ctx, current_upvalues[source.0 as usize].get());
            }

            Operation::SetUpValue { source, dest } => {
                registers.set_upvalue(
                    &ctx,
                    current_upvalues[dest.0 as usize].get(),
                    registers.stack_frame[source.0 as usize],
                );
            }

            Operation::Length { dest, source } => {
                match meta_ops::len(ctx, registers.stack_frame[source.0 as usize])? {
                    MetaResult::Value(v) => {
                        registers.stack_frame[dest.0 as usize] = v;
                    }
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::Eq {
                skip_if,
                left,
                right,
            } => {
                let left = get_rc(&registers.stack_frame, &current_prototype.constants, left);
                let right = get_rc(&registers.stack_frame, &current_prototype.constants, right);
                match meta_ops::equal(ctx, left, right)? {
                    MetaResult::Value(v) => {
                        if v.to_bool() == skip_if {
                            *registers.pc += 1;
                        }
                    }
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::SkipIf(skip_if),
                        )?;
                        break;
                    }
                }
            }

            Operation::Less {
                skip_if,
                left,
                right,
            } => {
                let left = get_rc(&registers.stack_frame, &current_prototype.constants, left);
                let right = get_rc(&registers.stack_frame, &current_prototype.constants, right);
                match meta_ops::less_than(ctx, left, right)? {
                    MetaResult::Value(v) => {
                        if v.to_bool() == skip_if {
                            *registers.pc += 1;
                        }
                    }
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::SkipIf(skip_if),
                        )?;
                        break;
                    }
                }
            }

            Operation::LessEq {
                skip_if,
                left,
                right,
            } => {
                let left = get_rc(&registers.stack_frame, &current_prototype.constants, left);
                let right = get_rc(&registers.stack_frame, &current_prototype.constants, right);
                match meta_ops::less_equal(ctx, left, right)? {
                    MetaResult::Value(v) => {
                        if v.to_bool() == skip_if {
                            *registers.pc += 1;
                        }
                    }
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::SkipIf(skip_if),
                        )?;
                        break;
                    }
                }
            }

            Operation::Not { dest, source } => {
                let source = registers.stack_frame[source.0 as usize];
                registers.stack_frame[dest.0 as usize] = (!source.to_bool()).into();
            }

            Operation::Minus { dest, source } => {
                let value = registers.stack_frame[source.0 as usize];
                match meta_ops::negate(ctx, value)? {
                    MetaResult::Value(v) => registers.stack_frame[dest.0 as usize] = v,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::BitNot { dest, source } => {
                let value = registers.stack_frame[source.0 as usize];
                match meta_ops::bitwise_not(ctx, value)? {
                    MetaResult::Value(v) => registers.stack_frame[dest.0 as usize] = v,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::Add { dest, left, right } => {
                let left = get_rc(&registers.stack_frame, &current_prototype.constants, left);
                let right = get_rc(&registers.stack_frame, &current_prototype.constants, right);
                match meta_ops::add(ctx, left, right)? {
                    MetaResult::Value(v) => registers.stack_frame[dest.0 as usize] = v,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::Sub { dest, left, right } => {
                let left = get_rc(&registers.stack_frame, &current_prototype.constants, left);
                let right = get_rc(&registers.stack_frame, &current_prototype.constants, right);
                match meta_ops::subtract(ctx, left, right)? {
                    MetaResult::Value(v) => registers.stack_frame[dest.0 as usize] = v,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::Mul { dest, left, right } => {
                let left = get_rc(&registers.stack_frame, &current_prototype.constants, left);
                let right = get_rc(&registers.stack_frame, &current_prototype.constants, right);
                match meta_ops::multiply(ctx, left, right)? {
                    MetaResult::Value(v) => registers.stack_frame[dest.0 as usize] = v,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::Div { dest, left, right } => {
                let left = get_rc(&registers.stack_frame, &current_prototype.constants, left);
                let right = get_rc(&registers.stack_frame, &current_prototype.constants, right);
                match meta_ops::float_divide(ctx, left, right)? {
                    MetaResult::Value(v) => registers.stack_frame[dest.0 as usize] = v,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::IDiv { dest, left, right } => {
                let left = get_rc(&registers.stack_frame, &current_prototype.constants, left);
                let right = get_rc(&registers.stack_frame, &current_prototype.constants, right);
                match meta_ops::floor_divide(ctx, left, right)? {
                    MetaResult::Value(v) => registers.stack_frame[dest.0 as usize] = v,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::Mod { dest, left, right } => {
                let left = get_rc(&registers.stack_frame, &current_prototype.constants, left);
                let right = get_rc(&registers.stack_frame, &current_prototype.constants, right);
                match meta_ops::modulo(ctx, left, right)? {
                    MetaResult::Value(v) => registers.stack_frame[dest.0 as usize] = v,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::Pow { dest, left, right } => {
                let left = get_rc(&registers.stack_frame, &current_prototype.constants, left);
                let right = get_rc(&registers.stack_frame, &current_prototype.constants, right);
                match meta_ops::exponentiate(ctx, left, right)? {
                    MetaResult::Value(v) => registers.stack_frame[dest.0 as usize] = v,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::BitAnd { dest, left, right } => {
                let left = get_rc(&registers.stack_frame, &current_prototype.constants, left);
                let right = get_rc(&registers.stack_frame, &current_prototype.constants, right);
                match meta_ops::bitwise_and(ctx, left, right)? {
                    MetaResult::Value(v) => registers.stack_frame[dest.0 as usize] = v,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::BitOr { dest, left, right } => {
                let left = get_rc(&registers.stack_frame, &current_prototype.constants, left);
                let right = get_rc(&registers.stack_frame, &current_prototype.constants, right);
                match meta_ops::bitwise_or(ctx, left, right)? {
                    MetaResult::Value(v) => registers.stack_frame[dest.0 as usize] = v,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::BitXor { dest, left, right } => {
                let left = get_rc(&registers.stack_frame, &current_prototype.constants, left);
                let right = get_rc(&registers.stack_frame, &current_prototype.constants, right);
                match meta_ops::bitwise_xor(ctx, left, right)? {
                    MetaResult::Value(v) => registers.stack_frame[dest.0 as usize] = v,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::ShiftLeft { dest, left, right } => {
                let left = get_rc(&registers.stack_frame, &current_prototype.constants, left);
                let right = get_rc(&registers.stack_frame, &current_prototype.constants, right);
                match meta_ops::shift_left(ctx, left, right)? {
                    MetaResult::Value(v) => registers.stack_frame[dest.0 as usize] = v,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::ShiftRight { dest, left, right } => {
                let left = get_rc(&registers.stack_frame, &current_prototype.constants, left);
                let right = get_rc(&registers.stack_frame, &current_prototype.constants, right);
                match meta_ops::shift_right(ctx, left, right)? {
                    MetaResult::Value(v) => registers.stack_frame[dest.0 as usize] = v,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }
        }

        instructions_run += 1;
        if instructions_run >= max_instructions {
            break;
        }
    }
    #[cfg(feature = "jit")]
    {
        interpreter_stats.reported_instructions = Some(instructions_run - native_instructions);
    }
    Ok(instructions_run)
}

fn add_offset(pc: usize, offset: i16) -> usize {
    pc.checked_add_signed(isize::from(offset)).unwrap()
}

#[cfg(test)]
mod offset_tests {
    use super::add_offset;

    #[test]
    fn signed_offsets_match_wide_arithmetic() {
        for pc in [0, 1, 32767, 32768, 65535, usize::MAX - 32767, usize::MAX] {
            for offset in i16::MIN..=i16::MAX {
                let expected = pc as i128 + i128::from(offset);
                if let Ok(expected) = usize::try_from(expected) {
                    assert_eq!(add_offset(pc, offset), expected, "pc={pc}, offset={offset}");
                }
            }
        }
    }

    #[test]
    fn minimum_signed_offset_reaches_zero() {
        assert_eq!(add_offset(32768, i16::MIN), 0);
    }

    #[test]
    fn maximum_backward_loop_executes_in_the_interpreter() {
        let source = format!(
            "local x=0 for i=1,1 do {} end return x",
            "x=x+1 ".repeat(32767)
        );
        let mut lua = crate::Lua::empty();
        #[cfg(feature = "jit")]
        lua.set_jit_config(crate::JitConfig {
            mode: crate::JitMode::Off,
            ..crate::JitConfig::default()
        })
        .unwrap();
        let executor = lua.enter(|ctx| {
            let closure = crate::Closure::load(ctx, None, source.as_bytes()).unwrap();
            assert!(closure.prototype().opcodes.iter().any(|op| matches!(
                op.decode(),
                crate::opcode::Operation::NumericForLoop { jump: i16::MIN, .. }
            )));
            ctx.stash(crate::Executor::start(ctx, closure.into(), ()))
        });
        assert_eq!(lua.execute::<i64>(&executor).unwrap(), 32767);
    }

    #[test]
    #[should_panic]
    fn signed_offset_underflow_is_rejected() {
        add_offset(32767, i16::MIN);
    }

    #[test]
    #[should_panic]
    fn signed_offset_overflow_is_rejected() {
        add_offset(usize::MAX, 1);
    }
}

fn raw_subtract<'gc>(lhs: Value<'gc>, rhs: Value<'gc>) -> Option<Value<'gc>> {
    Some(lhs.to_constant()?.subtract(&rhs.to_constant()?)?.into())
}
