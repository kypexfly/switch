//! Claude Code guards its files with directories whose modification time shows the holder is alive.

use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};

const TOUCH_INTERVAL: Duration = Duration::from_secs(3);
const WAIT: Duration = Duration::from_secs(9);

pub struct DirLock {
    path: PathBuf,
    stop: Option<Sender<()>>,
    toucher: Option<JoinHandle<()>>,
}

impl DirLock {
    pub fn acquire(path: &Path, stale_after: Duration) -> Result<Self> {
        Self::acquire_within(path, stale_after, WAIT)
    }

    fn acquire_within(path: &Path, stale_after: Duration, wait: Duration) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let deadline = Instant::now() + wait;
        loop {
            match fs::create_dir(path) {
                Ok(()) => return Ok(Self::hold(path)),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error).with_context(|| format!("could not lock {}", path.display())),
            }
            if is_stale(path, stale_after) {
                take_over(path, stale_after);
            } else if Instant::now() > deadline {
                bail!("Claude Code is updating its login. Try again in a few seconds.");
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn hold(path: &Path) -> Self {
        let (stop, stopped) = mpsc::channel();
        let touched = path.to_owned();
        let toucher = thread::spawn(move || {
            while stopped.recv_timeout(TOUCH_INTERVAL) == Err(RecvTimeoutError::Timeout) {
                if let Ok(dir) = File::open(&touched) {
                    dir.set_modified(SystemTime::now()).ok();
                }
            }
        });
        Self { path: path.to_owned(), stop: Some(stop), toucher: Some(toucher) }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(toucher) = self.toucher.take() {
            toucher.join().ok();
        }
        fs::remove_dir(&self.path).ok();
    }
}

fn is_stale(path: &Path, stale_after: Duration) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > stale_after)
}

/// Two waiters must not both remove a dead lock: the second would remove the first one's new lock.
fn take_over(path: &Path, stale_after: Duration) {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".takeover");
    let takeover = path.with_file_name(name);
    if fs::create_dir(&takeover).is_err() {
        if is_stale(&takeover, Duration::from_secs(5)) {
            fs::remove_dir(&takeover).ok();
        }
        return;
    }
    if is_stale(path, stale_after) {
        fs::remove_dir(path).ok();
    }
    fs::remove_dir(&takeover).ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: Duration = Duration::from_millis(250);

    #[test]
    fn excludes_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let held = DirLock::acquire(&path, Duration::from_secs(60)).unwrap();
        assert!(DirLock::acquire_within(&path, Duration::from_secs(60), SHORT).is_err());
        drop(held);
        assert!(!path.exists());
        DirLock::acquire_within(&path, Duration::from_secs(60), SHORT).unwrap();
    }

    #[test]
    fn takes_over_a_dead_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        fs::create_dir(&path).unwrap();
        File::open(&path).unwrap().set_modified(SystemTime::now() - Duration::from_secs(120)).unwrap();
        DirLock::acquire_within(&path, Duration::from_secs(60), SHORT).unwrap();
    }
}
