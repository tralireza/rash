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
//! Both TCP and UNIX-domain forwards are understood, chosen by the shape of the
//! `-L` value: `port:host:port` is TCP, `/path:/path` is a socket pair. Standing
//! in for the UNIX arrangement means playing the whole far side — ssh's local
//! listener, sshd's remote one, and the link between them — so the socket rash
//! writes to is bound here and its bytes are handed straight to the socket rash
//! is listening on.
//!
//! It is built only with the `test-harness` feature, which is on by default but
//! can be turned off so `cargo install` produces just `rash`.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

fn main() {
    exit_when_orphaned();
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

/// One end of a forward, as it appears in `-L`/`-R`.
///
/// TCP is `listen_port:host:target_port`; UNIX is `listen_path:target_path`.
enum Forward {
    Tcp { listen: u16, target: u16 },
    Unix { listen: PathBuf, target: PathBuf },
}

/// Pull a forward out of argv.
///
/// A leading `/` says the value is a socket pair, which is unambiguous: ssh's
/// TCP forms all begin with a port number or a bind address. For the TCP form,
/// splitting from both ends rather than on every colon keeps a bracketed IPv6
/// host in the middle intact.
fn forward(flag: &str) -> Option<Forward> {
    let argv: Vec<String> = std::env::args().collect();
    let spec = argv
        .iter()
        .position(|a| a == flag)
        .and_then(|i| argv.get(i + 1))?;

    if spec.starts_with('/') {
        let (listen, target) = spec.split_once(':')?;
        return Some(Forward::Unix {
            listen: PathBuf::from(listen),
            target: PathBuf::from(target),
        });
    }

    let (listen, rest) = spec.split_once(':')?;
    let (_host, target) = rest.rsplit_once(':')?;
    Some(Forward::Tcp {
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

    match (left, right) {
        (Some(Forward::Tcp { listen, .. }), r) => {
            let back = match r {
                Some(Forward::Tcp { target, .. }) => Some(target),
                _ => None,
            };
            tcp_tunnel(mode, listen, back);
        }
        // The -L value is `local_out:remote_sock` and the -R value is
        // `remote_sock:local_in`, so the socket to hand the bytes back to is
        // the -R target. Nothing ever binds the remote path: this process is
        // standing in for both ends of the link, so there is no link to cross.
        (Some(Forward::Unix { listen, .. }), r) => {
            let back = match r {
                Some(Forward::Unix { target, .. }) => Some(target),
                _ => None,
            };
            unix_tunnel(mode, listen, back);
        }
        (None, _) => {}
    }
}

fn tcp_tunnel(mode: &str, listen: u16, back_to: Option<u16>) {
    match mode {
        // Local port -> the port the remote's -R forward would deliver to.
        "loop" => {
            let Some(target) = back_to else { return };
            spawn_server(
                move || TcpListener::bind(("127.0.0.1", listen)),
                format!("127.0.0.1:{listen}"),
                move |mut inbound| {
                    let Ok(mut outbound) = TcpStream::connect(("127.0.0.1", target)) else {
                        return;
                    };
                    splice(&mut inbound, &mut outbound);
                },
            );
        }
        "echo" => spawn_server(
            move || TcpListener::bind(("127.0.0.1", listen)),
            format!("127.0.0.1:{listen}"),
            echo,
        ),
        // Accept, then do nothing. poll() cannot distinguish this from a healthy
        // but idle tunnel, which is why the probe needs a timeout.
        "blackhole" => spawn_server(
            move || TcpListener::bind(("127.0.0.1", listen)),
            format!("127.0.0.1:{listen}"),
            blackhole,
        ),
        _ => {}
    }
}

fn unix_tunnel(mode: &str, listen: PathBuf, back_to: Option<PathBuf>) {
    // rash unlinks this before every start, so there should be nothing here —
    // but a stale socket would fail the bind and look like a hung tunnel.
    let _ = std::fs::remove_file(&listen);
    let name = listen.display().to_string();
    let bind = {
        let listen = listen.clone();
        move || UnixListener::bind(&listen)
    };

    match mode {
        "loop" => {
            let Some(target) = back_to else { return };
            spawn_server(bind, name, move |mut inbound| {
                let Ok(mut outbound) = UnixStream::connect(&target) else {
                    return;
                };
                splice(&mut inbound, &mut outbound);
            });
        }
        "echo" => spawn_server(bind, name, echo),
        "blackhole" => spawn_server(bind, name, blackhole),
        _ => {}
    }
}

/// Copy in both directions until either side closes.
fn splice<S: Splittable>(inbound: &mut S, outbound: &mut S) {
    let (Ok(mut a), Ok(mut b)) = (inbound.dup(), outbound.dup()) else {
        return;
    };
    thread::spawn(move || {
        let _ = std::io::copy(&mut a, &mut b);
    });
    let _ = std::io::copy(outbound, inbound);
}

fn echo<S: Splittable>(mut s: S) {
    let Ok(mut back) = s.dup() else { return };
    let _ = std::io::copy(&mut back, &mut s);
}

fn blackhole<S: Splittable>(s: S) {
    thread::sleep(Duration::from_secs(3600));
    drop(s);
}

/// The little that the tunnel bodies need from a stream, so one set of them
/// serves both `TcpStream` and `UnixStream`.
trait Splittable: std::io::Read + std::io::Write + Send + Sized + 'static {
    fn dup(&self) -> std::io::Result<Self>;
}

impl Splittable for TcpStream {
    fn dup(&self) -> std::io::Result<Self> {
        self.try_clone()
    }
}

impl Splittable for UnixStream {
    fn dup(&self) -> std::io::Result<Self> {
        self.try_clone()
    }
}

/// Accept for ever, handing each connection to `handle` on its own thread.
///
/// `bind` is a closure rather than a bound listener so the failure is reported
/// from inside the spawned thread, where `name` can describe what could not be
/// bound.
fn spawn_server<L, S, B, H>(bind: B, name: String, handle: H)
where
    L: Listener<Conn = S>,
    S: Send + 'static,
    B: FnOnce() -> std::io::Result<L> + Send + 'static,
    // Clone rather than Copy: the UNIX handlers capture a PathBuf.
    H: Fn(S) + Send + Clone + 'static,
{
    thread::spawn(move || {
        let listener = match bind() {
            Ok(l) => l,
            Err(e) => {
                // Returning quietly here would surface as an unexplained probe
                // timeout twenty seconds later in whichever test is running.
                // Exiting makes the supervisor log it immediately instead.
                eprintln!("fake-ssh: cannot bind {name}: {e}");
                std::process::exit(70);
            }
        };
        while let Ok(conn) = listener.take() {
            let handle = handle.clone();
            thread::spawn(move || handle(conn));
        }
    });
}

/// One `accept()`, over whichever address family.
trait Listener: Send + 'static {
    type Conn;
    fn take(&self) -> std::io::Result<Self::Conn>;
}

impl Listener for TcpListener {
    type Conn = TcpStream;
    fn take(&self) -> std::io::Result<TcpStream> {
        self.accept().map(|(s, _)| s)
    }
}

impl Listener for UnixListener {
    type Conn = UnixStream;
    fn take(&self) -> std::io::Result<UnixStream> {
        self.accept().map(|(s, _)| s)
    }
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

/// Stop as soon as the supervisor goes away.
///
/// A test that has to resort to SIGKILL leaves rash no chance to reap this
/// process, so it is reparented to init and — in `sleep` mode — holds its
/// forward's ports until the machine is rebooted. Nothing here is worth
/// outliving its parent, and there is no portable `PR_SET_PDEATHSIG`, so poll.
fn exit_when_orphaned() {
    // SAFETY: getppid takes no arguments and only reads the calling process.
    let parent = unsafe { libc::getppid() };
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_millis(250));
            // SAFETY: as above.
            if unsafe { libc::getppid() } != parent {
                std::process::exit(0);
            }
        }
    });
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
