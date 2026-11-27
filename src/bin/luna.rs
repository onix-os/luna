//! `luna eval FILE`: evaluate a functional configuration and print it as canonical JSON, TOML,
//! YAML or INI, or as raw text.

use std::{path::PathBuf, process::ExitCode};

use luna::functional::{evaluate, Api, Data, Options};

const USAGE: &str = "usage: luna eval FILE [options]

Evaluates FILE in a pure functional state and prints the result as canonical JSON.

options:
  --json                 print JSON (the default)
  --pretty               indent the JSON
  --format FORMAT        print json, toml, yaml or ini
  --raw                  print a string result as it is, without quotes or a newline
  --input NAME=JSON      add an input, read as JSON
  --input-string NAME=TEXT
                         add an input, taken as a string
  --inputs FILE          add every field of a JSON object as inputs
  --api NAME             expose a recorded API as the global NAME
  --record PATH          record calls to the API function at PATH, e.g. env.set
  --returning PATH=JSON  an API function at PATH that returns JSON
  --value PATH=JSON      a plain API value at PATH
  --schema FILE          check and normalise the result against a lib.types schema
  --cache DIR            reuse results whose sources and inputs are unchanged
  --root DIR             the directory imports resolve in (default: FILE's directory)
  --fuel N               the most VM instructions the evaluation may run
  --memory BYTES         the most bytes the evaluation may allocate
  --declarations         print {\"value\", \"declarations\"} instead of the value
  --hash                 print the input and output hashes instead of the value
  --files                print every file read, with the SHA-256 of its contents";

struct Command {
    file: PathBuf,
    options: Options,
    inputs: Vec<(Data, Data)>,
    api_name: Option<String>,
    api: Api,
    pretty: bool,
    format: String,
    raw: bool,
    declarations: bool,
    hash: bool,
    files: bool,
}

fn split(argument: &str, flag: &str) -> Result<(String, String), String> {
    argument
        .split_once('=')
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .ok_or_else(|| format!("{flag} expects NAME=VALUE, got '{argument}'"))
}

fn json(text: &str, flag: &str) -> Result<Data, String> {
    Data::from_json(text).map_err(|error| format!("{flag}: {error}"))
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Command, String> {
    match args.next().as_deref() {
        Some("eval") => {}
        Some("--help" | "-h") | None => return Err(String::new()),
        Some(other) => return Err(format!("unknown command '{other}'")),
    }
    let mut file = None;
    let mut command = Command {
        file: PathBuf::new(),
        options: Options::default(),
        inputs: Vec::new(),
        api_name: None,
        api: Api::new(),
        pretty: false,
        format: "json".to_owned(),
        raw: false,
        declarations: false,
        hash: false,
        files: false,
    };
    while let Some(argument) = args.next() {
        let mut value = |flag: &str| args.next().ok_or_else(|| format!("{flag} expects a value"));
        match argument.as_str() {
            "--json" => command.format = "json".to_owned(),
            "--format" => {
                let format = value("--format")?;
                if !matches!(format.as_str(), "json" | "toml" | "yaml" | "ini") {
                    return Err(format!(
                        "--format expects json, toml, yaml or ini, got '{format}'"
                    ));
                }
                command.format = format;
            }
            "--raw" => command.raw = true,
            "--files" => command.files = true,
            "--pretty" => command.pretty = true,
            "--declarations" => command.declarations = true,
            "--hash" => command.hash = true,
            "--input" => {
                let (name, text) = split(&value("--input")?, "--input")?;
                command
                    .inputs
                    .push((Data::from(name), json(&text, "--input")?));
            }
            "--input-string" => {
                let (name, text) = split(&value("--input-string")?, "--input-string")?;
                command.inputs.push((Data::from(name), Data::from(text)));
            }
            "--inputs" => {
                let path = value("--inputs")?;
                let text =
                    std::fs::read_to_string(&path).map_err(|error| format!("{path}: {error}"))?;
                match json(&text, "--inputs")? {
                    Data::Map(entries) => command.inputs.extend(entries),
                    _ => return Err(format!("{path}: --inputs expects a JSON object")),
                }
            }
            "--api" => command.api_name = Some(value("--api")?),
            "--record" => command.api = command.api.record(&value("--record")?),
            "--returning" => {
                let (path, text) = split(&value("--returning")?, "--returning")?;
                command.api = command.api.returning(&path, json(&text, "--returning")?);
            }
            "--value" => {
                let (path, text) = split(&value("--value")?, "--value")?;
                command.api = command.api.value(&path, json(&text, "--value")?);
            }
            "--schema" => command.options.schema = Some(value("--schema")?.into()),
            "--cache" => command.options.cache = Some(value("--cache")?.into()),
            "--root" => command.options.root = Some(value("--root")?.into()),
            "--fuel" => {
                command.options.fuel = value("--fuel")?
                    .parse()
                    .map_err(|_| "--fuel expects a number".to_owned())?
            }
            "--memory" => {
                command.options.memory = Some(
                    value("--memory")?
                        .parse()
                        .map_err(|_| "--memory expects a number of bytes".to_owned())?,
                )
            }
            flag if flag.starts_with("--") => return Err(format!("unknown option '{flag}'")),
            path if file.is_none() => file = Some(PathBuf::from(path)),
            extra => return Err(format!("unexpected argument '{extra}'")),
        }
    }
    command.file = file.ok_or("eval expects a FILE")?;
    Ok(command)
}

fn main() -> ExitCode {
    let mut command = match parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("luna: {message}\n");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    if !command.inputs.is_empty() {
        match Data::map(std::mem::take(&mut command.inputs)) {
            Ok(inputs) => command.options.inputs = inputs,
            Err(error) => {
                eprintln!("luna: inputs: {error}");
                return ExitCode::from(2);
            }
        }
    }
    if let Some(name) = command.api_name.take() {
        command.options.api = Some((name, std::mem::take(&mut command.api)));
    }
    let evaluated = match evaluate(&command.file, &command.options) {
        Ok(evaluated) => evaluated,
        Err(error) => {
            eprintln!("luna: {error}");
            return ExitCode::from(1);
        }
    };
    if command.raw {
        return match &evaluated.value {
            Data::String(bytes) => {
                use std::io::Write;
                match std::io::stdout().write_all(bytes) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(error) => {
                        eprintln!("luna: {error}");
                        ExitCode::from(1)
                    }
                }
            }
            _ => {
                eprintln!("luna: --raw needs the configuration to return a string");
                ExitCode::from(1)
            }
        };
    }
    let output = if command.hash {
        Data::map(vec![
            (Data::from("cached"), Data::Boolean(evaluated.cached)),
            (Data::from("input"), Data::from(evaluated.input_hash)),
            (Data::from("output"), Data::from(evaluated.output_hash)),
        ])
    } else if command.files {
        Data::map(
            evaluated
                .files
                .into_iter()
                .map(|(name, hash)| (Data::from(name), Data::from(hash)))
                .collect(),
        )
    } else if command.declarations {
        Data::map(vec![
            (Data::from("declarations"), evaluated.declarations),
            (Data::from("value"), evaluated.value),
        ])
    } else {
        Ok(evaluated.value)
    };
    let text = output.and_then(|output| match command.format.as_str() {
        "toml" => output.to_toml(),
        "yaml" => output.to_yaml(),
        "ini" => output.to_ini(),
        _ => output.to_json(command.pretty).map(|text| text + "\n"),
    });
    match text {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("luna: {error}");
            ExitCode::from(1)
        }
    }
}
