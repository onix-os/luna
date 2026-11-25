use super::{abi::Slot, ir::Snapshot, prepared_keys::Keys, resources::BudgetAllocator};
use crate::{
    opcode::{Operation, RCIndex},
    types::{
        ConstantIndex16, ConstantIndex8, Opt254, PrototypeIndex, RegisterIndex, UpValueIndex,
        VarCount,
    },
};
use allocator_api2::vec::Vec;

const MAGIC: &[u8; 8] = b"LUNAJIT\0";
const VERSION: u32 = 1;
const HEADER: usize = 72;
const OP_BYTES: usize = 8;

#[derive(Clone, Copy, Debug)]
pub(in crate::jit) struct Limits {
    pub bytes: usize,
    pub operations: usize,
    pub constants: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::jit) enum Error {
    Header,
    Version,
    Limit,
    Padding,
    Operand,
    Opcode,
    Allocation,
    Snapshot,
}

trait Field: Sized {
    fn word(self) -> u16;
    fn from_word(word: u16) -> Option<Self>;
}
impl Field for u8 {
    fn word(self) -> u16 {
        u16::from(self)
    }
    fn from_word(word: u16) -> Option<Self> {
        word.try_into().ok()
    }
}
impl Field for i16 {
    fn word(self) -> u16 {
        self as u16
    }
    fn from_word(word: u16) -> Option<Self> {
        Some(word as i16)
    }
}
impl Field for bool {
    fn word(self) -> u16 {
        u16::from(self)
    }
    fn from_word(word: u16) -> Option<Self> {
        match word {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
}
macro_rules! byte_field {
    ($($name:ident),* $(,)?) => {$(
        impl Field for $name {
            fn word(self) -> u16 { u16::from(self.0) }
            fn from_word(word: u16) -> Option<Self> { Some(Self(word.try_into().ok()?)) }
        }
    )*};
}
byte_field!(RegisterIndex, ConstantIndex8, UpValueIndex, PrototypeIndex);
impl Field for ConstantIndex16 {
    fn word(self) -> u16 {
        self.0
    }
    fn from_word(word: u16) -> Option<Self> {
        Some(Self(word))
    }
}
impl Field for RCIndex {
    fn word(self) -> u16 {
        match self {
            Self::Register(index) => u16::from(index.0),
            Self::Constant(index) => 256 + u16::from(index.0),
        }
    }
    fn from_word(word: u16) -> Option<Self> {
        match word {
            0..=255 => Some(Self::Register(RegisterIndex(word as u8))),
            256..=511 => Some(Self::Constant(ConstantIndex8((word - 256) as u8))),
            _ => None,
        }
    }
}
impl Field for VarCount {
    fn word(self) -> u16 {
        u16::from(self.to_constant().unwrap_or(255))
    }
    fn from_word(word: u16) -> Option<Self> {
        if word == 255 {
            Some(Self::variable())
        } else {
            Self::try_constant(word.try_into().ok()?)
        }
    }
}
impl Field for Opt254 {
    fn word(self) -> u16 {
        u16::from(self.to_u8().unwrap_or(255))
    }
    fn from_word(word: u16) -> Option<Self> {
        let word: u8 = word.try_into().ok()?;
        Self::try_new((word != 255).then_some(word))
    }
}

macro_rules! operations {
    ($($tag:literal $name:ident { $($field:ident),* $(,)? }),* $(,)?) => {
        fn write_operation(op: Operation) -> [u8; OP_BYTES] {
            let (tag, words) = match op {
                $(Operation::$name { $($field),* } => {
                    const _: () = assert!([$(stringify!($field)),*].len() <= 3);
                    let mut words = [0u16; 3];
                    let mut fields = words.iter_mut();
                    $(*fields.next().unwrap() = $field.word();)*
                    ($tag, words)
                },)*
            };
            let mut bytes = [0u8; OP_BYTES];
            bytes[0] = tag;
            for (target, word) in bytes[1..7].chunks_exact_mut(2).zip(words) { target.copy_from_slice(&word.to_le_bytes()); }
            bytes
        }
        fn read_operation(bytes: &[u8]) -> Result<Operation, Error> {
            if bytes.len() != OP_BYTES { return Err(Error::Header); }
            if bytes[7] != 0 { return Err(Error::Padding); }
            let mut words = bytes[1..7].chunks_exact(2).map(|word| u16::from_le_bytes(word.try_into().unwrap()));
            let operation = match bytes[0] {
                $($tag => Operation::$name { $($field: Field::from_word(words.next().unwrap()).ok_or(Error::Operand)?),* },)*
                _ => return Err(Error::Opcode),
            };
            if words.any(|word| word != 0) { return Err(Error::Padding); }
            Ok(operation)
        }
        #[cfg(test)]
        fn all_operations() -> std::vec::Vec<Operation> {
            std::vec![$(Operation::$name { $($field: Field::from_word(0).unwrap()),* }),*]
        }
    };
}

operations! {
    1 Move { dest, source },
    2 LoadConstant { dest, constant },
    3 LoadBool { dest, value, skip_next },
    4 LoadNil { dest, count },
    5 NewTable { dest, array_size, map_size },
    6 GetTable { dest, table, key },
    7 SetTable { table, key, value },
    8 GetUpTable { dest, table, key },
    9 SetUpTable { table, key, value },
    10 SetList { base, count },
    11 Call { func, args, returns },
    12 TailCall { func, args },
    13 Return { start, count },
    14 VarArgs { dest, count },
    15 MarkToBeClosed { source },
    16 Jump { offset, close_upvalues },
    17 Test { value, is_true },
    18 TestSet { dest, value, is_true },
    19 Closure { dest, proto },
    20 NumericForPrep { base, jump },
    21 NumericForLoop { base, jump },
    22 GenericForCall { base, var_count },
    23 GenericForLoop { base, jump },
    24 Method { base, table, key },
    25 Concat { dest, source, count },
    26 GetUpValue { dest, source },
    27 SetUpValue { dest, source },
    28 Length { dest, source },
    29 Eq { skip_if, left, right },
    30 Less { skip_if, left, right },
    31 LessEq { skip_if, left, right },
    32 Not { dest, source },
    33 Minus { dest, source },
    34 Add { dest, left, right },
    35 Sub { dest, left, right },
    36 Mul { dest, left, right },
    37 Div { dest, left, right },
    38 IDiv { dest, left, right },
    39 Mod { dest, left, right },
    40 Pow { dest, left, right },
    41 BitAnd { dest, left, right },
    42 BitOr { dest, left, right },
    43 BitXor { dest, left, right },
    44 ShiftLeft { dest, left, right },
    45 ShiftRight { dest, left, right },
    46 BitNot { dest, source },
}

fn size(
    operations: usize,
    constants: usize,
    hashes: usize,
    limits: Limits,
) -> Result<usize, Error> {
    if operations > limits.operations || constants > limits.constants || hashes > 256 {
        return Err(Error::Limit);
    }
    let bytes = operations
        .checked_mul(OP_BYTES)
        .and_then(|n| constants.checked_mul(16).and_then(|m| n.checked_add(m)))
        .and_then(|n| hashes.checked_mul(8).and_then(|m| n.checked_add(m)))
        .and_then(|n| n.checked_add(HEADER))
        .ok_or(Error::Limit)?;
    if bytes > limits.bytes {
        return Err(Error::Limit);
    }
    Ok(bytes)
}

pub(in crate::jit) fn encode(
    snapshot: &Snapshot,
    limits: Limits,
    allocator: BudgetAllocator,
) -> Result<Vec<u8, BudgetAllocator>, Error> {
    let hashes = snapshot.prepared_keys.as_ref().map_or(&[][..], Keys::words);
    let total = size(
        snapshot.operations.len(),
        snapshot.constants.len(),
        hashes.len(),
        limits,
    )?;
    snapshot.verify().map_err(|_| Error::Snapshot)?;
    let mut bytes = Vec::new_in(allocator);
    bytes
        .try_reserve_exact(total)
        .map_err(|_| Error::Allocation)?;
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    for value in [
        total,
        snapshot.registers,
        snapshot.upvalues,
        snapshot.prototypes,
        snapshot.operations.len(),
        snapshot.constants.len(),
        hashes.len(),
    ] {
        let value = u64::try_from(value).map_err(|_| Error::Limit)?;
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    for &operation in &snapshot.operations {
        bytes.extend_from_slice(&write_operation(operation));
    }
    for slot in &snapshot.constants {
        bytes.extend_from_slice(&slot.tag.to_le_bytes());
        bytes.extend_from_slice(&slot.bits.to_le_bytes());
    }
    for hash in hashes {
        bytes.extend_from_slice(&hash.to_le_bytes());
    }
    debug_assert_eq!(bytes.len(), total);
    Ok(bytes)
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, size: usize) -> Result<&'a [u8], Error> {
        let (first, rest) = self.0.split_at_checked(size).ok_or(Error::Header)?;
        self.0 = rest;
        Ok(first)
    }
    fn word(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn count(&mut self) -> Result<usize, Error> {
        self.word()?.try_into().map_err(|_| Error::Limit)
    }
}

pub(in crate::jit) fn decode(
    bytes: &[u8],
    limits: Limits,
    allocator: BudgetAllocator,
) -> Result<Snapshot, Error> {
    if bytes.len() > limits.bytes {
        return Err(Error::Limit);
    }
    let mut reader = Reader(bytes);
    if reader.take(8)? != MAGIC {
        return Err(Error::Header);
    }
    if reader.take(4)? != VERSION.to_le_bytes() {
        return Err(Error::Version);
    }
    if reader.take(4)? != [0; 4] {
        return Err(Error::Padding);
    }
    let total = reader.count()?;
    let registers = reader.count()?;
    let upvalues = reader.count()?;
    let prototypes = reader.count()?;
    let operations = reader.count()?;
    let constants = reader.count()?;
    let hashes = reader.count()?;
    if registers > 256 {
        return Err(Error::Snapshot);
    }
    if total != bytes.len() || size(operations, constants, hashes, limits)? != total {
        return Err(Error::Header);
    }
    let mut ops = Vec::new_in(allocator.clone());
    ops.try_reserve_exact(operations)
        .map_err(|_| Error::Allocation)?;
    for _ in 0..operations {
        ops.push(read_operation(reader.take(OP_BYTES)?)?);
    }
    let mut slots = Vec::new_in(allocator.clone());
    slots
        .try_reserve_exact(constants)
        .map_err(|_| Error::Allocation)?;
    for _ in 0..constants {
        slots.push(Slot {
            tag: reader.word()?,
            bits: reader.word()?,
        });
    }
    let mut keys = Vec::new_in(allocator);
    keys.try_reserve_exact(hashes)
        .map_err(|_| Error::Allocation)?;
    for _ in 0..hashes {
        keys.push(reader.word()?);
    }
    if !reader.0.is_empty() {
        return Err(Error::Header);
    }
    let mut snapshot = Snapshot {
        prepared_keys: None,
        operations: ops,
        constants: slots,
        registers,
        upvalues,
        prototypes,
    };
    if hashes != 0 {
        snapshot.prepared_keys =
            Some(Keys::from_words(keys, &snapshot).map_err(|_| Error::Snapshot)?);
    }
    snapshot.verify().map_err(|_| Error::Snapshot)?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests;
