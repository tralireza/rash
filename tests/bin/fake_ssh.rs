//! A stand-in for ssh, so the supervisor can be tested without a network.
//!
//! autossh has no tests at all, largely because exercising it means having a
//! reachable host, working keys, and a way to break the connection on cue. This
//! binary removes all three: point `RASH_SSH_PATH` at it and drive its behaviour
//! from the environment.
//!
//! | variable | meaning |
//! |---|---|
//! | `FAKE_SSH_STATE` | file to append one line of argv to per start; its line count is how this process knows which start it is |
//! | `FAKE_SSH_MODE` | `sleep` (default), `hang`, or `exit` |
//! | `FAKE_SSH_EXIT_CODES` | comma-separated codes for `exit` mode, indexed by start, the last repeating |
//! | `FAKE_SSH_DELAY_MS` | how long to stay up before exiting, in `exit` mode |
//! | `FAKE_SSH_TUNNEL` | `auto` (default), `loop`, `echo`, `blackhole`, or `none` |
//!
//! In `auto` the tunnel is inferred from the forwards rash injected: `-L` with
//! `-R` is a loop, `-L` alone is an echo. `blackhole` accepts connections and
//! then moves no bytes at all, which is what a wedged tunnel looks like from the
//! outside — the case the whole monitor exists to catch.
//!
//! It is built only with the `test-harness` feature, which is on by default but
//! can be turned off so `cargo install` produces just `rash`.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

fn main() {
    let start = record_start();
    start_tunnel();

    match env_str("FAKE_SSH_MODE", "sleep").as_str() {
        // A child that refuses to die on SIGTERM. autossh blocks forever waiting
        // for this one; rash is expected to escalate to SIGKILL.
        "hang" => {
            ignore_sigterm();
            sleep_forever();
        }
        "exit" => {
            thread::sleep(Duration::from_millis(env_num("FAKE_SSH_DELAY_MS", 50)));
            std::process::exit(exit_code_for(start));
        }
        // A healthy session: stay up until signalled.
        _ => sleep_forever(),
    }
}

/// One end of a port forward, as it appears in `-L`/`-R`: `listen:host:target`.
struct Forward {
    listen: u16,
    target: u16,
}

/// Pull a forward out of argv. Splitting from both ends rather than on every
/// colon keeps a bracketed IPv6 host in the middle intact.
fn forward(flag: &str) -> Option<Forward> {
    let argv: Vec<String> = std::env::args().collect();
    let spec = argv
        .iter()
        .position(|a| a == flag)
        .and_then(|i| argv.get(i + 1))?;
    let (listen, rest) = spec.split_once(':')?;
    let (_host, target) = rest.rsplit_once(':')?;
    Some(Forward {
        listen: listen.parse().ok()?,
        target: target.parse().ok()?,
    })
}

fn start_tunnel() {
    let left = forward("-L");
    let right = forward("-R");

    let mode = env_str("FAKE_SSH_TUNNEL", "auto");
    let mode = match mode.as_str() {
        "auto" if left.is_some() && right.is_some() => "loop",
        "auto" if left.is_some() => "echo",
        "auto" => "none",
        other => other,
    };

    let Some(l) = left else { return };

    match mode {
        // Local port -> the port the remote's -R forward would deliver to.
        "loop" => {
            let Some(r) = right else { return };
            spawn_server(l.listen, move |mut inbound| {
                let Ok(mut outbound) = TcpStream::connect(("127.0.0.1", r.target)) else {
                    return;
                };
                let (Ok(mut a), Ok(mut b)) = (inbound.try_clone(), outbound.try_clone()) else {
                    return;
                };
                thread::spawn(move || {
                    let _ = std::io::copy(&mut a, &mut b);
                });
                let _ = std::io::copy(&mut outbound, &mut inbound);
            });
        }
        "echo" => spawn_server(l.listen, |mut s| {
            let Ok(mut back) = s.try_clone() else { return };
            let _ = std::io::copy(&mut back, &mut s);
        }),
        // Accept, then do nothing. poll() cannot distinguish this from a healthy
        // but idle tunnel, which is why the probe needs a timeout.
        "blackhole" => spawn_server(l.listen, |s| {
            thread::sleep(Duration::from_secs(3600));
            drop(s);
        }),
        _ => {}
    }
}

fn spawn_server(port: u16, handle: impl Fn(TcpStream) + Send + Copy + 'static) {
    thread::spawn(move || {
        let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) else {
            return;
        };
        for conn in listener.incoming().flatten() {
            thread::spawn(move || handle(conn));
        }
    });
}

/// Append this invocation's argv to the state file and return which start it is,
/// counting from 1.
fn record_start() -> usize {
    let Ok(path) = std::env::var("FAKE_SSH_STATE") else {
        return 1;
    };

    let before = OpenOptions::new()
        .read(true)
        .open(&path)
        .map(|f| BufReader::new(f).lines().count())
        .unwrap_or(0);

    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        let _ = writeln!(f, "{}", argv.join(" "));
        let _ = f.flush();
    }

    before + 1
}

/// The code for this start. The list is indexed by start number and its last
/// entry repeats, so `255,0` means "fail once, then succeed for ever after".
fn exit_code_for(start: usize) -> i32 {
    let codes = env_str("FAKE_SSH_EXIT_CODES", "0");
    let codes: Vec<i32> = codes
        .split(',')
        .filter_map(|c| c.trim().parse().ok())
        .collect();
    if codes.is_empty() {
        return 0;
    }
    codes[(start - 1).min(codes.len() - 1)]
}

fn ignore_sigterm() {
    // SAFETY: SIG_IGN is a valid disposition for SIGTERM, and this process has
    // no handler state to disturb.
    unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
}

fn sleep_forever() -> ! {
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

fn env_str(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

fn env_num(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
