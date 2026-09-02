//! One writer at a time (plan §5): an advisory `flock(LOCK_EX | LOCK_NB)` on
//! `<database_path>.lock`, taken in `main` for `generate`, `profile rebuild`,
//! `features backfill` and `backfill-social`. No table, no TTL: the kernel
//! releases the lock when the holder exits, however it exits.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

/// Held for as long as the value lives; dropping it (or dying) releases it.
#[derive(Debug)]
pub struct RunLock {
    _file: File,
    pub path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another process holds the lock; `holder` names its command when known.
    #[error("{holder} is already running")]
    Held { holder: String, path: PathBuf },
    #[error("lock file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// `<database_path>.lock`, next to the database.
pub fn lock_path(database_path: &Path) -> PathBuf {
    let mut name = database_path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    database_path.with_file_name(name)
}

/// Take the lock for `command`, writing the command's name into the file so a
/// second invocation can say who is holding it.
pub fn acquire(database_path: &Path, command: &str) -> Result<RunLock, LockError> {
    let path = lock_path(database_path);
    let io_err = |source: io::Error| LockError::Io {
        path: path.clone(),
        source,
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(io_err)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(io_err)?;
    // SAFETY: `file` owns a valid open descriptor for the duration of the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let source = io::Error::last_os_error();
        if source.kind() == io::ErrorKind::WouldBlock {
            let holder = std::fs::read_to_string(&path)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "generate".to_string());
            return Err(LockError::Held { holder, path });
        }
        return Err(io_err(source));
    }
    // Best effort: the name is a courtesy for the error message, never load-bearing.
    let _ = file.set_len(0);
    let _ = writeln!(file, "{command}");
    Ok(RunLock { _file: file, path })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const HELPER_ENV: &str = "DAILY_EPUB_LOCK_TEST_HOLDER";

    #[test]
    fn lock_path_sits_next_to_the_database() {
        assert_eq!(
            lock_path(Path::new("/var/lib/daily-epub/daily-epub.db")),
            PathBuf::from("/var/lib/daily-epub/daily-epub.db.lock")
        );
        assert_eq!(lock_path(Path::new("x.db")), PathBuf::from("x.db.lock"));
    }

    #[test]
    fn second_acquire_in_the_same_process_loses_and_names_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("nested").join("daily-epub.db");
        let first = acquire(&db, "generate").expect("first lock");
        assert!(first.path.exists(), "the lock file is created on demand");
        match acquire(&db, "profile rebuild") {
            Err(LockError::Held { holder, .. }) => {
                assert_eq!(holder, "generate");
                assert_eq!(
                    LockError::Held {
                        holder,
                        path: first.path.clone()
                    }
                    .to_string(),
                    "generate is already running"
                );
            }
            other => panic!("expected the lock to be held, got {other:?}"),
        }
        drop(first);
        let again = acquire(&db, "backfill-social").expect("released on drop");
        assert_eq!(
            std::fs::read_to_string(&again.path).unwrap().trim(),
            "backfill-social"
        );
    }

    /// Not a test of its own: when `HELPER_ENV` names a database path, this
    /// body takes the lock and holds it until it is killed. The parent below
    /// spawns the test binary with that variable set.
    #[test]
    fn lock_holder_helper() {
        let Ok(db) = std::env::var(HELPER_ENV) else {
            return;
        };
        let _held = acquire(Path::new(&db), "helper").expect("helper takes the lock");
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn a_killed_holder_frees_the_lock_for_the_next_process() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("daily-epub.db");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "lock::tests::lock_holder_helper", "--nocapture"])
            .env(HELPER_ENV, &db)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the holder process");

        // Wait (well under a second) until the child has written its name.
        let path = lock_path(&db);
        let deadline = Instant::now() + Duration::from_millis(900);
        while std::fs::read_to_string(&path)
            .map(|s| s.trim() != "helper")
            .unwrap_or(true)
        {
            assert!(Instant::now() < deadline, "the helper never took the lock");
            std::thread::sleep(Duration::from_millis(10));
        }
        match acquire(&db, "generate") {
            Err(LockError::Held { holder, .. }) => assert_eq!(holder, "helper"),
            other => panic!("another process holds the lock, got {other:?}"),
        }

        child.kill().expect("kill the holder");
        child.wait().expect("reap the holder");
        let lock = acquire(&db, "generate").expect("the kernel released the dead holder's lock");
        assert_eq!(
            std::fs::read_to_string(&lock.path).unwrap().trim(),
            "generate"
        );
    }
}
