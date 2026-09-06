//! The pid file guard.
//!
//! autossh removes its pid file from an `atexit()` handler, which it then has to
//! work around in `xerrlog()` because `_exit()` skips those. rash uses `Drop`,
//! which needs no special case — but does need to be sure it is removing its own
//! file and not somebody else's.

use rash::pidfile::PidFile;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "rash-pidfile-{}-{tag}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&d).expect("create scratch dir");
        Self(d)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn it_writes_our_pid_and_removes_the_file_on_drop() {
    let s = Scratch::new("basic");
    let p = s.path("rash.pid");

    let guard = PidFile::create(&p).expect("create");
    let written: u32 = fs::read_to_string(&p)
        .expect("read")
        .trim()
        .parse()
        .expect("a number");
    assert_eq!(written, std::process::id());

    drop(guard);
    assert!(!p.exists(), "the guard should remove its own file");
}

#[test]
fn touch_moves_the_modification_time_without_disturbing_the_contents() {
    let s = Scratch::new("touch");
    let p = s.path("rash.pid");
    let guard = PidFile::create(&p).expect("create");

    let before = fs::metadata(&p).expect("stat").modified().expect("mtime");
    std::thread::sleep(std::time::Duration::from_millis(20));
    guard.touch().expect("touch");

    let after = fs::metadata(&p).expect("stat").modified().expect("mtime");
    assert!(after > before, "touch should move the mtime forward");
    assert_eq!(
        fs::read_to_string(&p).expect("read").trim(),
        std::process::id().to_string(),
        "touch must not truncate the pid"
    );
}

#[test]
fn a_file_claimed_by_another_process_is_left_alone() {
    // Nothing stops a second rash being pointed at the same path: it truncates
    // the file and writes its own pid. Unconditional removal on drop would then
    // have the first one to exit delete the *survivor's* pid file, leaving a
    // live supervisor that no watchdog could find.
    let s = Scratch::new("stolen");
    let p = s.path("rash.pid");

    let guard = PidFile::create(&p).expect("create");
    fs::write(&p, "999999\n").expect("simulate a second rash taking it over");

    drop(guard);
    assert!(p.exists(), "somebody else's pid file must survive our drop");
    assert_eq!(fs::read_to_string(&p).expect("read").trim(), "999999");
}

#[test]
fn a_file_removed_underneath_us_is_not_an_error_on_drop() {
    let s = Scratch::new("vanished");
    let p = s.path("rash.pid");

    let guard = PidFile::create(&p).expect("create");
    fs::remove_file(&p).expect("remove");
    drop(guard); // must not panic
}
