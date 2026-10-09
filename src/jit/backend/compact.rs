use super::*;
use crate::jit::leaf::{Arithmetic, Operand, Pattern};
use cranelift_codegen::ir::{Function, Signature, UserFuncName};

type CompactEntry = unsafe extern "C" fn(i64, i64) -> i64;

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
    capture: i64,
    right: i64,
    read_alias: bool,
    right_alias: bool,
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
    upper_aliases: (bool, bool),
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
        signature.params.extend([AbiParam::new(types::I64); 2]);
        signature.returns.push(AbiParam::new(types::I64));
        let mut function = Function::with_name_signature(UserFuncName::user(0, 0), signature);
        let mut context = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut function, &mut context);
        let block = b.create_block();
        b.append_block_params_for_function_params(block);
        b.switch_to_block(block);
        let [left, input_right]: [_; 2] = b.block_params(block).try_into().unwrap();
        let right = match self.constant {
            Some(value) => b.ins().iconst(types::I64, value),
            None if self.pattern.right == Operand::Register(self.pattern.read) => left,
            None => input_right,
        };
        let value = match self.pattern.arithmetic {
            Arithmetic::Add => b.ins().iadd(left, right),
            Arithmetic::Sub => b.ins().isub(left, right),
            Arithmetic::Mul => b.ins().imul(left, right),
        };
        b.ins().return_(&[value]);
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

    fn aliases(&self, capture: Option<usize>) -> (bool, bool) {
        let read = usize::from(self.pattern.read.0);
        let result = usize::from(self.pattern.result.0);
        let right = match self.pattern.right {
            Operand::Register(index) => Some(usize::from(index.0)),
            Operand::Constant(_) => None,
        };
        (
            read == result || capture == Some(read),
            right.is_some() && (right == Some(result) || right == capture),
        )
    }

    #[cfg(test)]
    fn prepare(
        &self,
        registers: &[crate::Value<'_>],
        capture: Option<usize>,
        upper: i64,
    ) -> Option<Frame<'_>> {
        self.prepare_aliases(registers, capture, upper, self.aliases(capture))
    }

    fn prepare_aliases(
        &self,
        registers: &[crate::Value<'_>],
        capture: Option<usize>,
        upper: i64,
        aliases: (bool, bool),
    ) -> Option<Frame<'_>> {
        if registers.len() < self.registers || capture.is_some_and(|index| index >= self.registers)
        {
            return None;
        }
        let p = self.pattern;
        let mut right_value = 0;
        if let Operand::Register(right) = p.right {
            if right != p.read {
                let crate::Value::Integer(value) = registers[usize::from(right.0)] else {
                    return None;
                };
                right_value = value;
            }
        }
        let capture_value = match capture {
            Some(index) => match registers[index] {
                crate::Value::Integer(value) => value,
                _ => return None,
            },
            None => upper,
        };
        Some(Frame {
            binding: self,
            capture: capture_value,
            right: right_value,
            read_alias: aliases.0,
            right_alias: aliases.1,
        })
    }
}

impl Entry {
    pub(in crate::jit) fn prepare_arguments(
        &self,
        arguments: &[crate::Value<'_>],
        capture: i64,
    ) -> Option<Frame<'_>> {
        let right = match self.binding.pattern.right {
            Operand::Register(index) if index != self.binding.pattern.read => {
                let crate::Value::Integer(value) = arguments.get(usize::from(index.0))? else {
                    return None;
                };
                *value
            }
            _ => 0,
        };
        Some(Frame {
            binding: &self.binding,
            capture,
            right,
            read_alias: self.upper_aliases.0,
            right_alias: self.upper_aliases.1,
        })
    }

    /// Binds a verified compact function to its live executable image.
    ///
    /// # Safety
    /// The pointer must implement the binding and remain executable while used.
    pub(super) unsafe fn new(binding: Binding, pointer: *const u8) -> Self {
        Self {
            binding,
            upper_aliases: binding.aliases(None),
            entry: unsafe { std::mem::transmute::<*const u8, CompactEntry>(pointer) },
        }
    }

    pub(in crate::jit) fn prepare(
        &self,
        registers: &[crate::Value<'_>],
        capture: Option<usize>,
        upper: i64,
    ) -> Option<Frame<'_>> {
        let aliases = if capture.is_none() {
            self.upper_aliases
        } else {
            self.binding.aliases(capture)
        };
        self.binding
            .prepare_aliases(registers, capture, upper, aliases)
    }

    pub(in crate::jit) fn invoke(&self, frame: &mut Frame<'_>) -> Result<Outputs, JitError> {
        invoke(self.entry, &self.binding, frame)
    }
}

fn invoke(
    entry: CompactEntry,
    binding: &Binding,
    frame: &mut Frame<'_>,
) -> Result<Outputs, JitError> {
    if !std::ptr::eq(frame.binding, binding) {
        return Err(JitError::Compilation("compact invocation binding".into()));
    }
    let capture = frame.capture;
    let result = unsafe { (entry)(capture, frame.right) };
    frame.capture = result;
    if frame.right_alias {
        frame.right = result;
    }
    Ok(Outputs {
        capture: result,
        read: if frame.read_alias { result } else { capture },
        result,
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
