use super::*;
use crate::jit::leaf::{Arithmetic, Operand, Pattern};
use cranelift_codegen::ir::{Function, Signature, UserFuncName};

type CompactEntry = unsafe extern "C" fn(*mut i64, *mut i64, *mut i64, *mut i64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Binding {
    pattern: Pattern,
    constant: Option<i64>,
    registers: usize,
    returns: (u8, u8),
}

pub(super) struct Program {
    binding: Binding,
    pub function: Function,
}

#[cfg(test)]
pub(super) struct Code {
    _memory: Memory,
    entry: CompactEntry,
    binding: Binding,
}

pub(in crate::jit) struct Frame<'code> {
    binding: &'code Binding,
    cells: [i64; 4],
    indices: [u8; 4],
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::jit) struct Outputs {
    pub capture: i64,
    pub read: i64,
    pub result: i64,
}

pub(in crate::jit) struct Entry {
    entry: CompactEntry,
    binding: Binding,
    upper_indices: [u8; 4],
}

impl Binding {
    pub(super) fn new(source: &Snapshot) -> Result<Self, JitError> {
        let pattern = Pattern::recognize(source)
            .ok_or_else(|| JitError::Compilation("compact callee source".into()))?;
        let Operation::Return { start, count } = source.operations[3] else {
            unreachable!()
        };
        let count = count
            .to_constant()
            .ok_or_else(|| JitError::Compilation("compact variable return".into()))?;
        let constant = match pattern.right {
            Operand::Register(_) => None,
            Operand::Constant(index) => {
                let slot = source.constants[usize::from(index)];
                if slot.tag != abi::INTEGER {
                    return Err(JitError::Compilation("compact constant type".into()));
                }
                Some(slot.bits as i64)
            }
        };
        Ok(Self {
            pattern,
            constant,
            registers: source.registers,
            returns: (start.0, count),
        })
    }

    pub(super) fn program(self, isa: &dyn cranelift_codegen::isa::TargetIsa) -> Program {
        let mut signature = Signature::new(isa.default_call_conv());
        signature
            .params
            .extend([AbiParam::new(isa.frontend_config().pointer_type()); 4]);
        let mut function = Function::with_name_signature(UserFuncName::user(0, 0), signature);
        let mut context = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut function, &mut context);
        let block = b.create_block();
        b.append_block_params_for_function_params(block);
        b.switch_to_block(block);
        let [capture, read, right, dest]: [_; 4] = b.block_params(block).try_into().unwrap();
        let value = b.ins().load(types::I64, MemFlagsData::new(), capture, 0);
        b.ins().store(MemFlagsData::new(), value, read, 0);
        let left = b.ins().load(types::I64, MemFlagsData::new(), read, 0);
        let right = match self.constant {
            Some(value) => b.ins().iconst(types::I64, value),
            None => b.ins().load(types::I64, MemFlagsData::new(), right, 0),
        };
        let value = match self.pattern.arithmetic {
            Arithmetic::Add => b.ins().iadd(left, right),
            Arithmetic::Sub => b.ins().isub(left, right),
            Arithmetic::Mul => b.ins().imul(left, right),
        };
        b.ins().store(MemFlagsData::new(), value, dest, 0);
        let value = b.ins().load(types::I64, MemFlagsData::new(), dest, 0);
        b.ins().store(MemFlagsData::new(), value, capture, 0);
        b.ins().return_(&[]);
        b.seal_all_blocks();
        b.finalize(isa.frontend_config());
        Program {
            binding: self,
            function,
        }
    }

    pub(super) fn verify(
        self,
        program: &Program,
        isa: &dyn cranelift_codegen::isa::TargetIsa,
    ) -> Result<(), JitError> {
        if program.binding != self || program.function != self.program(isa).function {
            return Err(JitError::Compilation("compact source binding".into()));
        }
        cranelift_codegen::verify_function(&program.function, isa)
            .map_err(|error| JitError::Compilation(error.to_string()))
    }

    fn indices(&self, capture: Option<usize>) -> [u8; 4] {
        let read = usize::from(self.pattern.read.0);
        let result = usize::from(self.pattern.result.0);
        let right = match self.pattern.right {
            Operand::Register(index) => Some(usize::from(index.0)),
            Operand::Constant(_) => None,
        };
        let read_cell = if capture == Some(read) { 0 } else { 1 };
        let right_cell = if right.is_some() && right == capture {
            0
        } else if right == Some(read) {
            read_cell
        } else {
            2
        };
        let result_cell = if capture == Some(result) {
            0
        } else if result == read {
            read_cell
        } else if right == Some(result) {
            right_cell
        } else {
            3
        };
        [0, read_cell, right_cell, result_cell]
    }

    #[cfg(test)]
    fn prepare(
        &self,
        registers: &[crate::Value<'_>],
        capture: Option<usize>,
        upper: i64,
    ) -> Option<Frame<'_>> {
        self.prepare_indices(registers, capture, upper, self.indices(capture))
    }

    fn prepare_indices(
        &self,
        registers: &[crate::Value<'_>],
        capture: Option<usize>,
        upper: i64,
        indices: [u8; 4],
    ) -> Option<Frame<'_>> {
        if registers.len() < self.registers || capture.is_some_and(|index| index >= self.registers)
        {
            return None;
        }
        let p = self.pattern;
        let mut cells = [0; 4];
        if let Operand::Register(right) = p.right {
            if right != p.read {
                let crate::Value::Integer(value) = registers[usize::from(right.0)] else {
                    return None;
                };
                cells[usize::from(indices[2])] = value;
            }
        }
        cells[usize::from(indices[0])] = match capture {
            Some(index) => match registers[index] {
                crate::Value::Integer(value) => value,
                _ => return None,
            },
            None => upper,
        };
        Some(Frame {
            binding: self,
            cells,
            indices,
        })
    }
}

impl Entry {
    /// Binds a verified compact function to its live executable image.
    ///
    /// # Safety
    /// The pointer must implement the binding and remain executable while used.
    pub(super) unsafe fn new(binding: Binding, pointer: *const u8) -> Self {
        Self {
            binding,
            upper_indices: binding.indices(None),
            entry: unsafe { std::mem::transmute::<*const u8, CompactEntry>(pointer) },
        }
    }

    pub(in crate::jit) fn prepare(
        &self,
        registers: &[crate::Value<'_>],
        capture: Option<usize>,
        upper: i64,
    ) -> Option<Frame<'_>> {
        let indices = if capture.is_none() {
            self.upper_indices
        } else {
            self.binding.indices(capture)
        };
        self.binding
            .prepare_indices(registers, capture, upper, indices)
    }

    #[inline(always)]
    pub(in crate::jit) fn invoke(&self, frame: &mut Frame<'_>) -> Result<Outputs, JitError> {
        invoke(self.entry, &self.binding, frame)
    }
}

#[inline(always)]
fn invoke(
    entry: CompactEntry,
    binding: &Binding,
    frame: &mut Frame<'_>,
) -> Result<Outputs, JitError> {
    if !std::ptr::eq(frame.binding, binding) || frame.indices.iter().any(|&index| index >= 4) {
        return Err(JitError::Compilation("compact invocation binding".into()));
    }
    let base = frame.cells.as_mut_ptr();
    let [capture, read, right, dest] = frame.indices.map(usize::from);
    unsafe {
        (entry)(
            base.add(capture),
            base.add(read),
            base.add(right),
            base.add(dest),
        )
    };
    Ok(Outputs {
        capture: frame.cells[capture],
        read: frame.cells[read],
        result: frame.cells[dest],
    })
}

#[cfg(test)]
impl Code {
    fn invoke(&self, frame: &mut Frame<'_>) -> Result<Outputs, JitError> {
        invoke(self.entry, &self.binding, frame)
    }
}

#[cfg(test)]
fn compile(
    source: &Snapshot,
    total: MappingCounter,
    limit: usize,
    metadata: BudgetAllocator,
    limits: crate::jit::work::Limits,
    failure: Failure,
) -> Result<Code, JitError> {
    let expansion = crate::jit::work::Expansion::admit(source, limits)?;
    let binding = Binding::new(source)?;
    let workspace = source.operations.allocator().clone();
    let _templates = Reservation::new(workspace.0.clone(), 32 * 1024)
        .map_err(|_| JitError::ResourceLimit("compact template workspace"))?;
    let _signatures = Reservation::new(
        workspace.0.clone(),
        13 * 3 * Layout::new::<AbiParam>().size(),
    )
    .map_err(|_| JitError::ResourceLimit("compact signatures"))?;
    let status = MemoryStatus::try_new(metadata.clone())?;
    let fail = |error: cranelift_module::ModuleError| {
        status
            .error()
            .unwrap_or_else(|| JitError::Compilation(error.to_string()))
    };
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return Err(JitError::Unavailable(
            "cannot determine native memory page size",
        ));
    }
    let memory = Handoff::try_new(
        Memory {
            allocations: BudgetVec::new_in(metadata.clone()),
            total,
            status: status.clone(),
            failure,
            limit,
            page: page as usize,
        },
        metadata.clone(),
    )
    .map_err(|_| JitError::ResourceLimit("JIT metadata"))?;
    let (provider, _provider_charge) = global_box::try_new(Provider(memory.clone()), metadata)
        .map_err(|_| JitError::ResourceLimit("JIT metadata"))?;
    let mut jit = native_builder(cranelift_native::builder())?;
    jit.memory_provider(provider);
    let mut module = JITModule::new(jit);
    let program = binding.program(module.isa());
    binding.verify(&program, module.isa())?;
    expansion.verify_actual(
        program
            .function
            .layout
            .blocks()
            .map(|b| program.function.layout.block_insts(b).count())
            .sum(),
        program.function.layout.blocks().count(),
    )?;
    let id = module
        .declare_anonymous_function(&program.function.signature)
        .map_err(fail)?;
    if id.as_u32() != 0 {
        return Err(JitError::Compilation("compact function identity".into()));
    }
    let mut context = module.make_context();
    context.func = program.function;
    context
        .compile(module.isa(), &mut Default::default())
        .map_err(|error| fail(error.into()))?;
    let compiled = context.compiled_code().unwrap();
    if !compiled.buffer.relocs().is_empty() {
        return Err(JitError::Compilation(
            "compact unexpected relocation".into(),
        ));
    }
    module
        .define_function_bytes(
            id,
            u64::from(compiled.buffer.alignment),
            compiled.code_buffer(),
            &[],
        )
        .map_err(fail)?;
    module.finalize_definitions().map_err(fail)?;
    let pointer = module.get_finalized_function(id);
    let image = memory
        .take()
        .ok_or_else(|| JitError::Compilation("missing compact mappings".into()))?;
    drop(module);
    Ok(Code {
        _memory: image,
        entry: unsafe { std::mem::transmute::<*const u8, CompactEntry>(pointer) },
        binding,
    })
}

#[cfg(test)]
mod tests;
