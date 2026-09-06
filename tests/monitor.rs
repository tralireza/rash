//! The connection monitor, exercised against in-process stand-ins for ssh.

use rash::cli;
use rash::config::{self, Config};
use rash::monitor::{Monitor, probe};
use std::ffi::OsString;
use std::net::TcpListener as StdTcpListener;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn config_with(spec: &str, env: &[(&str, &str)]) -> Config {
    let inv = cli::parse(["-M", spec, "-N", "host"].iter().map(OsString::from)).expect("parse");
    config::resolve(inv, env).expect("resolve").config
}

/// A poll of 1s gives a 500ms net timeout, which keeps the failure paths quick.
fn config_for(spec: &str, poll: &str) -> Config {
    config_with(spec, &[("AUTOSSH_POLL", poll)])
}

/// Bind a stand-in server and return it along with its port, which is always
/// even and whose successor is free.
///
/// The listener is held rather than probed and released, so a parallel test
/// cannot take the port from under us. Handing out only even ports means no
/// test's server can ever land on another test's monitor port, which is always
/// odd (`port + 1`).
async fn stand_in() -> (TcpListener, u16) {
    for _ in 0..500 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let p = listener.local_addr().expect("local_addr").port();
        if p % 2 == 0 && p < u16::MAX && StdTcpListener::bind(("127.0.0.1", p + 1)).is_ok() {
            return (listener, p);
        }
    }
    panic!("could not find a usable even port");
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

#[tokio::test]
async fn a_disabled_monitor_is_never_probed() {
    let cfg = config_for("0", "60");
    let monitor = Monitor::bind(&cfg).await.expect("bind the monitor");
    assert!(!monitor.enabled());
    assert!(matches!(monitor, Monitor::Disabled));
}

#[tokio::test]
async fn the_listener_is_close_on_exec() {
    // Load-bearing: if the forked ssh inherited this socket it would hold the
    // port open after rash exits, and the next run could not bind it.
    let (_held, port) = stand_in().await;
    let cfg = config_for(&port.to_string(), "60");
    let monitor = Monitor::bind(&cfg).await.expect("bind the monitor");

    let Monitor::Loop { listener, .. } = &monitor else {
        panic!("expected a loop monitor");
    };
    // SAFETY: the descriptor is owned by the listener we are still holding.
    let flags = unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_GETFD) };
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
