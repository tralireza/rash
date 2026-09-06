//! End-to-end: the real rash binary supervising the fake-ssh stand-in.
//!
//! These are the tests autossh never had. Everything runs locally in a scratch
//! directory, with no network, no keys, and no remote host.

use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::time::{Duration, Instant};

const RASH: &str = env!("CARGO_BIN_EXE_rash");
const FAKE_SSH: &str = env!("CARGO_BIN_EXE_fake-ssh");

/// Generous enough for a loaded CI box, short enough that a hang is still a
/// test failure rather than a coffee break.
const LIMIT: Duration = Duration::from_secs(20);

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rash-test-{}-{tag}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create scratch dir");
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A rash invocation wired up to the fake ssh.
struct Rash {
    cmd: Command,
    state: PathBuf,
    log: PathBuf,
}

impl Rash {
    fn new(s: &Scratch) -> Self {
        let state = s.path("starts");
        let log = s.path("rash.log");
        let mut cmd = Command::new(RASH);
        // env_clear so an AUTOSSH_* variable in the developer's shell cannot
        // quietly change what is being tested.
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("RASH_SSH_PATH", FAKE_SSH)
            .env("FAKE_SSH_STATE", &state)
            .env("AUTOSSH_LOGFILE", &log)
            .env("AUTOSSH_LOGLEVEL", "7")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Self { cmd, state, log }
    }

    fn env(mut self, k: &str, v: impl AsRef<OsStr>) -> Self {
        self.cmd.env(k, v);
        self
    }

    fn args(mut self, a: &[&str]) -> Self {
        self.cmd.args(a);
        self
    }

    fn spawn(mut self) -> Running {
        Running {
            child: self.cmd.spawn().expect("spawn rash"),
            state: self.state,
            log: self.log,
        }
    }
}

struct Running {
    child: Child,
    state: PathBuf,
    log: PathBuf,
}

impl Running {
    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    fn signal(&self, sig: i32) {
        // SAFETY: the pid belongs to a child of this process that has not been
        // reaped, so it cannot have been reused.
        unsafe { libc::kill(self.pid(), sig) };
    }

    /// How many times the fake ssh has been started.
    fn starts(&self) -> usize {
        fs::read_to_string(&self.state)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn wait_until(&self, what: &str, mut cond: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + LIMIT;
        while !cond(self) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}\nlog:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + LIMIT;
        loop {
            match self.child.try_wait().expect("try_wait") {
                Some(s) => return s,
                None if Instant::now() >= deadline => {
                    let log = self.log();
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    panic!("rash did not exit within {LIMIT:?}\nlog:\n{log}");
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    /// The ssh pid rash logged, so a test can check it really was cleaned up.
    fn ssh_pid(&self) -> Option<i32> {
        self.log()
            .lines()
            .filter_map(|l| l.rsplit_once("ssh child pid is "))
            .filter_map(|(_, pid)| pid.trim().parse().ok())
            .next_back()
    }
}

impl Drop for Running {
    /// A test must leave neither a supervisor nor an ssh behind.
    ///
    /// SIGTERM first, and only then SIGKILL. Going straight to `Child::kill`,
    /// which is SIGKILL, gives rash no chance to reap its own child, so the
    /// fake ssh is reparented to init and sleeps for ever holding its ports.
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(None)) {
            return;
        }

        self.signal(libc::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs error checking only and delivers nothing.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// An even port whose successor is also free: the fake ssh takes `p` and rash's
/// monitor takes `p + 1`.
///
/// These deliberately come from a fixed range *below* the ephemeral range rather
/// than from `bind(0)`. Two reasons, both of which bite on Linux and not on a
/// quiet macOS box:
///
/// * Linux allocates outbound source ports from 32768-60999, so a port obtained
///   by binding 0 and releasing it can be taken by an unrelated connection in
///   the gap before the child process binds it — and these tests open a lot of
///   local connections.
/// * Two of the fourteen tests here run in parallel and can be handed the same
///   port by successive `bind(0)` calls. The counter makes each caller distinct.
///
/// Even-only, so one test's forward can never land on another's monitor port.
fn free_even_port() -> u16 {
    static NEXT: AtomicU16 = AtomicU16::new(0);
    for _ in 0..2000 {
        let p = 21000 + (NEXT.fetch_add(2, Ordering::Relaxed) % 4000);
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok()
            && std::net::TcpListener::bind(("127.0.0.1", p + 1)).is_ok()
        {
            return p;
        }
    }
    panic!("could not find a free port pair below the ephemeral range");
}

#[test]
fn the_json_log_sink_emits_one_object_per_line() {
    let s = Scratch::new("json");
    let mut r = Rash::new(&s)
        .env("RASH_LOG_FORMAT", "json")
        .env("FAKE_SSH_MODE", "exit")
        .env("FAKE_SSH_EXIT_CODES", "0")
        .env("AUTOSSH_GATETIME", "0")
        .args(&["-M", "0", "-N", "host"])
        .spawn();

    assert_eq!(r.wait_for_exit().code(), Some(0));

    let log = r.log();
    assert!(!log.trim().is_empty(), "nothing was logged");
    for l in log.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value =
            serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON: {l}\n{e}"));
        for key in ["ts", "level", "pid", "msg"] {
            assert!(v.get(key).is_some(), "missing {key:?} in {l}");
        }
        assert_eq!(v["pid"], serde_json::json!(r.pid()));
    }
    assert!(log.contains("starting ssh"), "log:\n{log}");
}

#[test]
fn a_healthy_tunnel_is_left_alone() {
    let port = free_even_port();
    let s = Scratch::new("healthy");
    let mut r = Rash::new(&s)
        // A 1s poll gives a 500ms net timeout, so several probes run quickly.
        .env("AUTOSSH_POLL", "1")
        .args(&["-M", &port.to_string(), "-N", "host"])
        .spawn();

    r.wait_until("the monitor to report a good probe", |r| {
        r.log().contains("connection ok")
    });
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(r.starts(), 1, "a working tunnel must not be restarted");

    r.signal(libc::SIGTERM);
    r.wait_for_exit();
}

#[test]
fn a_wedged_tunnel_triggers_a_restart() {
    // The case the whole monitor exists for: ssh is alive and the port still
    // accepts connections, but nothing travels through it. Nothing about the
    // child process looks wrong, so only an end-to-end probe can catch it.
    let port = free_even_port();
    let s = Scratch::new("blackhole");
    let mut r = Rash::new(&s)
        .env("FAKE_SSH_TUNNEL", "blackhole")
        .env("AUTOSSH_POLL", "1")
        .env("AUTOSSH_GATETIME", "0")
        .args(&["-M", &port.to_string(), "-N", "host"])
        .spawn();

    r.wait_until("ssh to be restarted", |r| r.starts() >= 2);
    assert!(
        r.log().contains("port down, restarting ssh"),
        "log:\n{}",
        r.log()
    );

    r.signal(libc::SIGTERM);
    r.wait_for_exit();
}

#[test]
fn the_monitor_forwards_reach_ssh() {
    let port = free_even_port();
    let s = Scratch::new("forwards");
    let mut r = Rash::new(&s)
        .env("FAKE_SSH_MODE", "exit")
        .env("FAKE_SSH_EXIT_CODES", "0")
        .env("AUTOSSH_GATETIME", "0")
        .args(&["-M", &port.to_string(), "-N", "host"])
        .spawn();

    r.wait_for_exit();
    let recorded = fs::read_to_string(&r.state).expect("state file");
    assert_eq!(
        recorded.trim(),
        format!(
            "-L {port}:127.0.0.1:{port} -R {port}:127.0.0.1:{} -N host",
            port + 1
        )
    );
}

#[test]
fn a_clean_exit_stops_rash() {
    let s = Scratch::new("clean");
    let mut r = Rash::new(&s)
        .env("FAKE_SSH_MODE", "exit")
        .env("FAKE_SSH_EXIT_CODES", "0")
        .env("AUTOSSH_GATETIME", "0")
        .args(&["-M", "0", "-N", "host"])
        .spawn();

    assert_eq!(r.wait_for_exit().code(), Some(0));
    assert_eq!(r.starts(), 1, "a clean exit should not be retried");
}

#[test]
fn a_premature_first_exit_stops_rash() {
    // The starting gate: ssh died at once on the very first try, so something is
    // wrong with the connection or the credentials and retrying would only spin.
    let s = Scratch::new("gate");
    let mut r = Rash::new(&s)
        .env("FAKE_SSH_MODE", "exit")
        .env("FAKE_SSH_EXIT_CODES", "1")
        .args(&["-M", "0", "-N", "host"])
        .spawn();

    assert_eq!(r.wait_for_exit().code(), Some(1));
    assert_eq!(r.starts(), 1);
    assert!(r.log().contains("exited prematurely"), "log:\n{}", r.log());
}

#[test]
fn a_dropped_connection_is_retried_until_it_succeeds() {
    let s = Scratch::new("retry");
    let mut r = Rash::new(&s)
        .env("FAKE_SSH_MODE", "exit")
        .env("FAKE_SSH_EXIT_CODES", "255,255,0")
        .env("AUTOSSH_GATETIME", "0")
        .args(&["-M", "0", "-N", "host"])
        .spawn();

    assert_eq!(r.wait_for_exit().code(), Some(0));
    assert_eq!(r.starts(), 3, "two failures then a clean exit");
}

#[test]
fn max_start_is_honoured() {
    let s = Scratch::new("maxstart");
    let mut r = Rash::new(&s)
        .env("FAKE_SSH_MODE", "exit")
        .env("FAKE_SSH_EXIT_CODES", "255")
        .env("AUTOSSH_GATETIME", "0")
        .env("AUTOSSH_MAXSTART", "3")
        .args(&["-M", "0", "-N", "host"])
        .spawn();

    assert_eq!(r.wait_for_exit().code(), Some(0));
    assert_eq!(r.starts(), 3);
    assert!(
        r.log().contains("max start count reached"),
        "log:\n{}",
        r.log()
    );
}

#[test]
fn sigterm_stops_rash_and_reaps_ssh() {
    let s = Scratch::new("sigterm");
    let mut r = Rash::new(&s).args(&["-M", "0", "-N", "host"]).spawn();

    r.wait_until("ssh to start", |r| r.starts() == 1);
    let ssh = r.ssh_pid().expect("rash should log the ssh pid");

    r.signal(libc::SIGTERM);
    assert_eq!(r.wait_for_exit().code(), Some(1));
    assert!(
        r.log().contains("received signal to exit"),
        "log:\n{}",
        r.log()
    );

    // Give the kernel a moment to finish tearing the child down.
    std::thread::sleep(Duration::from_millis(200));
    assert!(!alive(ssh), "ssh {ssh} outlived rash");
}

#[test]
fn sigusr1_restarts_ssh_without_stopping_rash() {
    let s = Scratch::new("usr1");
    let mut r = Rash::new(&s).args(&["-M", "0", "-N", "host"]).spawn();

    r.wait_until("the first ssh", |r| r.starts() == 1);
    r.signal(libc::SIGUSR1);
    r.wait_until("the second ssh", |r| r.starts() == 2);
    assert!(
        r.log().contains("signalled to kill and restart ssh"),
        "log:\n{}",
        r.log()
    );

    r.signal(libc::SIGTERM);
    r.wait_for_exit();
}

#[test]
fn d11_a_child_that_ignores_sigterm_is_killed() {
    // autossh blocks in waitpid() here for ever; its own source comment
    // questions the design. rash escalates to SIGKILL after RASH_KILL_TIMEOUT.
    let s = Scratch::new("hang");
    let mut r = Rash::new(&s)
        .env("FAKE_SSH_MODE", "hang")
        .env("RASH_KILL_TIMEOUT", "1")
        .args(&["-M", "0", "-N", "host"])
        .spawn();

    r.wait_until("ssh to start", |r| r.starts() == 1);
    let ssh = r.ssh_pid().expect("rash should log the ssh pid");

    r.signal(libc::SIGTERM);
    assert_eq!(r.wait_for_exit().code(), Some(1));
    assert!(r.log().contains("ignored SIGTERM"), "log:\n{}", r.log());

    std::thread::sleep(Duration::from_millis(200));
    assert!(!alive(ssh), "the wedged ssh {ssh} survived");
}

#[test]
fn max_lifetime_shuts_rash_down() {
    let s = Scratch::new("lifetime");
    let mut r = Rash::new(&s)
        .env("AUTOSSH_MAXLIFETIME", "2")
        .args(&["-M", "0", "-N", "host"])
        .spawn();

    let started = Instant::now();
    assert_eq!(r.wait_for_exit().code(), Some(0));
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "exited before the lifetime was up"
    );
    assert!(
        r.log().contains("exceeded maximum time to live"),
        "log:\n{}",
        r.log()
    );
}

#[test]
fn the_pid_file_is_written_and_cleaned_up() {
    let s = Scratch::new("pidfile");
    let pid_file = s.path("rash.pid");
    let mut r = Rash::new(&s)
        .env("AUTOSSH_PIDFILE", &pid_file)
        .args(&["-M", "0", "-N", "host"])
        .spawn();

    r.wait_until("the pid file", |_| pid_file.exists());
    let written: i32 = fs::read_to_string(&pid_file)
        .expect("read pid file")
        .trim()
        .parse()
        .expect("pid file should hold a number");
    assert_eq!(written, r.pid(), "the pid file should hold rash's own pid");

    r.signal(libc::SIGTERM);
    r.wait_for_exit();
    assert!(!pid_file.exists(), "the pid file should be removed on exit");
}

#[test]
fn the_ssh_command_line_is_what_dry_run_promised() {
    let s = Scratch::new("argv");
    let mut r = Rash::new(&s)
        .env("FAKE_SSH_MODE", "exit")
        .env("FAKE_SSH_EXIT_CODES", "0")
        .env("AUTOSSH_GATETIME", "0")
        .args(&["-M", "0", "-N", "-p", "2222", "me@host"])
        .spawn();

    r.wait_for_exit();
    let recorded = fs::read_to_string(&r.state).expect("state file");
    assert_eq!(recorded.trim(), "-N -p 2222 me@host");
}

#[test]
fn background_daemonises_and_keeps_dash_f_from_ssh() {
    let s = Scratch::new("background");
    let pid_file = s.path("rash.pid");
    let mut r = Rash::new(&s)
        .env("AUTOSSH_PIDFILE", &pid_file)
        // A backstop, so a failure here cannot leave a daemon running for ever.
        .env("AUTOSSH_MAXLIFETIME", "30")
        .args(&["-M", "0", "-fN", "-p", "2222", "me@host"])
        .spawn();

    // What we spawned is only the pre-fork parent, which leaves immediately.
    assert_eq!(r.wait_for_exit().code(), Some(0));

    r.wait_until("the daemon's pid file", |_| pid_file.exists());
    let daemon: i32 = fs::read_to_string(&pid_file)
        .expect("read pid file")
        .trim()
        .parse()
        .expect("pid file should hold a number");
    assert_ne!(
        daemon,
        r.pid(),
        "the pid file must record the daemon's pid, not the pre-fork one"
    );

    r.wait_until("ssh to start", |r| r.starts() == 1);
    let recorded = fs::read_to_string(&r.state).expect("state file");
    assert_eq!(
        recorded.trim(),
        "-N -p 2222 me@host",
        "-f is rash's own and must not reach ssh"
    );

    // SAFETY: signalling a process we started and have not yet seen exit.
    unsafe { libc::kill(daemon, libc::SIGTERM) };
    r.wait_until("the daemon to exit", |_| !pid_file.exists());
}
