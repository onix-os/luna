use super::{Error, Relocation};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Architecture {
    X64,
    A64,
}

impl Architecture {
    pub(super) fn native() -> Self {
        if cfg!(target_arch = "aarch64") {
            Self::A64
        } else {
            Self::X64
        }
    }

    fn width(self, kind: u8) -> Result<usize, Error> {
        match (self, kind) {
            (_, 1) => Ok(8),
            (Self::X64, 2..=4 | 6) | (Self::A64, 5) => Ok(4),
            _ => Err(Error::Relocation),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Layout {
    code: usize,
    veneers: usize,
    got: usize,
    got_count: usize,
    pub(super) bytes: usize,
}

impl Layout {
    pub(super) fn new(
        code: usize,
        relocations: &[Relocation],
        architecture: Architecture,
    ) -> Result<Self, Error> {
        if code == 0 {
            return Err(Error::Module);
        }
        let (mut veneers, mut got_count) = (0usize, 0usize);
        for relocation in relocations {
            let width = architecture.width(relocation.kind)?;
            if !(relocation.offset as usize)
                .checked_add(width)
                .is_some_and(|end| end <= code)
            {
                return Err(Error::Relocation);
            }
            if matches!(relocation.kind, 2 | 3 | 5) {
                veneers = veneers.checked_add(1).ok_or(Error::Limit)?;
            }
            if relocation.kind == 4 {
                got_count = got_count.checked_add(1).ok_or(Error::Limit)?;
            }
        }
        let end = veneers
            .checked_mul(16)
            .and_then(|n| code.checked_add(n))
            .ok_or(Error::Limit)?;
        let got = if got_count == 0 {
            end
        } else {
            end.checked_add(7).ok_or(Error::Limit)? & !7
        };
        let bytes = got_count
            .checked_mul(8)
            .and_then(|n| got.checked_add(n))
            .filter(|&n| n <= isize::MAX as usize)
            .ok_or(Error::Limit)?;
        Ok(Self {
            code,
            veneers,
            got,
            got_count,
            bytes,
        })
    }
}

fn write<const N: usize>(bytes: &mut [u8], at: usize, value: [u8; N]) -> Result<(), Error> {
    let end = at.checked_add(N).ok_or(Error::Relocation)?;
    bytes
        .get_mut(at..end)
        .ok_or(Error::Relocation)?
        .copy_from_slice(&value);
    Ok(())
}

fn displacement(target: u64, at: u64) -> Result<i32, Error> {
    i32::try_from(i128::from(target) - i128::from(at)).map_err(|_| Error::Relocation)
}

fn branch(target: u64, at: u64) -> Result<u32, Error> {
    let delta = i128::from(target) - i128::from(at);
    if delta % 4 != 0 || !(-(1i128 << 27)..(1i128 << 27)).contains(&delta) {
        return Err(Error::Relocation);
    }
    Ok(((delta / 4) as u32) & 0x03ff_ffff)
}

pub(super) struct Patcher<'a> {
    bytes: &'a mut [u8],
    layout: Layout,
    architecture: Architecture,
    base: u64,
    veneers: usize,
    got: usize,
}

impl<'a> Patcher<'a> {
    pub(super) fn new(
        bytes: &'a mut [u8],
        layout: Layout,
        architecture: Architecture,
        base: u64,
    ) -> Result<Self, Error> {
        if bytes.len() != layout.bytes || base.checked_add(layout.bytes as u64).is_none() {
            return Err(Error::Relocation);
        }
        Ok(Self {
            bytes,
            layout,
            architecture,
            base,
            veneers: 0,
            got: 0,
        })
    }

    fn address(&self, offset: usize) -> Result<u64, Error> {
        self.base
            .checked_add(offset as u64)
            .ok_or(Error::Relocation)
    }

    fn veneer(&mut self) -> Result<usize, Error> {
        if self.veneers >= self.layout.veneers {
            return Err(Error::Relocation);
        }
        let offset = self
            .veneers
            .checked_mul(16)
            .and_then(|n| self.layout.code.checked_add(n))
            .ok_or(Error::Relocation)?;
        self.veneers += 1;
        Ok(offset)
    }

    pub(super) fn apply(&mut self, relocation: &Relocation, target: u64) -> Result<(), Error> {
        let offset = relocation.offset as usize;
        let width = self.architecture.width(relocation.kind)?;
        if !offset
            .checked_add(width)
            .is_some_and(|end| end <= self.layout.code)
        {
            return Err(Error::Relocation);
        }
        let at = self.address(offset)?;
        let what = target
            .checked_add_signed(relocation.addend)
            .ok_or(Error::Relocation)?;
        match relocation.kind {
            1 => write(self.bytes, offset, what.to_le_bytes()),
            6 => write(self.bytes, offset, displacement(what, at)?.to_le_bytes()),
            2 | 3 => {
                let delta = match displacement(what, at) {
                    Ok(delta) => delta,
                    Err(_) => {
                        let veneer = self.veneer()?;
                        let destination = what.checked_add(4).ok_or(Error::Relocation)?;
                        let delta = displacement(
                            self.address(veneer)?,
                            at.checked_add(4).ok_or(Error::Relocation)?,
                        )?;
                        write(self.bytes, veneer, [0xff, 0x25, 0, 0, 0, 0])?;
                        write(self.bytes, veneer + 6, destination.to_le_bytes())?;
                        delta
                    }
                };
                write(self.bytes, offset, delta.to_le_bytes())
            }
            4 => {
                let direct = offset
                    .checked_sub(3)
                    .and_then(|start| self.bytes.get(start..offset));
                let mov = direct.is_some_and(
                    |bytes| matches!(bytes, [0x48..=0x4f, 0x8b, modrm] if modrm & 0xc7 == 0x05),
                );
                if let (true, Ok(delta)) = (mov, displacement(what, at)) {
                    self.bytes[offset - 2] = 0x8d;
                    return write(self.bytes, offset, delta.to_le_bytes());
                }
                if self.got >= self.layout.got_count {
                    return Err(Error::Relocation);
                }
                let entry = self
                    .got
                    .checked_mul(8)
                    .and_then(|n| self.layout.got.checked_add(n))
                    .ok_or(Error::Relocation)?;
                let destination = what.checked_add(4).ok_or(Error::Relocation)?;
                let delta = displacement(
                    self.address(entry)?,
                    at.checked_add(4).ok_or(Error::Relocation)?,
                )?;
                write(self.bytes, entry, destination.to_le_bytes())?;
                write(self.bytes, offset, delta.to_le_bytes())?;
                self.got += 1;
                Ok(())
            }
            5 => {
                if at % 4 != 0 || what % 4 != 0 {
                    return Err(Error::Relocation);
                }
                let instruction =
                    u32::from_le_bytes(self.bytes[offset..offset + 4].try_into().unwrap());
                if instruction & 0x7c00_0000 != 0x1400_0000 {
                    return Err(Error::Relocation);
                }
                let immediate = match branch(what, at) {
                    Ok(immediate) => immediate,
                    Err(_) => {
                        let veneer = self.veneer()?;
                        let address = self.address(veneer)?;
                        if address % 4 != 0 {
                            return Err(Error::Relocation);
                        }
                        let immediate = branch(address, at)?;
                        write(self.bytes, veneer, 0x5800_0050u32.to_le_bytes())?;
                        write(self.bytes, veneer + 4, 0xd61f_0200u32.to_le_bytes())?;
                        write(self.bytes, veneer + 8, what.to_le_bytes())?;
                        immediate
                    }
                };
                write(
                    self.bytes,
                    offset,
                    ((instruction & !0x03ff_ffff) | immediate).to_le_bytes(),
                )
            }
            _ => Err(Error::Relocation),
        }
    }
}

pub(super) fn protection(
    environment: &[u8],
    architecture: Architecture,
) -> Result<cranelift_jit::BranchProtection, Error> {
    let mut bti = None;
    for field in environment.split(|&byte| byte == b';') {
        if let Some(value) = field.strip_prefix(b"use_bti=") {
            if bti.is_some() {
                return Err(Error::Identity);
            }
            bti = Some(match value {
                b"0" => false,
                b"1" => true,
                _ => return Err(Error::Identity),
            });
        }
    }
    match (architecture, bti) {
        (Architecture::A64, Some(true)) => Ok(cranelift_jit::BranchProtection::BTI),
        (Architecture::A64, None) | (Architecture::X64, Some(_)) => Err(Error::Identity),
        _ => Ok(cranelift_jit::BranchProtection::None),
    }
}
