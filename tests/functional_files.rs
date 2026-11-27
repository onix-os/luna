//! `functional::write_files`: files a configuration describes, written into a directory with a
//! manifest, so only files luna wrote are ever replaced or removed.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use luna::functional::{write_files, Data, WriteError, Written, MANIFEST};

struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "luna-functional-files-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        Dir(path)
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.0.join(name)).unwrap()
    }

    fn write(&self, files: &str, force: bool) -> Result<Written, WriteError> {
        write_files(&Data::from_json(files).unwrap(), &self.0, force)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn files_are_written_updated_and_removed_through_the_manifest() {
    let dir = Dir::new();
    let first = dir
        .write(r#"{"a.txt": "a", "deep/b.txt": "b", "c.txt": "c"}"#, false)
        .unwrap();
    assert_eq!(first.written, ["a.txt", "c.txt", "deep/b.txt"]);
    assert!(dir.read(MANIFEST).contains(r#""format": "luna-files-1""#));

    fs::write(dir.0.join("c.txt"), "edited").unwrap();
    let second = dir.write(r#"{"a.txt": "A"}"#, false).unwrap();
    assert_eq!(
        second,
        Written {
            written: vec!["a.txt".into()],
            unchanged: vec![],
            removed: vec!["deep/b.txt".into()],
            kept: vec!["c.txt".into()],
        }
    );
    assert_eq!(dir.read("a.txt"), "A");
    assert_eq!(dir.read("c.txt"), "edited");
    assert!(!dir.0.join("deep").exists());

    let third = dir.write(r#"{"a.txt": "A"}"#, false).unwrap();
    assert_eq!(third.unchanged, ["a.txt"]);
    assert!(third.written.is_empty() && third.removed.is_empty());
}

#[test]
fn files_luna_did_not_write_are_conflicts_and_nothing_is_written() {
    let dir = Dir::new();
    fs::create_dir_all(dir.0.join("sub")).unwrap();
    fs::write(dir.0.join("theirs"), "theirs").unwrap();
    fs::write(dir.0.join("same"), "same").unwrap();
    let error = dir
        .write(
            r#"{"theirs": "mine", "same": "same", "sub": "x", "new": "n"}"#,
            false,
        )
        .unwrap_err();
    assert_eq!(
        error,
        WriteError::Conflicts(vec![
            "sub (a directory)".into(),
            "theirs (not written by luna)".into()
        ])
    );
    assert!(
        !dir.0.join("new").exists(),
        "a refused write writes nothing"
    );
    assert!(dir
        .write(r#"{"theirs": "mine", "sub": "x"}"#, true)
        .is_err());
    let forced = dir
        .write(r#"{"theirs": "mine", "same": "same"}"#, true)
        .unwrap();
    assert_eq!(
        (forced.written, forced.unchanged),
        (vec!["theirs".into()], vec!["same".into()])
    );
    assert_eq!(dir.read("theirs"), "mine");
}

#[cfg(unix)]
#[test]
fn the_executable_bit_is_part_of_a_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Dir::new();
    let mode = |name: &str| fs::metadata(dir.0.join(name)).unwrap().permissions().mode() & 0o777;
    dir.write(r#"{"run": {"text": "x", "executable": true}}"#, false)
        .unwrap();
    assert_eq!(mode("run"), 0o755);
    let plain = dir.write(r#"{"run": "x"}"#, false).unwrap();
    assert_eq!(plain.written, ["run"]);
    assert_eq!(mode("run"), 0o644);
}

#[test]
fn paths_and_specs_are_checked_before_anything_is_written() {
    let dir = Dir::new();
    for (files, expected) in [
        (r#"{"../x": "a"}"#, "../x: file paths are relative"),
        (r#"{"/etc/x": "a"}"#, "/etc/x: file paths are relative"),
        (r#"{"./x": "a"}"#, "./x: file paths are relative"),
        (
            r#"{".luna-files.json": "a"}"#,
            ".luna-files.json: file paths are relative",
        ),
        (r#"{"x": 1}"#, "x: a file is a string or"),
        (
            r#"{"x": {"text": "a", "mode": 1}}"#,
            "x: a file is a string or",
        ),
        (r#"{"x": {"executable": true}}"#, "x: the file has no text"),
        (
            r#"["x"]"#,
            "files: expected a map from paths to file contents",
        ),
    ] {
        let error = dir.write(files, false).unwrap_err().to_string();
        assert!(error.starts_with(expected), "{files}: {error}");
    }
    fs::create_dir_all(&dir.0).unwrap();
    fs::write(dir.0.join(MANIFEST), "{}").unwrap();
    let error = dir.write(r#"{"x": "a"}"#, false).unwrap_err().to_string();
    assert!(
        error.ends_with("the manifest is not readable; remove it to start over"),
        "{error}"
    );
}
