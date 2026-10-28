use super::*;

const MAGIC: &[u8; 8] = b"LUNACOD\0";
const HEADER: usize = 88;
const VERSION: u32 = 1;

#[cfg(test)]
mod tests;

fn branch_span(code: usize, relocations: usize) -> bool {
    let limit = if cfg!(target_arch = "aarch64") {
        (1 << 27) - 4
    } else {
        i32::MAX as usize
    };
    relocations
        .checked_mul(24)
        .and_then(|n| n.checked_add(code))
        .and_then(|n| n.checked_add(7))
        .is_some_and(|n| n <= limit)
}

#[derive(Clone, Copy)]
pub(in crate::jit) struct Binding<'a> {
    pub request: u64,
    pub build: &'a [u8; 32],
    pub source: &'a [u8],
    pub environment: &'a [u8],
}

#[derive(Clone, Copy)]
pub(in crate::jit) struct Limits {
    pub bytes: usize,
    pub modules: usize,
    pub declarations: usize,
    pub functions: usize,
    pub relocations: usize,
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], Error> {
        let (first, rest) = self.0.split_at_checked(length).ok_or(Error::Header)?;
        self.0 = rest;
        Ok(first)
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn count(&mut self) -> Result<usize, Error> {
        self.u64()?.try_into().map_err(|_| Error::Limit)
    }
}

#[derive(Clone, Copy)]
struct FunctionView<'a> {
    id: u32,
    alignment: u32,
    bytes: &'a [u8],
    relocations: &'a [u8],
}

struct ModuleView<'a> {
    kind: Kind,
    declarations: &'a [u8],
    functions: [Option<FunctionView<'a>>; 64],
    count: usize,
}

#[derive(Default)]
struct Totals {
    declarations: usize,
    functions: usize,
    relocations: usize,
}

fn add(total: &mut usize, count: usize, limit: usize) -> Result<(), Error> {
    *total = total
        .checked_add(count)
        .filter(|&n| n <= limit)
        .ok_or(Error::Limit)?;
    Ok(())
}

fn declaration(bytes: &[u8], id: usize) -> Result<Declaration, Error> {
    let start = id.checked_mul(16).ok_or(Error::Declaration)?;
    let record = bytes
        .get(start..start.checked_add(16).ok_or(Error::Declaration)?)
        .ok_or(Error::Declaration)?;
    let declaration = Declaration(record.try_into().unwrap());
    declaration.verify()?;
    Ok(declaration)
}

fn relocation(bytes: &[u8]) -> Result<Relocation, Error> {
    let mut reader = Reader(bytes);
    let offset = reader.u32()?;
    let tags = reader.take(4)?;
    if tags[2..] != [0, 0] {
        return Err(Error::Relocation);
    }
    let index = reader.u32()?;
    let target_offset = reader.u32()?;
    let addend = reader.u64()? as i64;
    if !reader.0.is_empty() {
        return Err(Error::Relocation);
    }
    Ok(Relocation {
        offset,
        kind: tags[0],
        target_kind: tags[1],
        index,
        target_offset,
        addend,
    })
}

impl<'a> ModuleView<'a> {
    fn read(reader: &mut Reader<'a>, totals: &mut Totals, limits: Limits) -> Result<Self, Error> {
        let kind = Kind::from_word(reader.u32()?)?;
        let declarations = usize::try_from(reader.u32()?).map_err(|_| Error::Limit)?;
        let count = usize::try_from(reader.u32()?).map_err(|_| Error::Limit)?;
        if reader.u32()? != 0 || declarations == 0 || declarations > 64 || count == 0 || count > 64
        {
            return Err(Error::Module);
        }
        add(&mut totals.declarations, declarations, limits.declarations)?;
        add(&mut totals.functions, count, limits.functions)?;
        let declarations = reader.take(declarations * 16)?;
        let mut view = Self {
            kind,
            declarations,
            functions: [None; 64],
            count,
        };
        for slot in &mut view.functions[..count] {
            let id = reader.u32()?;
            let alignment = reader.u32()?;
            let length = reader.count()?;
            let relocations = reader.count()?;
            add(&mut totals.relocations, relocations, limits.relocations)?;
            let bytes = reader.take(length)?;
            let relocations = reader.take(relocations.checked_mul(24).ok_or(Error::Limit)?)?;
            *slot = Some(FunctionView {
                id,
                alignment,
                bytes,
                relocations,
            });
        }
        view.verify()?;
        Ok(view)
    }
    fn function(&self, id: u32) -> Option<FunctionView<'a>> {
        self.functions[..self.count]
            .iter()
            .flatten()
            .find(|function| function.id == id)
            .copied()
    }
    fn verify(&self) -> Result<(), Error> {
        let mut symbols = 0u16;
        for id in 0..self.declarations.len() / 16 {
            let decl = declaration(self.declarations, id)?;
            if decl.0[0] == 0 {
                if self.function(id as u32).is_none() {
                    return Err(Error::Declaration);
                }
            } else {
                let bit = 1 << decl.0[0];
                if symbols & bit != 0 {
                    return Err(Error::Declaration);
                }
                symbols |= bit;
            }
        }
        for (index, function) in self.functions[..self.count].iter().flatten().enumerate() {
            if function.bytes.is_empty()
                || !function.alignment.is_power_of_two()
                || function.alignment > 4096
                || !branch_span(function.bytes.len(), function.relocations.len() / 24)
            {
                return Err(Error::Module);
            }
            let decl = declaration(self.declarations, function.id as usize)?;
            if decl.0[0] != 0 || (index > 0 && self.functions[index - 1].unwrap().id >= function.id)
            {
                return Err(Error::Declaration);
            }
            if (index == 0 || matches!(self.kind, Kind::Arrays | Kind::Resume))
                && (decl.params() != self.kind.signature() || decl.0[2] != 0)
            {
                return Err(Error::Declaration);
            }
            if matches!(self.kind, Kind::Arrays | Kind::Resume) && !function.relocations.is_empty()
            {
                return Err(Error::Relocation);
            }
            for bytes in function.relocations.chunks_exact(24) {
                let relocation = relocation(bytes)?;
                let width = relocation.width()?;
                if !usize::try_from(relocation.offset)
                    .ok()
                    .and_then(|n| n.checked_add(width))
                    .is_some_and(|n| n <= function.bytes.len())
                {
                    return Err(Error::Relocation);
                }
                if relocation.target_kind > 1
                    || (relocation.kind == 5 && relocation.offset % 4 != 0)
                    || relocation.addend
                        != if matches!(relocation.kind, 2..=4 | 6) {
                            -4
                        } else {
                            0
                        }
                {
                    return Err(Error::Relocation);
                }
                let target = declaration(self.declarations, relocation.index as usize)?;
                if relocation.target_kind == 0 {
                    if relocation.target_offset != 0 {
                        return Err(Error::Relocation);
                    }
                } else {
                    let body = self.function(relocation.index).ok_or(Error::Relocation)?;
                    if target.0[0] != 0 || relocation.target_offset as usize >= body.bytes.len() {
                        return Err(Error::Relocation);
                    }
                }
                if relocation.kind == 6
                    && (relocation.target_kind != 1 || relocation.index != function.id)
                {
                    return Err(Error::Relocation);
                }
            }
        }
        if self.functions[0].unwrap().id != 0 || (self.kind == Kind::Resume && self.count != 1) {
            return Err(Error::Module);
        }
        Ok(())
    }
    fn own(&self, allocator: BudgetAllocator) -> Result<ModuleImage, Error> {
        let mut declarations = BudgetVec::new_in(allocator.clone());
        declarations
            .try_reserve_exact(self.declarations.len() / 16)
            .map_err(|_| Error::Allocation)?;
        for bytes in self.declarations.chunks_exact(16) {
            declarations.push(Declaration(bytes.try_into().unwrap()));
        }
        let mut functions = BudgetVec::new_in(allocator.clone());
        functions
            .try_reserve_exact(self.count)
            .map_err(|_| Error::Allocation)?;
        for function in self.functions[..self.count].iter().flatten() {
            let mut relocations = BudgetVec::new_in(allocator.clone());
            relocations
                .try_reserve_exact(function.relocations.len() / 24)
                .map_err(|_| Error::Allocation)?;
            for bytes in function.relocations.chunks_exact(24) {
                relocations.push(relocation(bytes)?);
            }
            functions.push(Function {
                id: function.id,
                alignment: function.alignment,
                bytes: copy(function.bytes, allocator.clone())?,
                relocations,
            });
        }
        Ok(ModuleImage {
            kind: self.kind,
            declarations,
            functions,
        })
    }
}

fn architecture() -> u32 {
    if cfg!(target_arch = "x86_64") {
        1
    } else {
        2
    }
}

fn header<'a>(
    packet: &'a [u8],
    binding: Binding<'_>,
    limits: Limits,
) -> Result<(Reader<'a>, usize), Error> {
    if packet.len() > limits.bytes {
        return Err(Error::Limit);
    }
    let mut reader = Reader(packet);
    if reader.take(8)? != MAGIC || reader.u32()? != VERSION {
        return Err(Error::Header);
    }
    if reader.u32()? != architecture()
        || binding.request == 0
        || reader.u64()? != binding.request
        || reader.take(32)? != binding.build
    {
        return Err(Error::Identity);
    }
    if reader.count()? != packet.len() {
        return Err(Error::Header);
    }
    let environment = reader.u32()? as usize;
    if reader.u32()? != 0 {
        return Err(Error::Header);
    }
    let source = reader.count()?;
    let modules = reader.count()?;
    if modules == 0 || modules > limits.modules || modules > 16 || environment > 32768 {
        return Err(Error::Limit);
    }
    if reader.take(environment)? != binding.environment || reader.take(source)? != binding.source {
        return Err(Error::Identity);
    }
    Ok((reader, modules))
}

fn scan(packet: &[u8], binding: Binding<'_>, limits: Limits) -> Result<(), Error> {
    let (mut reader, modules) = header(packet, binding, limits)?;
    let mut totals = Totals::default();
    let mut previous = 0;
    let mut kinds = 0u8;
    for _ in 0..modules {
        let module = ModuleView::read(&mut reader, &mut totals, limits)?;
        if module.kind as u8 <= previous {
            return Err(Error::Module);
        }
        previous = module.kind as u8;
        kinds |= 1 << (module.kind as u8 - 1);
    }
    if !matches!(kinds, 1 | 3 | 5 | 11) {
        return Err(Error::Module);
    }
    if !reader.0.is_empty() {
        return Err(Error::Header);
    }
    Ok(())
}

pub(in crate::jit) fn decode(
    packet: &[u8],
    binding: Binding<'_>,
    limits: Limits,
    allocator: BudgetAllocator,
) -> Result<Bundle, Error> {
    scan(packet, binding, limits)?;
    let (mut reader, count) = header(packet, binding, limits)?;
    let mut modules = BudgetVec::new_in(allocator.clone());
    modules
        .try_reserve_exact(count)
        .map_err(|_| Error::Allocation)?;
    let mut totals = Totals::default();
    for _ in 0..count {
        modules.push(ModuleView::read(&mut reader, &mut totals, limits)?.own(allocator.clone())?);
    }
    Ok(Bundle {
        modules,
        environment: copy(binding.environment, allocator)?,
    })
}

pub(in crate::jit) fn encode(
    bundle: &Bundle,
    binding: Binding<'_>,
    limits: Limits,
    allocator: BudgetAllocator,
) -> Result<BudgetVec<u8, BudgetAllocator>, Error> {
    if *bundle.environment != *binding.environment {
        return Err(Error::Identity);
    }
    let mut total = HEADER;
    for length in [binding.environment.len(), binding.source.len()] {
        add(&mut total, length, limits.bytes)?;
    }
    for module in &bundle.modules {
        add(&mut total, 16, limits.bytes)?;
        add(
            &mut total,
            module
                .declarations
                .len()
                .checked_mul(16)
                .ok_or(Error::Limit)?,
            limits.bytes,
        )?;
        for function in &module.functions {
            add(&mut total, 24, limits.bytes)?;
            add(&mut total, function.bytes.len(), limits.bytes)?;
            add(
                &mut total,
                function
                    .relocations
                    .len()
                    .checked_mul(24)
                    .ok_or(Error::Limit)?,
                limits.bytes,
            )?;
        }
    }
    let mut packet = BudgetVec::new_in(allocator);
    packet
        .try_reserve_exact(total)
        .map_err(|_| Error::Allocation)?;
    packet.extend_from_slice(MAGIC);
    for word in [VERSION, architecture()] {
        packet.extend_from_slice(&word.to_le_bytes());
    }
    packet.extend_from_slice(&binding.request.to_le_bytes());
    packet.extend_from_slice(binding.build);
    packet.extend_from_slice(&(total as u64).to_le_bytes());
    packet.extend_from_slice(&(binding.environment.len() as u32).to_le_bytes());
    packet.extend_from_slice(&0u32.to_le_bytes());
    for word in [binding.source.len(), bundle.modules.len()] {
        packet.extend_from_slice(&(word as u64).to_le_bytes());
    }
    packet.extend_from_slice(binding.environment);
    packet.extend_from_slice(binding.source);
    for module in &bundle.modules {
        for word in [
            module.kind as u32,
            module.declarations.len() as u32,
            module.functions.len() as u32,
            0,
        ] {
            packet.extend_from_slice(&word.to_le_bytes());
        }
        for declaration in &module.declarations {
            packet.extend_from_slice(&declaration.0);
        }
        for function in &module.functions {
            for word in [function.id, function.alignment] {
                packet.extend_from_slice(&word.to_le_bytes());
            }
            for word in [function.bytes.len(), function.relocations.len()] {
                packet.extend_from_slice(&(word as u64).to_le_bytes());
            }
            packet.extend_from_slice(&function.bytes);
            for relocation in &function.relocations {
                packet.extend_from_slice(&relocation.offset.to_le_bytes());
                packet.extend_from_slice(&[relocation.kind, relocation.target_kind, 0, 0]);
                packet.extend_from_slice(&relocation.index.to_le_bytes());
                packet.extend_from_slice(&relocation.target_offset.to_le_bytes());
                packet.extend_from_slice(&relocation.addend.to_le_bytes());
            }
        }
    }
    if packet.len() != total {
        return Err(Error::Header);
    }
    scan(&packet, binding, limits)?;
    Ok(packet)
}
