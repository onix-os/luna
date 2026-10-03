use std::{
    fs::{File, OpenOptions},
    io,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

const ATTEMPTS: usize = 128;
static NEXT: AtomicUsize = AtomicUsize::new(0);

pub(super) fn create() -> io::Result<(PathBuf, File)> {
    reserve(|| {
        let id = NEXT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| io::Error::other("temporary-file sequence exhausted"))?;
        Ok(std::env::temp_dir().join(format!("luna_{:x}_{id:x}", std::process::id())))
    })
}

fn reserve(mut next: impl FnMut() -> io::Result<PathBuf>) -> io::Result<(PathBuf, File)> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    for _ in 0..ATTEMPTS {
        let path = next()?;
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "temporary-file collision limit reached",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct OwnedPath(PathBuf);

    impl Drop for OwnedPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn retries_collisions_without_changing_existing_files() {
        let (path, file) = create().unwrap();
        let existing = OwnedPath(path);
        drop(file);
        std::fs::write(&existing.0, b"preserved").unwrap();
        let (path, file) = create().unwrap();
        let directory = OwnedPath(path);
        drop(file);
        std::fs::remove_file(&directory.0).unwrap();
        std::fs::create_dir(&directory.0).unwrap();
        let fresh = OwnedPath(directory.0.join("fresh"));
        let mut calls = 0;
        let (path, file) = reserve(|| {
            calls += 1;
            Ok(if calls == 1 { &existing.0 } else { &fresh.0 }.clone())
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(path, fresh.0);
        assert_eq!(file.metadata().unwrap().len(), 0);
        assert_eq!(std::fs::read(&existing.0).unwrap(), b"preserved");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o077, 0);
        }
    }

    #[test]
    fn collision_retries_are_bounded() {
        let (path, _file) = create().unwrap();
        let existing = OwnedPath(path);
        let mut calls = 0;
        let error = reserve(|| {
            calls += 1;
            Ok(existing.0.clone())
        })
        .unwrap_err();
        assert_eq!(calls, ATTEMPTS);
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn other_creation_errors_return_without_retry() {
        let (path, _file) = create().unwrap();
        let existing = OwnedPath(path);
        let mut calls = 0;
        assert!(reserve(|| {
            calls += 1;
            Ok(existing.0.join("not-a-directory"))
        })
        .is_err());
        assert_eq!(calls, 1);
    }
}
