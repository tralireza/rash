//! The pid file, removed when the guard is dropped.
//!
//! autossh registers an `atexit()` handler for this (autossh.c:1721-1727), which
//! it then has to work around in `xerrlog()` because `_exit()` skips those on
//! most systems. A `Drop` guard needs no such special case.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug)]
pub struct PidFile {
    path: PathBuf,
}

impl PidFile {
    /// Write the current pid. Call this *after* any daemonising fork, so the
    /// file records the daemon's pid rather than that of the process that
    /// forked it.
    pub fn create(path: &Path) -> io::Result<Self> {
        let mut f = File::create(path)?;
        writeln!(f, "{}", std::process::id())?;
        f.flush()?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    /// Bump the modification time so a watchdog can see rash is still alive.
    ///
    /// autossh only does this when built with `-DTOUCH_PIDFILE`, which is off in
    /// the stock build; rash exposes it as `RASH_TOUCH_PIDFILE`.
    pub fn touch(&self) -> io::Result<()> {
        OpenOptions::new()
            .write(true)
            .open(&self.path)?
            .set_modified(SystemTime::now())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
