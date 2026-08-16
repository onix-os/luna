//! A binary form for compiled functions, and a loader that does not trust it.
//!
//! # The threat model
//!
//! luna is safe Rust, so a malformed chunk cannot corrupt memory. What it *can* do is make the VM
//! index a register past the frame, a constant past the table, or a prototype that is not there —
//! each of which is a panic, and a panic in an embedder is a denial of service or a lost process.
//! So the rule here is that **nothing read from a chunk is used before it has been checked against
//! the thing it indexes**.
//!
//! That rule is enforced structurally rather than by review. Operands are decoded through
//! type-directed readers — a register operand can only be read by `read_register`, which takes the
//! frame's `stack_size` — so an operand cannot be decoded *without* being bounds-checked. Adding an
//! opcode cannot forget the check, because there is nowhere to write one.
//!
//! Sizes are checked before they are believed: a count is rejected if the bytes to satisfy it are
//! not present, so a four-byte length cannot ask for a four-gigabyte allocation.
//!
//! # What is guaranteed, and what is not
//!
//! **Loading is total.** No byte sequence makes the loader panic; it either produces a prototype
//! whose every index and span is in range, or an error. `tests/dump.rs` checks that by truncating a
//! valid chunk at every length and by corrupting every byte to six different values.
//!
//! **Running a crafted chunk is not covered.** Index validation stops the VM reading past a
//! register or a constant, but the compiler also maintains invariants no index check can express —
//! that a frame is balanced when an error unwinds, for instance. A chunk written by hand can
//! violate one and trip an assertion in the VM. That is the same caveat PUC-Rio states about its
//! own `undump`, and it is why `load` requires an explicit `"b"` mode before it will accept binary
//! at all: taking bytecode from somewhere is a decision, not a default.
//!
//! And in the end a *valid* chunk is arbitrary code, exactly as source is. Loading one you did not
//! produce is the same decision as running a script you did not write.

use std::string::String as StdString;

use allocator_api2::vec;
use ottavino_gc_arena::allocator_api::MetricsAlloc;
use thiserror::Error;

use crate::{
    compiler::{FunctionRef, LineNumber, LocalVarInfo},
    opcode::{OpCode, Operation, RCIndex},
    types::{
        ConstantIndex16, ConstantIndex8, Opt254, PrototypeIndex, RegisterIndex, UpValueDescriptor,
        UpValueIndex, VarCount,
    },
    Constant, Context, FunctionPrototype, String,
};

/// Chunks start with this, so a loader can tell source from bytecode before parsing either.
pub const SIGNATURE: &[u8; 5] = b"\x1bLuna";

/// Bumped whenever the encoding changes. A chunk from another version is refused rather than
/// misread — there is no compatibility story, and pretending otherwise is how loaders get exploited.
pub const VERSION: u8 = 1;

#[derive(Debug, Clone, Error)]
pub enum DumpError {
    #[error("not a luna chunk")]
    BadSignature,
    #[error("chunk is version {found}, this build reads version {expected}")]
    BadVersion { found: u8, expected: u8 },
    #[error("chunk ends in the middle of a {0}")]
    Truncated(&'static str),
    #[error("chunk claims {count} {what}, more than the remaining {remaining} bytes allow")]
    ImpossibleCount {
        what: &'static str,
        count: u64,
        remaining: usize,
    },
    #[error("unknown {what} tag {tag}")]
    BadTag { what: &'static str, tag: u8 },
    #[error("{what} index {index} is out of range ({limit})")]
    OutOfRange {
        what: &'static str,
        index: u64,
        limit: usize,
    },
    #[error("a jump from instruction {from} lands outside the function")]
    BadJump { from: usize },
    #[error("string is not valid for this chunk")]
    BadString,
    #[error("{0} bytes of trailing data after the chunk")]
    TrailingData(usize),
}

/// What a prototype's operands are allowed to name. Filled in as the prototype is decoded, so that
/// every operand is checked against the real thing rather than a guess.
struct Limits {
    stack_size: usize,
    constants: usize,
    upvalues: usize,
    prototypes: usize,
    opcodes: usize,
}

// ---------------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------------

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn i16(&mut self, v: i16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn bytes(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
    }
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], DumpError> {
        let end = self.at.checked_add(n).ok_or(DumpError::Truncated(what))?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or(DumpError::Truncated(what))?;
        self.at = end;
        Ok(slice)
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, DumpError> {
        Ok(self.take(1, what)?[0])
    }

    fn u16(&mut self, what: &'static str) -> Result<u16, DumpError> {
        Ok(u16::from_le_bytes(self.take(2, what)?.try_into().unwrap()))
    }

    fn i16(&mut self, what: &'static str) -> Result<i16, DumpError> {
        Ok(i16::from_le_bytes(self.take(2, what)?.try_into().unwrap()))
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, DumpError> {
        Ok(u32::from_le_bytes(self.take(4, what)?.try_into().unwrap()))
    }

    fn u64(&mut self, what: &'static str) -> Result<u64, DumpError> {
        Ok(u64::from_le_bytes(self.take(8, what)?.try_into().unwrap()))
    }

    fn bytes(&mut self, what: &'static str) -> Result<&'a [u8], DumpError> {
        let len = self.u32(what)? as usize;
        self.take(len, what)
    }

    /// A count that the remaining input could actually satisfy.
    ///
    /// `least` is the smallest number of bytes one element can occupy. Without this a four-byte
    /// length field asks for a four-gigabyte allocation before anyone notices the chunk is nine
    /// bytes long.
    fn count(&self, raw: u32, least: usize, what: &'static str) -> Result<usize, DumpError> {
        let count = raw as usize;
        if count.saturating_mul(least.max(1)) > self.remaining() {
            return Err(DumpError::ImpossibleCount {
                what,
                count: raw as u64,
                remaining: self.remaining(),
            });
        }
        Ok(count)
    }
}

fn checked(index: u64, limit: usize, what: &'static str) -> Result<(), DumpError> {
    if (index as usize) < limit {
        Ok(())
    } else {
        Err(DumpError::OutOfRange { what, index, limit })
    }
}

// ---------------------------------------------------------------------------------------------
// Operands
//
// Every operand kind has a writer and a *checking* reader. The reader takes whatever it must be
// checked against, so decoding an operand without checking it is not something that can be written.
// ---------------------------------------------------------------------------------------------

macro_rules! write_operand {
    (reg, $v:expr, $w:expr) => {
        $w.u8($v.0)
    };
    (up, $v:expr, $w:expr) => {
        $w.u8($v.0)
    };
    (proto, $v:expr, $w:expr) => {
        $w.u8($v.0)
    };
    (k16, $v:expr, $w:expr) => {
        $w.u16($v.0)
    };
    (rc, $v:expr, $w:expr) => {
        match $v {
            RCIndex::Register(r) => {
                $w.u8(0);
                $w.u8(r.0);
            }
            RCIndex::Constant(c) => {
                $w.u8(1);
                $w.u8(c.0);
            }
        }
    };
    (vc, $v:expr, $w:expr) => {
        $w.u8(match $v.to_constant() {
            Some(v) => v,
            None => 255,
        })
    };
    (opt, $v:expr, $w:expr) => {
        $w.u8(match $v.to_u8() {
            Some(v) => v,
            None => 255,
        })
    };
    (jmp, $v:expr, $w:expr) => {
        $w.i16($v)
    };
    (bool, $v:expr, $w:expr) => {
        $w.u8($v as u8)
    };
    (byte, $v:expr, $w:expr) => {
        $w.u8($v)
    };
}

macro_rules! read_operand {
    (reg, $r:expr, $l:expr, $i:expr) => {{
        let v = $r.u8("register")?;
        checked(v as u64, $l.stack_size, "register")?;
        RegisterIndex(v)
    }};
    (up, $r:expr, $l:expr, $i:expr) => {{
        let v = $r.u8("upvalue")?;
        checked(v as u64, $l.upvalues, "upvalue")?;
        UpValueIndex(v)
    }};
    (proto, $r:expr, $l:expr, $i:expr) => {{
        let v = $r.u8("prototype")?;
        checked(v as u64, $l.prototypes, "prototype")?;
        PrototypeIndex(v)
    }};
    (k16, $r:expr, $l:expr, $i:expr) => {{
        let v = $r.u16("constant")?;
        checked(v as u64, $l.constants, "constant")?;
        ConstantIndex16(v)
    }};
    (rc, $r:expr, $l:expr, $i:expr) => {{
        match $r.u8("operand kind")? {
            0 => {
                let v = $r.u8("register")?;
                checked(v as u64, $l.stack_size, "register")?;
                RCIndex::Register(RegisterIndex(v))
            }
            1 => {
                let v = $r.u8("constant")?;
                checked(v as u64, $l.constants, "constant")?;
                RCIndex::Constant(ConstantIndex8(v))
            }
            tag => {
                return Err(DumpError::BadTag {
                    what: "operand",
                    tag,
                })
            }
        }
    }};
    (vc, $r:expr, $l:expr, $i:expr) => {{
        let v = $r.u8("var count")?;
        if v == 255 {
            VarCount::variable()
        } else {
            VarCount::try_constant(v).ok_or(DumpError::BadTag {
                what: "var count",
                tag: v,
            })?
        }
    }};
    (opt, $r:expr, $l:expr, $i:expr) => {{
        let v = $r.u8("optional register")?;
        if v != 255 {
            checked(v as u64, $l.stack_size, "register")?;
        }
        Opt254::try_new(if v == 255 { None } else { Some(v) }).ok_or(DumpError::BadTag {
            what: "optional register",
            tag: v,
        })?
    }};
    (jmp, $r:expr, $l:expr, $i:expr) => {{
        let offset = $r.i16("jump")?;
        // `pc` has already advanced past this instruction when the jump is taken, so the target is
        // the next index plus the offset — and it has to land on an instruction that exists.
        let target = ($i as i64) + 1 + (offset as i64);
        if target < 0 || target >= $l.opcodes as i64 {
            return Err(DumpError::BadJump { from: $i });
        }
        offset
    }};
    (bool, $r:expr, $l:expr, $i:expr) => {
        $r.u8("flag")? != 0
    };
    (byte, $r:expr, $l:expr, $i:expr) => {
        $r.u8("byte")?
    };
}

/// The opcode table, declared once.
///
/// The writer's `match` is exhaustive, so a new `Operation` variant fails to compile until it is
/// listed here; the reader's operands go through the checking readers above, so a new variant
/// cannot be decoded without its indices being validated.
macro_rules! opcodes {
    ($($tag:literal $name:ident { $($field:ident : $kind:ident),* $(,)? }),* $(,)?) => {
        fn write_operation(op: Operation, w: &mut Writer) {
            match op {
                $(Operation::$name { $($field),* } => {
                    w.u8($tag);
                    $(write_operand!($kind, $field, w);)*
                })*
            }
        }

        fn read_operation(
            r: &mut Reader<'_>,
            limits: &Limits,
            index: usize,
        ) -> Result<Operation, DumpError> {
            let tag = r.u8("opcode")?;
            Ok(match tag {
                $($tag => Operation::$name {
                    $($field: read_operand!($kind, r, limits, index)),*
                },)*
                tag => return Err(DumpError::BadTag { what: "opcode", tag }),
            })
        }
    };
}

opcodes! {
    1 Move { dest: reg, source: reg },
    2 LoadConstant { dest: reg, constant: k16 },
    3 LoadBool { dest: reg, value: bool, skip_next: bool },
    4 LoadNil { dest: reg, count: byte },
    5 NewTable { dest: reg, array_size: byte, map_size: byte },
    6 GetTable { dest: reg, table: reg, key: rc },
    7 SetTable { table: reg, key: rc, value: rc },
    8 GetUpTable { dest: reg, table: up, key: rc },
    9 SetUpTable { table: up, key: rc, value: rc },
    10 SetList { base: reg, count: vc },
    11 Call { func: reg, args: vc, returns: vc },
    12 TailCall { func: reg, args: vc },
    13 Return { start: reg, count: vc },
    14 VarArgs { dest: reg, count: vc },
    15 MarkToBeClosed { source: reg },
    16 Jump { offset: jmp, close_upvalues: opt },
    17 Test { value: reg, is_true: bool },
    18 TestSet { dest: reg, value: reg, is_true: bool },
    19 Closure { dest: reg, proto: proto },
    20 NumericForPrep { base: reg, jump: jmp },
    21 NumericForLoop { base: reg, jump: jmp },
    22 GenericForCall { base: reg, var_count: byte },
    23 GenericForLoop { base: reg, jump: jmp },
    24 Method { base: reg, table: reg, key: rc },
    25 Concat { dest: reg, source: reg, count: byte },
    26 GetUpValue { dest: reg, source: up },
    27 SetUpValue { dest: up, source: reg },
    28 Length { dest: reg, source: reg },
    29 Eq { skip_if: bool, left: rc, right: rc },
    30 Less { skip_if: bool, left: rc, right: rc },
    31 LessEq { skip_if: bool, left: rc, right: rc },
    32 Not { dest: reg, source: reg },
    33 Minus { dest: reg, source: reg },
    34 Add { dest: reg, left: rc, right: rc },
    35 Sub { dest: reg, left: rc, right: rc },
    36 Mul { dest: reg, left: rc, right: rc },
    37 Div { dest: reg, left: rc, right: rc },
    38 IDiv { dest: reg, left: rc, right: rc },
    39 Mod { dest: reg, left: rc, right: rc },
    40 Pow { dest: reg, left: rc, right: rc },
    41 BitAnd { dest: reg, left: rc, right: rc },
    42 BitOr { dest: reg, left: rc, right: rc },
    43 BitXor { dest: reg, left: rc, right: rc },
    44 ShiftLeft { dest: reg, left: rc, right: rc },
    45 ShiftRight { dest: reg, left: rc, right: rc },
    46 BitNot { dest: reg, source: reg },
}

/// Check the operands an instruction uses *together*.
///
/// Per-operand checking cannot see these: `Concat { source, count }` reads the whole span
/// `source .. source + count`, and both halves are individually in range while their sum is not.
/// That is not hypothetical — the mutation test found it as a slice-index panic in the VM.
///
/// The match is exhaustive with no catch-all on purpose. A new opcode will not compile until
/// someone has decided whether it addresses a span, which is the only way this stays correct.
fn check_spans(op: &Operation, stack_size: usize) -> Result<(), DumpError> {
    fn span(start: u8, len: usize, stack_size: usize) -> Result<(), DumpError> {
        let end = start as usize + len;
        if end <= stack_size {
            Ok(())
        } else {
            Err(DumpError::OutOfRange {
                what: "register span",
                index: end as u64,
                limit: stack_size,
            })
        }
    }

    // A variable count is bounded by the stack at run time, not by the instruction.
    let counted = |c: VarCount| c.to_constant().map(|n| n as usize).unwrap_or(0);

    match op {
        Operation::LoadNil { dest, count } => span(dest.0, *count as usize, stack_size),
        Operation::Concat { source, count, .. } => span(source.0, *count as usize, stack_size),
        Operation::VarArgs { dest, count } => span(dest.0, counted(*count), stack_size),
        Operation::SetList { base, count } => span(base.0, counted(*count), stack_size),
        Operation::Return { start, count } => span(start.0, counted(*count), stack_size),
        // A call reads the function and its arguments, and writes its returns back over them.
        Operation::Call {
            func,
            args,
            returns,
        } => {
            span(func.0, 1 + counted(*args), stack_size)?;
            span(func.0, counted(*returns), stack_size)
        }
        Operation::TailCall { func, args } => span(func.0, 1 + counted(*args), stack_size),
        // The generic `for` protocol keeps three values at `base` and calls with two of them.
        Operation::GenericForCall { base, var_count } => {
            span(base.0, 3 + *var_count as usize, stack_size)
        }
        Operation::GenericForLoop { base, .. } => span(base.0, 2, stack_size),
        // The numeric `for` keeps its control, limit and step together.
        Operation::NumericForPrep { base, .. } | Operation::NumericForLoop { base, .. } => {
            span(base.0, 3, stack_size)
        }
        // A method call writes the receiver beside the function.
        Operation::Method { base, .. } => span(base.0, 2, stack_size),

        // Everything else addresses single registers, which are already checked as they are read.
        Operation::Move { .. }
        | Operation::LoadConstant { .. }
        | Operation::LoadBool { .. }
        | Operation::NewTable { .. }
        | Operation::GetTable { .. }
        | Operation::SetTable { .. }
        | Operation::GetUpTable { .. }
        | Operation::SetUpTable { .. }
        | Operation::MarkToBeClosed { .. }
        | Operation::Jump { .. }
        | Operation::Test { .. }
        | Operation::TestSet { .. }
        | Operation::Closure { .. }
        | Operation::GetUpValue { .. }
        | Operation::SetUpValue { .. }
        | Operation::Length { .. }
        | Operation::Eq { .. }
        | Operation::Less { .. }
        | Operation::LessEq { .. }
        | Operation::Not { .. }
        | Operation::Minus { .. }
        | Operation::Add { .. }
        | Operation::Sub { .. }
        | Operation::Mul { .. }
        | Operation::Div { .. }
        | Operation::IDiv { .. }
        | Operation::Mod { .. }
        | Operation::Pow { .. }
        | Operation::BitAnd { .. }
        | Operation::BitOr { .. }
        | Operation::BitXor { .. }
        | Operation::ShiftLeft { .. }
        | Operation::ShiftRight { .. }
        | Operation::BitNot { .. } => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// Prototypes
// ---------------------------------------------------------------------------------------------

/// Serialise a compiled function.
///
/// With `strip`, local-variable names are left out — the same trade `debug.getlocal` documents,
/// and the reason PUC-Rio's `string.dump` takes the flag.
pub fn dump(proto: &FunctionPrototype<'_>, strip: bool) -> Vec<u8> {
    let mut w = Writer(Vec::new());
    w.0.extend_from_slice(SIGNATURE);
    w.u8(VERSION);
    w.u8(strip as u8);
    w.bytes(proto.chunk_name.as_bytes());
    write_prototype(proto, strip, &mut w);
    w.0
}

fn write_prototype(proto: &FunctionPrototype<'_>, strip: bool, w: &mut Writer) {
    match &proto.reference {
        FunctionRef::Chunk => w.u8(0),
        FunctionRef::Expression(line) => {
            w.u8(1);
            w.u64(line.0);
        }
        FunctionRef::Named(name, line) => {
            w.u8(2);
            w.bytes(name.as_bytes());
            w.u64(line.0);
        }
    }

    w.u8(proto.fixed_params);
    w.u8(proto.has_varargs as u8);
    w.u16(proto.stack_size);

    w.u32(proto.constants.len() as u32);
    for constant in proto.constants.iter() {
        match constant {
            Constant::Nil => w.u8(0),
            Constant::Boolean(b) => {
                w.u8(1);
                w.u8(*b as u8);
            }
            Constant::Integer(i) => {
                w.u8(2);
                w.u64(*i as u64);
            }
            Constant::Number(n) => {
                w.u8(3);
                w.u64(n.to_bits());
            }
            Constant::String(s) => {
                w.u8(4);
                w.bytes(s.as_bytes());
            }
        }
    }

    w.u32(proto.upvalues.len() as u32);
    for upvalue in proto.upvalues.iter() {
        match upvalue {
            UpValueDescriptor::Environment => w.u8(0),
            UpValueDescriptor::ParentLocal(r) => {
                w.u8(1);
                w.u8(r.0);
            }
            UpValueDescriptor::Outer(u) => {
                w.u8(2);
                w.u8(u.0);
            }
        }
    }

    // Nested prototypes before the opcodes that name them, so the reader knows the limit before it
    // has to check against it.
    w.u32(proto.prototypes.len() as u32);
    for nested in proto.prototypes.iter() {
        write_prototype(nested, strip, w);
    }

    w.u32(proto.opcodes.len() as u32);
    for opcode in proto.opcodes.iter() {
        write_operation(opcode.decode(), w);
    }

    w.u32(proto.opcode_line_numbers.len() as u32);
    for (index, line) in proto.opcode_line_numbers.iter() {
        w.u32(*index as u32);
        w.u64(line.0);
    }

    if strip {
        w.u32(0);
    } else {
        w.u32(proto.locals.len() as u32);
        for local in proto.locals.iter() {
            w.bytes(local.name.as_bytes());
            w.u8(local.register.0);
            w.u32(local.start_pc as u32);
            w.u32(local.end_pc as u32);
        }
    }
}

/// Load a chunk produced by [`dump`].
///
/// Every index in the chunk is checked against what it names before it is used, and a chunk from a
/// different format version is refused rather than reinterpreted.
pub fn undump<'gc>(ctx: Context<'gc>, bytes: &[u8]) -> Result<FunctionPrototype<'gc>, DumpError> {
    let mut r = Reader { bytes, at: 0 };

    if r.take(SIGNATURE.len(), "signature")? != SIGNATURE {
        return Err(DumpError::BadSignature);
    }
    let version = r.u8("version")?;
    if version != VERSION {
        return Err(DumpError::BadVersion {
            found: version,
            expected: VERSION,
        });
    }
    let _stripped = r.u8("flags")?;
    let chunk_name = ctx.intern(r.bytes("chunk name")?);

    let proto = read_prototype(ctx, &mut r, chunk_name)?;

    if r.remaining() != 0 {
        return Err(DumpError::TrailingData(r.remaining()));
    }
    Ok(proto)
}

fn read_prototype<'gc>(
    ctx: Context<'gc>,
    r: &mut Reader<'_>,
    chunk_name: String<'gc>,
) -> Result<FunctionPrototype<'gc>, DumpError> {
    let alloc = MetricsAlloc::new(&ctx);

    let reference = match r.u8("function reference")? {
        0 => FunctionRef::Chunk,
        1 => FunctionRef::Expression(LineNumber(r.u64("line")?)),
        2 => {
            let name = ctx.intern(r.bytes("function name")?);
            FunctionRef::Named(name, LineNumber(r.u64("line")?))
        }
        tag => {
            return Err(DumpError::BadTag {
                what: "function reference",
                tag,
            })
        }
    };

    let fixed_params = r.u8("parameter count")?;
    let has_varargs = r.u8("varargs flag")? != 0;
    let stack_size = r.u16("stack size")?;

    let raw = r.u32("constant count")?;
    let count = r.count(raw, 1, "constants")?;
    let mut constants = vec::Vec::new_in(alloc.clone());
    constants.reserve(count);
    for _ in 0..count {
        constants.push(match r.u8("constant")? {
            0 => Constant::Nil,
            1 => Constant::Boolean(r.u8("boolean")? != 0),
            2 => Constant::Integer(r.u64("integer")? as i64),
            3 => Constant::Number(f64::from_bits(r.u64("number")?)),
            4 => Constant::String(ctx.intern(r.bytes("string")?)),
            tag => {
                return Err(DumpError::BadTag {
                    what: "constant",
                    tag,
                })
            }
        });
    }

    let raw = r.u32("upvalue count")?;
    let count = r.count(raw, 1, "upvalues")?;
    let mut upvalues = vec::Vec::new_in(alloc.clone());
    upvalues.reserve(count);
    for _ in 0..count {
        upvalues.push(match r.u8("upvalue")? {
            0 => UpValueDescriptor::Environment,
            1 => {
                let v = r.u8("parent local")?;
                // A parent local names a register in the *enclosing* frame, whose size is not
                // known here. `u8` is the whole range the VM can address, and the closure builder
                // checks it against the real frame when it runs.
                UpValueDescriptor::ParentLocal(RegisterIndex(v))
            }
            2 => UpValueDescriptor::Outer(UpValueIndex(r.u8("outer upvalue")?)),
            tag => {
                return Err(DumpError::BadTag {
                    what: "upvalue",
                    tag,
                })
            }
        });
    }

    let raw = r.u32("prototype count")?;
    // A nested prototype is at least a handful of bytes; one byte each is the safe floor.
    let count = r.count(raw, 1, "prototypes")?;
    let mut prototypes = vec::Vec::new_in(alloc.clone());
    prototypes.reserve(count);
    for _ in 0..count {
        let nested = read_prototype(ctx, r, chunk_name)?;
        prototypes.push(ottavino_gc_arena::Gc::new(&ctx, nested));
    }

    let raw = r.u32("opcode count")?;
    let opcode_count = r.count(raw, 2, "opcodes")?;
    let limits = Limits {
        stack_size: stack_size as usize,
        constants: constants.len(),
        upvalues: upvalues.len(),
        prototypes: prototypes.len(),
        opcodes: opcode_count,
    };
    let mut opcodes = vec::Vec::new_in(alloc.clone());
    opcodes.reserve(opcode_count);
    for index in 0..opcode_count {
        let op = read_operation(r, &limits, index)?;
        check_spans(&op, limits.stack_size)?;
        opcodes.push(OpCode::encode(op));
    }

    let raw = r.u32("line number count")?;
    let count = r.count(raw, 12, "line numbers")?;
    let mut opcode_line_numbers = vec::Vec::new_in(alloc.clone());
    opcode_line_numbers.reserve(count);
    for _ in 0..count {
        let index = r.u32("line index")? as usize;
        checked(index as u64, opcode_count.max(1), "line index")?;
        opcode_line_numbers.push((index, LineNumber(r.u64("line")?)));
    }

    let raw = r.u32("local count")?;
    let count = r.count(raw, 13, "locals")?;
    let mut locals = vec::Vec::new_in(alloc);
    locals.reserve(count);
    for _ in 0..count {
        let name = ctx.intern(r.bytes("local name")?);
        let register = r.u8("local register")?;
        checked(
            register as u64,
            stack_size.max(1) as usize,
            "local register",
        )?;
        locals.push(LocalVarInfo {
            name,
            register: RegisterIndex(register),
            start_pc: r.u32("local start")? as usize,
            end_pc: r.u32("local end")? as usize,
        });
    }

    Ok(FunctionPrototype {
        chunk_name,
        reference,
        fixed_params,
        has_varargs,
        stack_size,
        constants: constants.into_boxed_slice(),
        opcodes: opcodes.into_boxed_slice(),
        opcode_line_numbers: opcode_line_numbers.into_boxed_slice(),
        upvalues: upvalues.into_boxed_slice(),
        prototypes: prototypes.into_boxed_slice(),
        locals: locals.into_boxed_slice(),
    })
}

/// Whether these bytes look like a dumped chunk rather than source.
pub fn is_binary_chunk(bytes: &[u8]) -> bool {
    bytes.starts_with(SIGNATURE)
}

impl From<DumpError> for StdString {
    fn from(err: DumpError) -> StdString {
        err.to_string()
    }
}
