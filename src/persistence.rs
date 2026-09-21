use fs2::FileExt;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct LockGuard {
    file: File,
}

impl LockGuard {
    pub(crate) fn acquire(path: &Path) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.create(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        let deadline = Instant::now() + LOCK_TIMEOUT;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self { file }),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "timed out after {}ms waiting for {}",
                            LOCK_TIMEOUT.as_millis(),
                            path.display()
                        ),
                    ));
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub(crate) fn acquire_for(target: &Path) -> io::Result<Self> {
        let parent = target.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        Self::acquire(&sibling_path(target, ".hooklinesinker.lock"))
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub(crate) fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic(path, bytes, true, ExpectedFile::Any)
}

#[cfg(test)]
fn write_preserving_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic(path, bytes, false, ExpectedFile::Any)
}

pub(crate) fn write_preserving_atomic_if_unchanged(
    path: &Path,
    expected: Option<&[u8]>,
    bytes: &[u8],
) -> io::Result<()> {
    let expected = expected.map_or(ExpectedFile::Missing, ExpectedFile::Bytes);
    write_atomic(path, bytes, false, expected)
}

#[derive(Clone, Copy)]
enum ExpectedFile<'a> {
    Any,
    Missing,
    Bytes(&'a [u8]),
}

fn write_atomic(
    path: &Path,
    bytes: &[u8],
    private: bool,
    expected: ExpectedFile<'_>,
) -> io::Result<()> {
    let target_exists = fs::symlink_metadata(path).is_ok();
    let (tmp_path, mut file) = create_unique_sibling(path, private || !target_exists)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        if !private {
            #[cfg(unix)]
            if let Ok(meta) = fs::metadata(path) {
                fs::set_permissions(&tmp_path, meta.permissions())?;
            }
        }
        drop(file);
        if !matches!(expected, ExpectedFile::Any) {
            let current = match fs::read(path) {
                Ok(bytes) => Some(bytes),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(e),
            };
            let matches_expected = match expected {
                ExpectedFile::Any => true,
                ExpectedFile::Missing => current.is_none(),
                ExpectedFile::Bytes(bytes) => current.as_deref() == Some(bytes),
            };
            if !matches_expected {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("{} changed during reconciliation", path.display()),
                ));
            }
        }
        fs::rename(&tmp_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn create_unique_sibling(path: &Path, private: bool) -> io::Result<(std::path::PathBuf, File)> {
    for _ in 0..100 {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let suffix = format!(".hooklinesinker-{}-{id}.tmp", std::process::id());
        let tmp_path = sibling_path(path, &suffix);
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&tmp_path) {
            Ok(file) => return Ok((tmp_path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("could not allocate a temporary file for {}", path.display()),
    ))
}

fn sibling_path(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(suffix);
    path.parent().unwrap_or_else(|| Path::new(".")).join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, mpsc};
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "hooklinesinker-persistence-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn target_locks_serialize_independent_callers() {
        let root = temp_root("locks");
        let target = root.join("settings.json");
        let first = LockGuard::acquire_for(&target).unwrap();
        let (attempted_tx, attempted_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let other_target = target;
        let handle = thread::spawn(move || {
            attempted_tx.send(()).unwrap();
            let _guard = LockGuard::acquire_for(&other_target).unwrap();
            acquired_tx.send(()).unwrap();
        });

        attempted_rx.recv().unwrap();
        assert!(
            acquired_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );
        drop(first);
        acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        handle.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parallel_atomic_writes_use_independent_temporary_files() {
        let root = temp_root("writes");
        fs::create_dir_all(&root).unwrap();
        let target = root.join("settings.json");
        let writers = 16;
        let barrier = Arc::new(Barrier::new(writers));
        let handles: Vec<_> = (0..writers)
            .map(|index| vec![b'a' + u8::try_from(index).unwrap(); 128 * 1024])
            .map(|payload| {
                let target = target.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    write_preserving_atomic(&target, &payload)
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        let final_bytes = fs::read(&target).unwrap();
        assert_eq!(final_bytes.len(), 128 * 1024);
        assert!(final_bytes.iter().all(|byte| *byte == final_bytes[0]));
        assert!((b'a'..=b'p').contains(&final_bytes[0]));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn conditional_atomic_write_rejects_a_changed_source() {
        let root = temp_root("conditional-write");
        fs::create_dir_all(&root).unwrap();
        let target = root.join("settings.json");
        fs::write(&target, b"changed by another process").unwrap();

        let error = write_preserving_atomic_if_unchanged(&target, Some(b"original"), b"our update")
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(fs::read(&target).unwrap(), b"changed by another process");
        fs::remove_dir_all(root).unwrap();
    }
}
