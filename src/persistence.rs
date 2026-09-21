use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub struct LockGuard {
    file: File,
}

impl LockGuard {
    pub fn acquire(path: &Path) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.create(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        file.lock_exclusive()?;
        Ok(Self { file })
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp_path = path.with_extension("tmp");
    {
        let mut options = OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp_path, path)?;
    Ok(())
}
