mod compiler;
pub mod interning;
pub mod lexer;
mod operators;
pub mod parser;
mod register_allocator;
pub mod string_utils;

pub use self::{
    compiler::{
        compile_chunk, compile_chunk_with, CompileError, CompileErrorKind, CompiledPrototype,
        FunctionRef, LocalVarInfo,
    },
    interning::StringInterner,
    lexer::LineNumber,
    parser::{parse_chunk, ParseError, ParseErrorKind},
};
