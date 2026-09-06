//! The supervision loop.
//!
//! autossh's equivalent is `ssh_run()`/`ssh_watch()` (autossh.c:711-892), built
//! on `sigsetjmp`/`siglongjmp`, `alarm()` and `pause()`, with `syslog()`
//! reachable from the signal handler. Here the same events — the child exiting,
//! the poll timer firing, the lifetime deadline passing, a signal arriving — are
//! arms of one `select!`. There is no handler context to be careful in and no
//! jump buffer to unwind, and signals stay live for the whole run rather than
//! only while a child exists (autossh CHANGES, 1.4f).

use crate::backoff::Backoff;
use crate::config::Config;
use crate::monitor::Monitor;
use crate::pidfile::PidFile;
use crate::{log_debug, log_err, log_info};
use std::fmt;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::time::{Instant, MissedTickBehavior};

/// What should happen once the ssh child is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Restart,
    ExitOk,
    ExitErr,
}

/// Why the supervisor reached its verdict. Kept separate from [`Verdict`] so the
/// log line and the tests can tell "never made it out of the starting gate"
/// apart from "the command line was wrong".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Signalled,
    PrematureExit,
    ConnectionLost,
    CleanExit,
    Failed,
}

/// How the child died.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Death {
    Signal(i32),
    Exit(i32),
}

impl Death {
    pub fn from_status(s: ExitStatus) -> Self {
        match s.signal() {
            Some(sig) => Self::Signal(sig),
            None => Self::Exit(s.code().unwrap_or(0)),
        }
    }
}

/// autossh's `ssh_wait()` policy (autossh.c:960-1071).
///
/// `uptime` is how long this child ran, and `start_count` counts from 1.
pub fn classify(
    death: Death,
    start_count: u64,
    uptime: Duration,
    gate: Duration,
) -> (Verdict, Reason) {
    let code = match death {
        // A killed child was more likely hung than deliberately stopped, so
        // restarting beats assuming we were meant to exit too (CHANGES, 1.4f).
        Death::Signal(_) => return (Verdict::Restart, Reason::Signalled),
        Death::Exit(c) => c,
    };

    // The starting gate: a first session that dies at once never authenticated
    // or never connected, and retrying would only spin (autossh.c:998-1010).
    if start_count == 1 && !gate.is_zero() && uptime <= gate {
        return (Verdict::ExitErr, Reason::PrematureExit);
    }

    match code {
        // ssh reports both a dropped connection and a failed authentication as
        // 255 and gives us no way to tell them apart — hence the gate above.
        255 => (Verdict::Restart, Reason::ConnectionLost),
        0 => (Verdict::ExitOk, Reason::CleanExit),
        // 1 and 2 on a later run mean the network went away, not that the
        // command line is wrong. 2 can also come from a tunnel setup race.
        1 | 2 if start_count > 1 || gate.is_zero() => (Verdict::Restart, Reason::ConnectionLost),
        _ => (Verdict::ExitErr, Reason::Failed),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sig {
    Term,
    Int,
    Quit,
    Hup,
    Usr1,
    Usr2,
}

impl fmt::Display for Sig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Term => "SIGTERM",
            Self::Int => "SIGINT",
            Self::Quit => "SIGQUIT",
            Self::Hup => "SIGHUP",
            Self::Usr1 => "SIGUSR1",
            Self::Usr2 => "SIGUSR2",
        };
        f.write_str(s)
    }
}

/// The signals rash reacts to.
///
/// SIGPIPE is not here because the Rust runtime already sets it to `SIG_IGN` at
/// startup, which is what autossh arranges by hand (autossh.c:1202-1204).
pub struct Signals {
    term: Signal,
    int: Signal,
    quit: Signal,
    hup: Signal,
    usr1: Signal,
    usr2: Signal,
}

impl Signals {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
            quit: signal(SignalKind::quit())?,
            hup: signal(SignalKind::hangup())?,
            usr1: signal(SignalKind::user_defined1())?,
            usr2: signal(SignalKind::user_defined2())?,
        })
    }

    pub async fn next(&mut self) -> Sig {
        tokio::select! {
            _ = self.term.recv() => Sig::Term,
            _ = self.int.recv()  => Sig::Int,
            _ = self.quit.recv() => Sig::Quit,
            _ = self.hup.recv()  => Sig::Hup,
            _ = self.usr1.recv() => Sig::Usr1,
            _ = self.usr2.recv() => Sig::Usr2,
        }
    }
}

/// Start ssh, watch it, restart it, until something says to stop.
pub async fn run(cfg: &Config, pid_file: Option<&PidFile>) -> Verdict {
    let mut sigs = match Signals::new() {
        Ok(s) => s,
        Err(e) => {
            log_err!("cannot install signal handlers: {e}");
            return Verdict::ExitErr;
        }
    };

    // Opened once and held for the whole run. A failure to bind is fatal: the
    // loop could never complete, so every probe would fail (autossh.c:465-472).
    let monitor = match Monitor::bind(cfg).await {
        Ok(m) => m,
        Err(e) => {
            log_err!("cannot open monitor socket: {e}");
            return Verdict::ExitErr;
        }
    };

    let ctx = Ctx {
        cfg,
        monitor: &monitor,
        pid_file,
        deadline: cfg.max_lifetime.map(|d| Instant::now() + d),
    };
    let deadline = ctx.deadline;
    let mut backoff = Backoff::default();
    let mut start_count: u64 = 0;
    let mut last_start: Option<Instant> = None;

    loop {
        if cfg.max_start >= 0 && start_count >= cfg.max_start as u64 {
            log_info!("max start count reached; exiting");
            return Verdict::ExitOk;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            log_info!("exceeded maximum time to live, shutting down");
            return Verdict::ExitOk;
        }

        let uptime = last_start.map_or(Duration::MAX, |t| t.elapsed());
        let delay = backoff.next_delay(uptime, cfg.poll);
        log_debug!("checking for grace period, tries = {}", backoff.tries());
        if !delay.is_zero() {
            log_debug!("sleeping for grace time {} secs", delay.as_secs());
            // autossh leaves most signals unhandled while it sleeps here, so a
            // SIGHUP meant to prod it kills it instead. This stays responsive.
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                sig = sigs.next() => {
                    if let Some(v) = exiting(sig) {
                        log_info!("received signal to exit ({sig})");
                        return v;
                    }
                    log_debug!("{sig} during backoff; retrying now");
                }
            }
        }

        start_count += 1;
        if cfg.max_start < 0 {
            log_info!("starting ssh (count {start_count})");
        } else {
            log_info!("starting ssh (count {start_count} of {})", cfg.max_start);
        }

        let mut child = match spawn(cfg) {
            Ok(c) => c,
            Err(e) => {
                // Rust reports a failed exec back through spawn(), so unlike
                // autossh there is no forked child left to signal us with
                // SIGTERM to break the restart loop (autossh.c:748-752).
                log_err!("{}: {e}", cfg.ssh_path.display());
                return Verdict::ExitErr;
            }
        };
        let started = Instant::now();
        last_start = Some(started);
        log_info!("ssh child pid is {}", child.id().unwrap_or(0));

        let verdict = watch(&mut child, &ctx, start_count, started, &mut sigs).await;

        if verdict != Verdict::Restart {
            return verdict;
        }
    }
}

/// The parts of a run that do not change from one restart to the next.
struct Ctx<'a> {
    cfg: &'a Config,
    monitor: &'a Monitor,
    pid_file: Option<&'a PidFile>,
    deadline: Option<Instant>,
}

/// One event at a time, until the child's fate is decided.
enum Event {
    Exited(io::Result<ExitStatus>),
    Tick,
    Deadline,
    Signal(Sig),
}

async fn watch(
    child: &mut Child,
    ctx: &Ctx<'_>,
    start_count: u64,
    started: Instant,
    sigs: &mut Signals,
) -> Verdict {
    let cfg = ctx.cfg;
    let mut tick = tokio::time::interval_at(Instant::now() + cfg.first_poll, cfg.poll);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        // The futures are built here and dropped as soon as one wins, which is
        // what frees `child` for the handlers below to use.
        let event = tokio::select! {
            status = child.wait() => Event::Exited(status),
            _ = tick.tick() => Event::Tick,
            _ = at(ctx.deadline) => Event::Deadline,
            sig = sigs.next() => Event::Signal(sig),
        };

        match event {
            Event::Exited(Err(e)) => {
                log_err!("waiting on ssh: {e}");
                return Verdict::ExitErr;
            }
            Event::Exited(Ok(status)) => {
                let death = Death::from_status(status);
                let (verdict, reason) =
                    classify(death, start_count, started.elapsed(), cfg.gate_time);
                report(death, reason);
                return verdict;
            }
            Event::Tick => {
                log_debug!("check on child {}", child.id().unwrap_or(0));

                if ctx.monitor.enabled() && !ctx.monitor.probe(cfg).await {
                    log_info!("port down, restarting ssh");
                    kill(child, cfg).await;
                    return Verdict::Restart;
                }

                if cfg.touch_pid_file
                    && let Some(p) = ctx.pid_file
                    && let Err(e) = p.touch()
                {
                    log_err!("could not touch pid file: {e}");
                }
            }
            Event::Deadline => {
                log_info!("exceeded maximum time to live, shutting down");
                kill(child, cfg).await;
                return Verdict::ExitOk;
            }
            Event::Signal(sig) => match sig {
                Sig::Term | Sig::Int | Sig::Quit => {
                    log_info!("received signal to exit ({sig})");
                    kill(child, cfg).await;
                    return Verdict::ExitErr;
                }
                Sig::Usr1 => {
                    log_info!("signalled to kill and restart ssh");
                    kill(child, cfg).await;
                    return Verdict::Restart;
                }
                Sig::Hup | Sig::Usr2 => log_debug!("woken by {sig}"),
            },
        }
    }
}

fn spawn(cfg: &Config) -> io::Result<Child> {
    // No process_group() call: the child shares rash's group, as autossh's
    // fork/execvp child does, so a terminal ^C reaches both.
    Command::new(&cfg.ssh_path)
        .args(&cfg.ssh_args)
        .kill_on_drop(false)
        .spawn()
}

/// SIGTERM, then SIGKILL if the child will not go.
///
/// autossh blocks in `waitpid()` here for as long as it takes (autossh.c:1078-1102);
/// its own comment questions the design. A child that ignores SIGTERM wedges the
/// supervisor permanently, so rash escalates.
async fn kill(child: &mut Child, cfg: &Config) {
    let Some(pid) = child.id() else {
        return; // already reaped
    };

    log_debug!("sending SIGTERM to {pid}");
    // SAFETY: `pid` belongs to a child of this process that has not been reaped,
    // so the id cannot yet have been reused, and SIGTERM is a valid signal.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };

    match tokio::time::timeout(cfg.kill_timeout, child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => log_err!("waitpid() not successful: {e}"),
        Err(_) => {
            log_err!(
                "ssh {pid} ignored SIGTERM after {}s; sending SIGKILL",
                cfg.kill_timeout.as_secs()
            );
            let _ = child.start_kill();
            if let Err(e) = child.wait().await {
                log_err!("waitpid() not successful: {e}");
            }
        }
    }
}

/// A future that completes at `deadline`, or never if there is not one.
async fn at(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// Signals that mean "stop", and the verdict each produces.
fn exiting(sig: Sig) -> Option<Verdict> {
    match sig {
        Sig::Term | Sig::Int | Sig::Quit => Some(Verdict::ExitErr),
        Sig::Hup | Sig::Usr1 | Sig::Usr2 => None,
    }
}

/// Log the child's death using autossh's wording, so existing log greps still hit.
fn report(death: Death, reason: Reason) {
    match (death, reason) {
        (Death::Signal(s), _) => log_info!("ssh exited on signal {s}, restarting ssh"),
        (Death::Exit(c), Reason::PrematureExit) => {
            log_err!("ssh exited prematurely with status {c}; rash exiting")
        }
        (Death::Exit(c), Reason::ConnectionLost) => {
            log_info!("ssh exited with error status {c}; restarting ssh")
        }
        (Death::Exit(c), Reason::CleanExit | Reason::Failed) => {
            log_info!("ssh exited with status {c}; rash exiting")
        }
        (Death::Exit(c), Reason::Signalled) => log_info!("ssh exited with status {c}"),
    }
}
