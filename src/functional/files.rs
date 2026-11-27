//! Writing a configuration's files into a directory, as Home Manager's `home.file` does: a
//! manifest records what was written, so files that leave the configuration are removed and files
//! luna did not write are never overwritten.

use std::{
    collections::BTreeMap,
    fmt, fs, io,
    path::{Component, Path},
};

use super::{content_hash, Data};

/// The manifest's name inside the output directory.
pub const MANIFEST: &str = ".luna-files.json";
const FORMAT: &str = "luna-files-1";

/// What [`write_files`] changed, by relative path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Written {
    /// Created or rewritten.
    pub written: Vec<String>,
    /// Already up to date.
    pub unchanged: Vec<String>,
    /// Written before, no longer in the configuration, and removed.
    pub removed: Vec<String>,
    /// Written before and no longer in the configuration, but changed since, so left in place.
    pub kept: Vec<String>,
}

/// Why files could not be written. Nothing is written when the files or conflicts are refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// The value does not describe files: a path and what is wrong with it.
    Invalid { path: String, message: String },
    /// Existing files luna did not write, or that changed since it wrote them.
    Conflicts(Vec<String>),
    /// The filesystem refused an operation.
    Io { path: String, message: String },
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::Invalid { path, message } | WriteError::Io { path, message } => {
                write!(f, "{path}: {message}")
            }
            WriteError::Conflicts(conflicts) => {
                write!(f, "refusing to replace files luna did not write:")?;
                for conflict in conflicts {
                    write!(f, "\n  {conflict}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for WriteError {}

fn io_error(path: &Path, error: io::Error) -> WriteError {
    WriteError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    }
}

fn invalid(path: &str, message: impl Into<String>) -> WriteError {
    WriteError::Invalid {
        path: path.to_owned(),
        message: message.into(),
    }
}

struct File {
    text: Vec<u8>,
    executable: bool,
}

/// The files a value describes: a map from relative paths to contents, or to
/// `{ text = ..., executable = true }`.
fn files(value: &Data) -> Result<BTreeMap<String, File>, WriteError> {
    let Data::Map(entries) = value else {
        return Err(invalid(
            "files",
            "expected a map from paths to file contents",
        ));
    };
    let mut out = BTreeMap::new();
    for (key, spec) in entries {
        let Some(name) = key.as_str() else {
            return Err(invalid("files", "file paths must be UTF-8 strings"));
        };
        let relative = Path::new(name);
        if name.is_empty()
            || name == MANIFEST
            || !relative
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            return Err(invalid(
                name,
                "file paths are relative, without '.' or '..', and not the manifest",
            ));
        }
        let file = match spec {
            Data::String(text) => File {
                text: text.clone(),
                executable: false,
            },
            Data::Map(fields) => {
                let mut text = None;
                let mut executable = false;
                for (field, value) in fields {
                    match (field.as_str(), value) {
                        (Some("text"), Data::String(bytes)) => text = Some(bytes.clone()),
                        (Some("executable"), Data::Boolean(flag)) => executable = *flag,
                        _ => {
                            return Err(invalid(
                                name,
                                "a file is a string or { text = string, executable = boolean }",
                            ))
                        }
                    }
                }
                let Some(text) = text else {
                    return Err(invalid(name, "the file has no text"));
                };
                File { text, executable }
            }
            _ => {
                return Err(invalid(
                    name,
                    "a file is a string or { text = string, executable = boolean }",
                ))
            }
        };
        out.insert(name.to_owned(), file);
    }
    Ok(out)
}

/// The previous manifest: each path luna wrote with the hash of what it wrote.
fn manifest(out: &Path) -> Result<BTreeMap<String, String>, WriteError> {
    let path = out.join(MANIFEST);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(io_error(&path, error)),
    };
    let unreadable = || WriteError::Invalid {
        path: path.display().to_string(),
        message: "the manifest is not readable; remove it to start over".to_owned(),
    };
    let data = Data::from_json(&text).map_err(|_| unreadable())?;
    if data.get("format").and_then(Data::as_str) != Some(FORMAT) {
        return Err(unreadable());
    }
    let Some(Data::Map(entries)) = data.get("files") else {
        return Err(unreadable());
    };
    entries
        .iter()
        .map(|(name, hash)| Some((name.as_str()?.to_owned(), hash.as_str()?.to_owned())))
        .collect::<Option<_>>()
        .ok_or_else(unreadable)
}

fn current_hash(path: &Path) -> Result<Option<String>, WriteError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(path, error)),
        Ok(metadata) if metadata.is_dir() => Ok(Some(String::new())),
        Ok(_) => fs::read(path)
            .map(|text| Some(content_hash(&text)))
            .map_err(|error| io_error(path, error)),
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_: &Path) -> bool {
    false
}

#[cfg(unix)]
fn set_mode(path: &Path, executable: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
    )
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: bool) -> io::Result<()> {
    Ok(())
}

/// Write each file through a temporary sibling and a rename, so a reader never sees half a file.
fn write_atomically(path: &Path, file: &File) -> Result<(), WriteError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    let partial = path.with_file_name(format!(
        ".{}.luna-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let result = fs::write(&partial, &file.text)
        .and_then(|()| set_mode(&partial, file.executable))
        .and_then(|()| fs::rename(&partial, path));
    if let Err(error) = result {
        let _ = fs::remove_file(&partial);
        return Err(io_error(path, error));
    }
    Ok(())
}

/// Remove `path` and then each parent left empty, up to `out`.
fn remove(out: &Path, path: &Path) -> Result<(), WriteError> {
    fs::remove_file(path).map_err(|error| io_error(path, error))?;
    let mut parent = path.parent();
    while let Some(directory) = parent.filter(|directory| *directory != out) {
        if fs::remove_dir(directory).is_err() {
            break;
        }
        parent = directory.parent();
    }
    Ok(())
}

/// Write the files `value` describes into `out` and update its manifest.
///
/// `value` maps relative paths to contents, or to `{ text = ..., executable = true }`. A file that
/// already exists is replaced only if luna wrote it and it is unchanged since, or if it already
/// has the right contents; anything else is a conflict, and with conflicts nothing is written
/// unless `force` is set. Files the previous manifest lists that are no longer described are
/// removed if unchanged, along with directories they leave empty.
pub fn write_files(value: &Data, out: &Path, force: bool) -> Result<Written, WriteError> {
    let files = files(value)?;
    fs::create_dir_all(out).map_err(|error| io_error(out, error))?;
    let previous = manifest(out)?;

    let mut conflicts = Vec::new();
    let mut plan = Vec::new();
    for (name, file) in &files {
        let path = out.join(name);
        let wanted = content_hash(&file.text);
        let current = current_hash(&path)?;
        let up_to_date =
            current.as_deref() == Some(wanted.as_str()) && is_executable(&path) == file.executable;
        let ours = current.is_none() || current == previous.get(name).cloned();
        if current.as_deref() == Some("") {
            conflicts.push(format!("{name} (a directory)"));
        } else if !up_to_date && !ours && !force {
            conflicts.push(match previous.get(name) {
                Some(_) => format!("{name} (changed since luna wrote it)"),
                None => format!("{name} (not written by luna)"),
            });
        }
        plan.push((name, file, path, up_to_date));
    }
    if !conflicts.is_empty() {
        return Err(WriteError::Conflicts(conflicts));
    }

    let mut written = Written::default();
    let mut entries = Vec::new();
    for (name, file, path, up_to_date) in plan {
        if up_to_date {
            written.unchanged.push(name.clone());
        } else {
            write_atomically(&path, file)?;
            written.written.push(name.clone());
        }
        entries.push((
            Data::from(name.as_str()),
            Data::from(content_hash(&file.text)),
        ));
    }
    for (name, hash) in &previous {
        if files.contains_key(name) {
            continue;
        }
        let path = out.join(name);
        match current_hash(&path)? {
            Some(current) if current == *hash => {
                remove(out, &path)?;
                written.removed.push(name.clone());
            }
            Some(_) => written.kept.push(name.clone()),
            None => {}
        }
    }

    let manifest = Data::map(vec![
        (
            Data::from("files"),
            Data::map(entries).expect("paths are unique"),
        ),
        (Data::from("format"), Data::from(FORMAT)),
    ])
    .and_then(|manifest| manifest.to_json(true))
    .expect("the manifest is JSON");
    write_atomically(
        &out.join(MANIFEST),
        &File {
            text: format!("{manifest}\n").into_bytes(),
            executable: false,
        },
    )?;
    Ok(written)
}
