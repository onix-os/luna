use std::{
    env,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use proc_macro::TokenStream;
use sha2::{Digest, Sha256};

mod elf;

#[proc_macro]
pub fn image(input: TokenStream) -> TokenStream {
    let result = if input.is_empty() {
        build()
    } else {
        Err("image! takes no arguments".into())
    };
    match result {
        Ok(output) => output.parse().unwrap(),
        Err(error) => format!("compile_error!({error:?})").parse().unwrap(),
    }
}

type Result<T> = std::result::Result<T, String>;

fn variable(name: &str) -> Result<String> {
    env::var(name).map_err(|_| format!("missing compiler build context: {name}"))
}

fn digest_file(hash: &mut Sha256, path: &Path) -> Result<()> {
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    hash.update(
        file.metadata()
            .map_err(|e| e.to_string())?
            .len()
            .to_le_bytes(),
    );
    let mut buffer = [0; 16384];
    loop {
        let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            return Ok(());
        }
        hash.update(&buffer[..count]);
    }
}

fn source_files(directory: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_dir() {
            source_files(&entry.path(), output)?;
        } else if kind.is_file() {
            output.push(entry.path());
        } else {
            return Err("compiler source contains a non-regular entry".into());
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct Context {
    externs: Vec<String>,
    search: Vec<String>,
    configurations: Vec<String>,
    checks: Vec<String>,
    edition: String,
    panic: String,
    metadata_only: bool,
}

fn parse(arguments: &[String], cwd: &Path) -> Result<Context> {
    let mut context = Context {
        externs: Vec::new(),
        search: Vec::new(),
        configurations: Vec::new(),
        checks: Vec::new(),
        edition: "2021".into(),
        panic: "unwind".into(),
        metadata_only: false,
    };
    let mut index = 1;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument.starts_with('@') {
            return Err("compiler response files are not supported by the image bootstrap".into());
        }
        let (option, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(a, b)| (a, Some(b)));
        let paired = matches!(
            option,
            "--extern" | "-L" | "--cfg" | "--check-cfg" | "--edition" | "--emit" | "-C"
        );
        let value = if paired {
            if let Some(value) = inline {
                Some(value.to_owned())
            } else {
                index += 1;
                Some(
                    arguments
                        .get(index)
                        .ok_or("missing compiler option value")?
                        .clone(),
                )
            }
        } else if let Some(value) = argument.strip_prefix("-L") {
            Some(value.to_owned())
        } else if let Some(value) = argument.strip_prefix("-C") {
            Some(value.to_owned())
        } else {
            None
        };
        if let Some(value) = value {
            if option == "--extern" {
                let (name, path) = value
                    .split_once('=')
                    .ok_or("extern has no explicit artifact")?;
                let path = cwd
                    .join(path)
                    .canonicalize()
                    .map_err(|e| format!("extern {name}: {e}"))?;
                context.externs.push(format!(
                    "{name}={}",
                    path.to_str().ok_or("non-UTF8 extern path")?
                ));
            } else if option == "-L" || argument.starts_with("-L") {
                let (kind, path) = value
                    .split_once('=')
                    .map_or(("all", value.as_str()), |(a, b)| (a, b));
                let path = cwd.join(path);
                context.search.push(format!(
                    "{kind}={}",
                    path.to_str().ok_or("non-UTF8 search path")?
                ));
            } else if option == "--cfg" {
                if value != "feature=\"jit-worker-embedded\"" && value != "luna_jit_embedded_worker"
                {
                    if matches!(value.as_str(), "test" | "miri" | "doctest") {
                        return Err("native bootstrap cannot inherit harness configuration".into());
                    }
                    context.configurations.push(value);
                }
            } else if option == "--check-cfg" {
                context.checks.push(value);
            } else if option == "--edition" {
                context.edition = value;
            } else if option == "--emit" {
                context.metadata_only = value
                    .split(',')
                    .all(|part| matches!(part.split('=').next(), Some("dep-info" | "metadata")));
            } else if let Some(panic) = value.strip_prefix("panic=") {
                context.panic = panic.to_owned();
            }
        }
        index += 1;
    }
    Ok(context)
}

fn command(context: &Context, root: &Path, output: &Path, target: &str) -> Result<Command> {
    let mut command = Command::new(variable("LUNA_EMBED_RUSTC")?);
    command
        .current_dir(root)
        .arg(format!("--edition={}", context.edition))
        .args([
            "--target",
            target,
            "-C",
            "opt-level=s",
            "-C",
            "codegen-units=1",
            "-C",
            "embed-bitcode=no",
            "-C",
            "debuginfo=0",
            "-C",
            "strip=symbols",
            "-C",
            "target-feature=+crt-static",
        ])
        .arg("-C")
        .arg(format!("panic={}", context.panic))
        .arg(format!(
            "--remap-path-prefix={}=/luna-source",
            root.display()
        ))
        .arg(format!(
            "--remap-path-prefix={}=/luna-build",
            output.display()
        ))
        .env("LUNA_EMBED_INNER", "1");
    for external in &context.externs {
        command.arg("--extern").arg(external);
    }
    for search in &context.search {
        command.arg("-L").arg(search);
    }
    for configuration in &context.configurations {
        command.arg("--cfg").arg(configuration);
    }
    for check in &context.checks {
        command.arg("--check-cfg").arg(check);
    }
    let linker = variable("LUNA_EMBED_LINKER")?;
    if !linker.is_empty() {
        command.arg("-C").arg(format!("linker={linker}"));
    }
    if target.ends_with("-gnu") && variable("LUNA_EMBED_HOST")? == target {
        let archive = variable("LUNA_EMBED_GLIBC_STATIC")?;
        if !archive.is_empty() {
            if !Path::new(&archive).join("libc.a").is_file() {
                return Err("static GNU archive is missing".into());
            }
            command.arg("-L").arg(format!("native={archive}"));
        }
    }
    Ok(command)
}

fn execute(mut command: Command, log: &Path) -> Result<()> {
    let invocation = format!("{command:?}");
    fs::write(log.with_extension("command"), invocation).map_err(|e| e.to_string())?;
    command
        .stdout(Stdio::null())
        .stderr(File::create(log).map_err(|e| e.to_string())?);
    let status = command.status().map_err(|e| e.to_string())?;
    if !status.success() {
        let text = fs::read(log).unwrap_or_default();
        let diagnostic = String::from_utf8_lossy(&text[..text.len().min(16384)]);
        return Err(format!(
            "private compiler failed ({status}); {}:\n{diagnostic}",
            log.display()
        ));
    }
    Ok(())
}

fn wait_for_native(context: &Context) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
    loop {
        let mut pending = None;
        for external in &context.externs {
            let metadata = Path::new(external.split_once('=').unwrap().1);
            if metadata
                .extension()
                .is_some_and(|extension| extension == "rmeta")
            {
                let native = metadata.with_extension("rlib");
                let ready = native
                    .metadata()
                    .and_then(|file| file.modified())
                    .and_then(|native_time| {
                        metadata
                            .metadata()?
                            .modified()
                            .map(|metadata_time| native_time >= metadata_time)
                    })
                    .unwrap_or(false);
                if !ready {
                    pending = Some(metadata);
                    break;
                }
            }
        }
        let Some(pending) = pending else {
            return Ok(());
        };
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "Cargo-selected dependency has no completed native payload: {}",
                pending.display()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_checks_do_not_request_a_runtime_image() {
        let arguments = ["rustc", "--emit=dep-info,metadata"];
        let arguments = arguments.map(str::to_owned);
        assert!(parse(&arguments, Path::new("/")).unwrap().metadata_only);
        let arguments = ["rustc", "--emit=dep-info,metadata,link"];
        assert!(
            !parse(&arguments.map(str::to_owned), Path::new("/"))
                .unwrap()
                .metadata_only
        );
    }

    #[test]
    fn bootstrap_filters_only_embedding_and_preserves_source_features_and_panic() {
        let arguments = [
            "rustc",
            "--cfg",
            "feature=\"jit-worker-embedded\"",
            "--cfg=feature=\"jit-worker-bootstrap\"",
            "--cfg",
            "feature=\"async\"",
            "-Cpanic=abort",
        ];
        let context = parse(&arguments.map(str::to_owned), Path::new("/")).unwrap();
        assert_eq!(
            context.configurations,
            ["feature=\"jit-worker-bootstrap\"", "feature=\"async\""]
        );
        assert_eq!(context.panic, "abort");
        for cfg in ["test", "miri", "doctest"] {
            assert!(parse(
                &["rustc".into(), "--cfg".into(), cfg.into()],
                Path::new("/")
            )
            .is_err());
        }
    }

    #[test]
    fn ambiguous_context_and_response_files_are_not_guessed() {
        for arguments in [
            vec!["rustc", "--extern", "crate_without_artifact"],
            vec!["rustc", "@response"],
            vec!["rustc", "--cfg"],
        ] {
            assert!(parse(
                &arguments.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                Path::new("/")
            )
            .is_err());
        }
    }
}

fn build() -> Result<String> {
    if env::var_os("LUNA_EMBED_INNER").is_some() {
        return Err("recursive compiler-image bootstrap".into());
    }
    let arguments: Vec<String> = env::args_os()
        .map(|s| {
            s.into_string()
                .map_err(|_| "non-UTF8 compiler argument".to_owned())
        })
        .collect::<Result<_>>()?;
    if arguments.first().is_some_and(|s| {
        Path::new(s)
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("rustdoc"))
    }) {
        return Ok("None".into());
    }
    let cwd = env::current_dir().map_err(|e| e.to_string())?;
    let context = parse(&arguments, &cwd)?;
    if context.metadata_only {
        return Ok("None".into());
    }
    for name in [
        "ahash",
        "allocator_api2",
        "cranelift_codegen",
        "cranelift_frontend",
        "cranelift_jit",
        "cranelift_module",
        "cranelift_native",
        "libc",
        "sha2",
        "target_lexicon",
    ] {
        if !context
            .externs
            .iter()
            .any(|item| item.starts_with(&format!("{name}=")))
        {
            return Err(format!(
                "parent compiler did not select required extern {name}"
            ));
        }
    }
    let root = PathBuf::from(variable("CARGO_MANIFEST_DIR")?);
    let target = variable("LUNA_EMBED_TARGET")?;
    let mut fingerprint = Sha256::new();
    fingerprint.update(b"luna-worker-bootstrap-v1\0");
    for argument in &arguments {
        fingerprint.update((argument.len() as u64).to_le_bytes());
        fingerprint.update(argument);
    }
    let mut files = Vec::new();
    source_files(&root.join("src"), &mut files)?;
    files.sort();
    for path in &files {
        let relative = path
            .strip_prefix(&root)
            .unwrap()
            .to_str()
            .ok_or("non-UTF8 source path")?;
        fingerprint.update((relative.len() as u64).to_le_bytes());
        fingerprint.update(relative);
        digest_file(&mut fingerprint, path)?;
    }
    for external in &context.externs {
        digest_file(
            &mut fingerprint,
            Path::new(external.split_once('=').unwrap().1),
        )?;
    }
    let key = format!("{:x}", fingerprint.finalize());
    let output = PathBuf::from(variable("OUT_DIR")?).join(format!("worker-{key}"));
    fs::create_dir_all(&output).map_err(|e| e.to_string())?;
    let mut inputs = File::create(output.join("inputs.txt")).map_err(|e| e.to_string())?;
    writeln!(inputs, "target={target}\nkey={key}").map_err(|e| e.to_string())?;
    for external in &context.externs {
        writeln!(inputs, "extern={external}").map_err(|e| e.to_string())?;
    }
    let library = output.join("libluna_jit_compiler.rlib");
    let mut compile = command(&context, &root, &output, &target)?;
    compile
        .args(["--crate-name", "luna_jit_compiler", "--crate-type", "rlib"])
        .arg(root.join("src/lib.rs"))
        .arg("-o")
        .arg(&library);
    execute(compile, &output.join("library.log"))?;
    let waiting = std::time::Instant::now();
    wait_for_native(&context)?;
    writeln!(inputs, "native-wait-ms={}", waiting.elapsed().as_millis())
        .map_err(|e| e.to_string())?;
    let image = output.join("luna-jit-worker");
    let staged = output.join("luna-jit-worker.next");
    let mut link_context = context.clone();
    link_context.externs.clear();
    let mut link = command(&link_context, &root, &output, &target)?;
    link.args(["--crate-name", "luna_jit_worker", "--crate-type", "bin"])
        .arg(root.join("src/bin/jit_worker.rs"))
        .arg("--extern")
        .arg(format!("luna={}", library.display()))
        .arg("-L")
        .arg(format!("dependency={}", output.display()))
        .arg("-o")
        .arg(&staged);
    execute(link, &output.join("binary.log"))?;
    let length = staged.metadata().map_err(|e| e.to_string())?.len();
    if length == 0 || length > 512 << 20 {
        return Err("embedded producer exceeds its image byte cap".into());
    }
    let bytes = fs::read(&staged).map_err(|e| e.to_string())?;
    elf::validate(&bytes, &target)?;
    let unchanged = image
        .metadata()
        .is_ok_and(|metadata| metadata.len() == length)
        && fs::read(&image).is_ok_and(|previous| previous == bytes);
    if unchanged {
        fs::remove_file(&staged).map_err(|e| e.to_string())?;
    } else {
        fs::rename(&staged, &image).map_err(|e| e.to_string())?;
    }
    let mut identity = Sha256::new();
    identity.update(b"luna-compiler-image-v1\0");
    identity.update(&bytes);
    identity.update((bytes.len() as u64).to_le_bytes());
    let identity: [u8; 32] = identity.finalize().into();
    writeln!(inputs, "bytes={}\nidentity={identity:?}", bytes.len()).map_err(|e| e.to_string())?;
    Ok(format!(
        "Some((include_bytes!({:?}) as &'static [u8], {identity:?}, {target:?}))",
        image.to_str().ok_or("non-UTF8 image path")?
    ))
}
