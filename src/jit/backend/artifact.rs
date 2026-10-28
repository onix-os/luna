use std::cell::{Cell, RefCell};

use cranelift_codegen::binemit::Reloc;
use cranelift_codegen::ir::Signature;
use cranelift_module::{FuncId, ModuleRelocTarget};

use super::*;

#[cfg(not(miri))]
mod assemble;
#[cfg(not(miri))]
pub(in crate::jit) mod emission;
mod install;
mod link;
mod target;
#[cfg(not(miri))]
pub(in crate::jit) use assemble::{assemble, Assembly, Route};
#[cfg(not(miri))]
pub(in crate::jit) mod runtime;
#[cfg(all(test, not(miri)))]
mod tests;
mod wire;
pub(in crate::jit) use install::{install, InstallError, Installed};
pub(super) use target::apply_target;
pub(in crate::jit) use target::{target_features, with_target};
#[cfg(not(miri))]
pub(in crate::jit) use tests::{exercise, exercise_prepared};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit) enum Error {
    Allocation,
    Header,
    Limit,
    Identity,
    Declaration,
    Relocation,
    Module,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit) enum Kind {
    Ordinary = 1,
    Readonly = 2,
    Arrays = 3,
    Resume = 4,
}

impl Kind {
    fn from_word(value: u32) -> Result<Self, Error> {
        match value {
            1 => Ok(Self::Ordinary),
            2 => Ok(Self::Readonly),
            3 => Ok(Self::Arrays),
            4 => Ok(Self::Resume),
            _ => Err(Error::Module),
        }
    }
    fn signature(self) -> &'static [u8] {
        match self {
            Self::Ordinary | Self::Arrays => &[2, 2, 1, 2, 2],
            Self::Readonly => &[2, 2, 1, 2, 2, 2],
            Self::Resume => &[2, 1, 2],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Declaration([u8; 16]);

impl Declaration {
    fn capture(
        name: Option<&str>,
        signature: &Signature,
        module: &JITModule,
    ) -> Result<Self, Error> {
        if signature.params.len() > 8
            || signature.returns.len() > 1
            || signature.call_conv != module.isa().default_call_conv()
        {
            return Err(Error::Declaration);
        }
        let mut record = [0; 16];
        record[0] = match name {
            None => 0,
            Some("luna_get_up_table_c3") => 11,
            Some("luna_get_up_table_h1") => 12,
            Some(name) => helpers::SYMBOLS
                .iter()
                .position(|(_, symbol, _)| *symbol == name)
                .map(|index| index as u8 + 1)
                .ok_or(Error::Declaration)?,
        };
        record[1] = signature.params.len() as u8;
        record[2] = signature
            .returns
            .first()
            .map_or(Ok(0), |param| field(*param))?;
        for (target, &parameter) in record[4..12].iter_mut().zip(&signature.params) {
            *target = field(parameter)?;
        }
        let declaration = Self(record);
        declaration.verify()?;
        Ok(declaration)
    }
    fn verify(self) -> Result<(), Error> {
        let bytes = self.0;
        if bytes[0] > 12
            || bytes[1] > 8
            || bytes[2] > 2
            || bytes[3] != 0
            || bytes[12..] != [0; 4]
            || bytes[4..4 + usize::from(bytes[1])]
                .iter()
                .any(|&ty| ty != 1 && ty != 2)
            || bytes[4 + usize::from(bytes[1])..12]
                .iter()
                .any(|&ty| ty != 0)
        {
            return Err(Error::Declaration);
        }
        if bytes[0] != 0 {
            let expected = if bytes[0] == 11 {
                &[2, 2, 2][..]
            } else {
                &[2, 2, 1, 1, 1, 1][..]
            };
            if self.params() != expected || bytes[2] != 1 {
                return Err(Error::Declaration);
            }
        }
        Ok(())
    }
    fn params(&self) -> &[u8] {
        &self.0[4..4 + usize::from(self.0[1])]
    }
}

fn field(parameter: AbiParam) -> Result<u8, Error> {
    if parameter != AbiParam::new(parameter.value_type) {
        return Err(Error::Declaration);
    }
    match parameter.value_type {
        types::I32 => Ok(1),
        types::I64 => Ok(2),
        _ => Err(Error::Declaration),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Relocation {
    offset: u32,
    kind: u8,
    target_kind: u8,
    index: u32,
    target_offset: u32,
    addend: i64,
}

impl Relocation {
    fn capture(relocation: &ModuleReloc) -> Result<Self, Error> {
        let kind = match relocation.kind {
            Reloc::Abs8 => 1,
            Reloc::X86CallPCRel4 => 2,
            Reloc::X86CallPLTRel4 => 3,
            Reloc::X86GOTPCRel4 => 4,
            Reloc::Arm64Call => 5,
            Reloc::X86PCRel4 => 6,
            _ => return Err(Error::Relocation),
        };
        let (target_kind, index, target_offset) = match relocation.name {
            ModuleRelocTarget::User {
                namespace: 0,
                index,
            } => (0, index, 0),
            ModuleRelocTarget::FunctionOffset(id, offset) => (1, id.as_u32(), offset),
            _ => return Err(Error::Relocation),
        };
        Ok(Self {
            offset: relocation.offset,
            kind,
            target_kind,
            index,
            target_offset,
            addend: relocation.addend,
        })
    }
    fn width(self) -> Result<usize, Error> {
        match self.kind {
            1 => Ok(8),
            2..=4 | 6 if cfg!(target_arch = "x86_64") => Ok(4),
            5 if cfg!(target_arch = "aarch64") => Ok(4),
            _ => Err(Error::Relocation),
        }
    }
}

struct Function {
    id: u32,
    alignment: u32,
    bytes: BudgetVec<u8, BudgetAllocator>,
    relocations: BudgetVec<Relocation, BudgetAllocator>,
}

struct ModuleImage {
    kind: Kind,
    declarations: BudgetVec<Declaration, BudgetAllocator>,
    functions: BudgetVec<Function, BudgetAllocator>,
}

pub(in crate::jit) struct Bundle {
    modules: BudgetVec<ModuleImage, BudgetAllocator>,
    environment: BudgetVec<u8, BudgetAllocator>,
}

fn copy<T: Copy>(
    values: &[T],
    allocator: BudgetAllocator,
) -> Result<BudgetVec<T, BudgetAllocator>, Error> {
    let mut output = BudgetVec::new_in(allocator);
    output
        .try_reserve_exact(values.len())
        .map_err(|_| Error::Allocation)?;
    output.extend_from_slice(values);
    Ok(output)
}

pub(in crate::jit) fn environment() -> Result<BudgetVec<u8, BudgetAllocator>, Error> {
    let module =
        JITModule::new(native_builder(cranelift_native::builder()).map_err(|_| Error::Identity)?);
    environment_of(
        &module,
        BudgetAllocator(super::super::resources::Ledger::new(32768)),
    )
}

fn environment_of(
    module: &JITModule,
    allocator: BudgetAllocator,
) -> Result<BudgetVec<u8, BudgetAllocator>, Error> {
    let identity = format!(
        "cranelift=0.136.1;artifact=1;slot=16;exit=16;{};{};{};{}",
        module.isa().triple(),
        module.isa().default_call_conv(),
        module.isa().flags(),
        module
            .isa()
            .isa_flags()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(";")
    );
    if identity.len() > 32768 {
        return Err(Error::Limit);
    }
    copy(identity.as_bytes(), allocator)
}

struct Captured {
    image: ModuleImage,
    committed: bool,
}

struct State {
    id: u64,
    allocator: BudgetAllocator,
    modules: BudgetVec<Captured, BudgetAllocator>,
    environment: BudgetVec<u8, BudgetAllocator>,
    error: Option<Error>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static NEXT: Cell<u64> = const { Cell::new(0) };
}

pub(super) struct Capture {
    scope: u64,
    index: Option<usize>,
}

pub(super) fn begin(kind: Kind) -> Capture {
    STATE.with_borrow_mut(|state| {
        let Some(state) = state else {
            return Capture {
                scope: 0,
                index: None,
            };
        };
        let mut token = Capture {
            scope: state.id,
            index: None,
        };
        if state.error.is_some() {
            return token;
        }
        if state.modules.len() >= 16 {
            state.error = Some(Error::Limit);
            return token;
        }
        if state.modules.try_reserve(1).is_err() {
            state.error = Some(Error::Allocation);
            return token;
        }
        token.index = Some(state.modules.len());
        state.modules.push(Captured {
            image: ModuleImage {
                kind,
                declarations: BudgetVec::new_in(state.allocator.clone()),
                functions: BudgetVec::new_in(state.allocator.clone()),
            },
            committed: false,
        });
        token
    })
}

impl Capture {
    pub(super) fn record(
        &self,
        id: FuncId,
        bytes: &[u8],
        alignment: u64,
        relocations: &[ModuleReloc],
        module: &JITModule,
    ) {
        STATE.with_borrow_mut(|state| {
            let Some(state) = state else { return };
            if state.id != self.scope || state.error.is_some() {
                return;
            }
            let Some(index) = self.index else { return };
            let result = (|| {
                let identity = environment_of(module, state.allocator.clone())?;
                if state.environment.is_empty() {
                    state.environment = identity;
                } else if *state.environment != *identity {
                    return Err(Error::Identity);
                }
                if module.declarations().get_data_objects().next().is_some() {
                    return Err(Error::Declaration);
                }
                let image = &mut state.modules[index].image;
                let mut declarations = BudgetVec::new_in(state.allocator.clone());
                for (id, declaration) in module.declarations().get_functions() {
                    if declarations.len() >= 64 || id.as_u32() as usize != declarations.len() {
                        return Err(Error::Limit);
                    }
                    let local = declaration.name.is_none();
                    if declaration.linkage
                        != if local {
                            Linkage::Local
                        } else {
                            Linkage::Import
                        }
                    {
                        return Err(Error::Declaration);
                    }
                    declarations.try_reserve(1).map_err(|_| Error::Allocation)?;
                    declarations.push(Declaration::capture(
                        declaration.name.as_deref(),
                        &declaration.signature,
                        module,
                    )?);
                }
                image.declarations = declarations;
                let mut copied = BudgetVec::new_in(state.allocator.clone());
                copied
                    .try_reserve_exact(relocations.len())
                    .map_err(|_| Error::Allocation)?;
                for relocation in relocations {
                    copied.push(Relocation::capture(relocation)?);
                }
                image
                    .functions
                    .try_reserve(1)
                    .map_err(|_| Error::Allocation)?;
                image.functions.push(Function {
                    id: id.as_u32(),
                    alignment: alignment.try_into().map_err(|_| Error::Limit)?,
                    bytes: copy(bytes, state.allocator.clone())?,
                    relocations: copied,
                });
                Ok(())
            })();
            if let Err(error) = result {
                state.error = Some(error);
            }
        });
    }
    pub(super) fn commit(self) {
        STATE.with_borrow_mut(|state| {
            if let Some(state) = state {
                if state.id == self.scope {
                    if let Some(index) = self.index {
                        state.modules[index].committed = true;
                    }
                }
            }
        });
    }
}

pub(in crate::jit) fn capture<T>(
    allocator: BudgetAllocator,
    run: impl FnOnce() -> T,
) -> Result<(T, Bundle), Error> {
    struct Restore(Option<State>);
    impl Drop for Restore {
        fn drop(&mut self) {
            STATE.replace(self.0.take());
        }
    }
    let id = NEXT.get().checked_add(1).ok_or(Error::Limit)?;
    NEXT.set(id);
    let restore = Restore(STATE.replace(Some(State {
        id,
        allocator: allocator.clone(),
        modules: BudgetVec::new_in(allocator.clone()),
        environment: BudgetVec::new_in(allocator.clone()),
        error: None,
    })));
    let result = run();
    let state = STATE.replace(None).unwrap();
    drop(restore);
    if let Some(error) = state.error {
        return Err(error);
    }
    let mut modules = BudgetVec::new_in(allocator);
    modules
        .try_reserve_exact(state.modules.len())
        .map_err(|_| Error::Allocation)?;
    modules.extend(
        state
            .modules
            .into_iter()
            .filter(|module| module.committed)
            .map(|module| module.image),
    );
    Ok((
        result,
        Bundle {
            modules,
            environment: state.environment,
        },
    ))
}

impl Bundle {
    pub(in crate::jit) fn routes(&self) -> u64 {
        self.modules.iter().fold(0, |routes, module| {
            routes
                | match module.kind {
                    Kind::Ordinary => 1,
                    Kind::Arrays => 2,
                    Kind::Readonly => 4,
                    Kind::Resume => 8,
                }
        })
    }
    pub(in crate::jit) fn target_environment(&self) -> &[u8] {
        &self.environment
    }
    pub(in crate::jit) fn retain_routes(&mut self, routes: u64) {
        self.modules.retain(|module| match module.kind {
            Kind::Ordinary => routes & 1 != 0,
            Kind::Arrays => routes & 2 != 0,
            Kind::Readonly => routes & 4 != 0,
            Kind::Resume => routes & 8 != 0,
        });
    }
}

pub(in crate::jit) use wire::{decode, encode, Binding, Limits};
