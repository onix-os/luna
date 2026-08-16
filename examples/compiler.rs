use std::{error::Error as StdError, fs::File, io::Read};

use luna::{
    compiler::{self, interning::BasicInterner, string_utils::debug_utf8_lossy, CompiledPrototype},
    io,
};

fn print_function<S: AsRef<[u8]>>(function: &CompiledPrototype<S>, depth: usize) {
    let indent = "  ".repeat(depth);
    println!("{indent}===FunctionProto({:p})===", function);
    println!(
        "{indent}fixed_params: {}, has_varargs: {}, stack_size: {}",
        function.fixed_params, function.has_varargs, function.stack_size
    );
    if function.constants.len() > 0 {
        println!("{indent}---constants---");
        for (i, c) in function.constants.iter().enumerate() {
            println!(
                "{indent}{}: {:?}",
                i,
                c.as_string_ref()
                    .map_string(|s| debug_utf8_lossy(s.as_ref()))
            );
        }
    }
    if function.opcodes.len() > 0 {
        println!("{indent}---opcodes---");

        let mut line_number_ind = 0;
        println!("{indent}<line {}>", function.opcode_line_numbers[0].1);

        for (i, c) in function.opcodes.iter().enumerate() {
            if let Some(&(opcode_index, line_number)) =
                function.opcode_line_numbers.get(line_number_ind + 1)
            {
                if i >= opcode_index {
                    line_number_ind += 1;
                    println!("{indent}<line {}>", line_number);
                }
            }
            println!("{indent}{}: {:?}", i, c);
        }
    }
    if function.upvalues.len() > 0 {
        println!("{indent}---upvalues---");
        for (i, u) in function.upvalues.iter().enumerate() {
            println!("{indent}{}: {:?}", i, u);
        }
    }
    if function.prototypes.len() > 0 {
        println!("{indent}---prototypes---");
        for p in &function.prototypes {
            print_function(p, depth + 1);
        }
    }
}

fn main() -> Result<(), Box<dyn StdError>> {
    let mut parse_only = false;
    let mut file_name = None;

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-p" | "--parse" => parse_only = true,
            "-h" | "--help" => {
                println!(
                    "{} {} — {}\n\nusage: compiler [-p|--parse] <file>\n\n  \
                     -p, --parse  parse only and output the AST\n  \
                     -h, --help   print this message",
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION"),
                    env!("CARGO_PKG_DESCRIPTION"),
                );
                return Ok(());
            }
            _ if file_name.is_none() => file_name = Some(arg),
            _ => return Err(format!("unexpected argument: {arg}").into()),
        }
    }

    let file_name = file_name.ok_or("usage: compiler [-p|--parse] <file>")?;

    let mut file = io::buffered_read(File::open(file_name)?)?;
    let mut source = Vec::new();
    file.read_to_end(&mut source)?;

    let mut interner = BasicInterner::default();

    if parse_only {
        let chunk = compiler::parse_chunk(&source, &mut interner)?;
        println!("{:#?}", chunk);
    } else {
        let chunk = compiler::parse_chunk(&source, &mut interner)?;
        let prototype = compiler::compile_chunk(&chunk, &mut interner)?;
        print_function(&prototype, 0);
    }

    Ok(())
}
