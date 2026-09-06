//! The connection monitor, exercised against in-process stand-ins for ssh.

use rash::cli;
use rash::config::{self, Config};
use rash::monitor::{Monitor, probe};
use std::ffi::OsString;
use std::net::TcpListener as StdTcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};

fn config_with(spec: &str, env: &[(&str, &str)]) -> Config {
    let inv = cli::parse(["-M", spec, "-N", "host"].iter().map(OsString::from)).expect("parse");
    config::resolve(inv, env).expect("resolve").config
}

/// A poll of 1s gives a 500ms net timeout, which keeps the failure paths quick.
fn config_for(spec: &str, poll: &str) -> Config {
    config_with(spec, &[("AUTOSSH_POLL", poll)])
}

/// Bind a stand-in server and return it along with its port, which is always
/// even and whose successor is free for the monitor to take.
///
/// The port comes from a fixed range below the ephemeral range rather than from
/// `bind(0)`. With `bind(0)` the pair lands in 32768+ on Linux (49152+ on
/// macOS), which is also where the kernel draws outbound source ports from — and
/// these tests open plenty of local connections, so `port + 1` can be taken in
/// the gap between checking it and `Monitor::bind` claiming it.
///
/// Even-only, and a range disjoint from the one `e2e.rs` uses, so no two
/// stand-ins can ever collide.
async fn stand_in() -> (TcpListener, u16) {
    static NEXT: AtomicU16 = AtomicU16::new(0);
    for _ in 0..2000 {
        let p = 25000 + (NEXT.fetch_add(2, Ordering::Relaxed) % 4000);
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", p)).await
            && StdTcpListener::bind(("127.0.0.1", p + 1)).is_ok()
        {
            return (listener, p);
        }
    }
    panic!("could not find a free port pair below the ephemeral range");
}

/// Stand in for the ssh forward loop: accept on the write port and hand the
/// bytes to the port rash is listening on.
fn spawn_loop_tunnel(listener: TcpListener, back_to: u16) {
    tokio::spawn(async move {
        while let Ok((mut inbound, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(mut outbound) = TcpStream::connect(("127.0.0.1", back_to)).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
            });
        }
    });
}

#[tokio::test]
async fn a_loop_probe_succeeds_when_the_data_comes_back() {
    let (tunnel, port) = stand_in().await;
    let cfg = config_for(&port.to_string(), "60");
    let monitor = Monitor::bind(&cfg).await.expect("bind the monitor");
    spawn_loop_tunnel(tunnel, port + 1);

    assert!(monitor.probe(&cfg).await);
}

#[tokio::test]
async fn a_loop_probe_fails_when_traffic_is_black_holed() {
    // The case the monitor exists for: the connection still accepts, so nothing
    // looks broken, but no byte ever comes back.
    let (tunnel, port) = stand_in().await;
    let cfg = config_for(&port.to_string(), "1");
    let monitor = Monitor::bind(&cfg).await.expect("bind the monitor");

    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = tunnel.accept().await {
            held.push(s); // accepted and then ignored
        }
    });

    let started = Instant::now();
    assert!(!monitor.probe(&cfg).await);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the probe must give up rather than wait for ever"
    );
}

#[tokio::test]
async fn a_probe_fails_when_nothing_is_listening() {
    let (tunnel, port) = stand_in().await;
    // Nothing listens on the write port once the stand-in is gone, so every
    // attempt should be refused outright rather than time out.
    drop(tunnel);
    let cfg = config_for(&port.to_string(), "1");
    let monitor = Monitor::bind(&cfg).await.expect("bind the monitor");
    assert!(!monitor.probe(&cfg).await);
}

#[tokio::test]
async fn an_echo_probe_succeeds() {
    let (echo, port) = stand_in().await;
    let cfg = config_for(&format!("{port}:7"), "60");
    let monitor = Monitor::bind(&cfg).await.expect("bind the monitor");

    tokio::spawn(async move {
        while let Ok((s, _)) = echo.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = s.into_split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });

    assert!(monitor.probe(&cfg).await);
}

#[tokio::test]
async fn an_echo_probe_fails_when_the_reply_differs() {
    // Right length, wrong bytes: something is answering on the port, but it is
    // not our own message coming back.
    let (liar, port) = stand_in().await;
    let cfg = config_for(&format!("{port}:7"), "1");
    let monitor = Monitor::bind(&cfg).await.expect("bind the monitor");

    tokio::spawn(async move {
        while let Ok((mut s, _)) = liar.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 1024];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let _ = s.write_all(&vec![b'X'; n]).await;
            });
        }
    });

    assert!(!monitor.probe(&cfg).await);
}

/// A scratch directory for UNIX sockets, under `/tmp` rather than `$TMPDIR`:
/// on macOS the latter is a long `/var/folders/...` path that would not leave
/// room inside `sun_path`.
struct SockDir(PathBuf);

impl SockDir {
    fn new(tag: &str) -> Self {
        let d = PathBuf::from(format!("/tmp/rash-t{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch socket dir");
        Self(d)
    }

    fn as_str(&self) -> &str {
        self.0.to_str().expect("ascii path")
    }
}

impl Drop for SockDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn a_unix_probe_succeeds_when_the_data_comes_back() {
    let dir = SockDir::new("ok");
    let cfg = config_with(
        "unix",
        &[("RASH_SOCKET_DIR", dir.as_str()), ("AUTOSSH_POLL", "60")],
    );
    let monitor = Monitor::bind(&cfg).await.expect("bind the monitor");
    let u = cfg.unix.clone().expect("unix paths");

    // Stand in for ssh's -L: accept on the outbound socket and hand the bytes
    // to the socket rash is listening on.
    let tunnel = UnixListener::bind(&u.local_out).expect("bind the stand-in");
    tokio::spawn(async move {
        while let Ok((mut inbound, _)) = tunnel.accept().await {
            let back = u.local_in.clone();
            tokio::spawn(async move {
                let Ok(mut outbound) = UnixStream::connect(&back).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
            });
        }
    });

    assert!(monitor.probe(&cfg).await);
    assert!(monitor.listener_fd().is_some());
}

#[tokio::test]
async fn a_unix_probe_fails_when_nothing_answers() {
    let dir = SockDir::new("dead");
    let cfg = config_with(
        "unix",
        &[("RASH_SOCKET_DIR", dir.as_str()), ("AUTOSSH_POLL", "1")],
    );
    let monitor = Monitor::bind(&cfg).await.expect("bind the monitor");
    // Nothing ever binds local_out, so there is no ssh to connect to.
    assert!(!monitor.probe(&cfg).await);
}

#[tokio::test]
async fn the_remote_socket_path_is_new_on_every_start() {
    // The whole reason the UNIX arrangement is safe. StreamLocalBindUnlink
    // defaults to no on the server and a client cannot override it, so reusing
    // a path that an unclean disconnect left behind would mean sshd could never
    // bind it again and rash would restart for ever against a dead forward.
    let dir = SockDir::new("rotate");
    let cfg = config_with("unix", &[("RASH_SOCKET_DIR", dir.as_str())]);
    let monitor = Monitor::bind(&cfg).await.expect("bind the monitor");

    let first = monitor.next_forwards();
    let second = monitor.next_forwards();
    assert_ne!(first, second, "the remote socket path must not be reused");

    // Only the remote half moves; rash's own sockets stay put for the run.
    let flat = |v: &[std::ffi::OsString]| {
        v.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    };
    let (a, b) = (flat(&first), flat(&second));
    assert_eq!(a[0], "-L");
    assert_eq!(a[2], "-R");
    assert!(a[1].starts_with(&format!("{}/rash-", dir.as_str())));
    assert!(b[1].starts_with(&format!("{}/rash-", dir.as_str())));
}

#[tokio::test]
async fn a_disabled_monitor_is_never_probed() {
    let cfg = config_for("0", "60");
    let monitor = Monitor::bind(&cfg).await.expect("bind the monitor");
    assert!(!monitor.enabled());
    assert!(
        monitor.listener_fd().is_none(),
        "a disabled monitor should not be listening on anything"
    );
}

#[tokio::test]
async fn the_listener_is_close_on_exec() {
    // Load-bearing: if the forked ssh inherited this socket it would hold the
    // port open after rash exits, and the next run could not bind it.
    let (_held, port) = stand_in().await;
    let cfg = config_for(&port.to_string(), "60");
    let monitor = Monitor::bind(&cfg).await.expect("bind the monitor");

    let fd = monitor.listener_fd().expect("a loop monitor listens");
    // SAFETY: the descriptor is owned by the monitor we are still holding.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(flags >= 0, "F_GETFD failed");
    assert!(
        flags & libc::FD_CLOEXEC != 0,
        "the monitor listener must be close-on-exec"
    );
}

#[test]
fn the_probe_message_is_identifiable_and_unique() {
    let cfg = config_with("20000", &[("AUTOSSH_MESSAGE", "homelab")]);

    let raw = probe::message(&cfg);
    let text = String::from_utf8(raw).expect("the message should be text");
    assert!(text.ends_with("\r\n"), "got {text:?}");

    // hostname, program, pid, nonce, user message — autossh.c:1308-1310.
    let fields: Vec<&str> = text.trim_end().split(' ').collect();
    assert_eq!(fields.len(), 5, "got {fields:?}");
    assert_eq!(fields[1], "rash");
    assert_eq!(fields[2], std::process::id().to_string());
    assert_eq!(fields[4], "homelab");

    // Two probes must never be mistaken for one another.
    assert_ne!(probe::message(&cfg), probe::message(&cfg));
}
