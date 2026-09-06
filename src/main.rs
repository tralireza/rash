//! rash — Rust Auto SSH.
//!
//! Starts an `ssh` session or tunnel, monitors it, and restarts it if it dies or
//! stops passing traffic. A behaviour-compatible reimplementation of autossh(1)
//! by Carson Harding: the same `-M`/`-f`/`-V` flags, the same `AUTOSSH_*`
//! environment variables, the same exit-status policy and backoff curve.

use rash::config::{self, Config, ConfigError, LogTarget, ProcessEnv, Resolved};
use rash::pidfile::PidFile;
use rash::supervise::{self, Verdict};
use rash::{cli, daemon, log, log_info, settings};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
usage: rash [-V] [-M monitor_port[:echo_port]] [-f] [SSH_OPTIONS]

    -M  monitor port. May be overridden by AUTOSSH_PORT (or RASH_PORT).
        0 turns the monitoring loop off. Alternatively a port for an echo
        service on the remote machine may be given (normally port 7).
    -f  run in background. rash handles this itself and does not pass it
        to ssh. Implies a gate time of 0.
    -V  print version and exit.

    --dry-run       print the ssh command and resolved settings, then exit.
    --monitor SPEC  as -M, but takes precedence over it. Also accepts `unix`,
                    which runs the monitor loop over UNIX-domain sockets so
                    there are no ports to pick on either machine.
    --session NAME  take settings from [session.NAME] in the config file.
    --config PATH   use this config file instead of the default.
    --list          list the config file's sessions and exit.
    --help          print this message and exit.
    --version       as -V.

All other options are passed through to ssh unchanged. Long options are always
rash's own, since ssh has none; everything after a `--` belongs to ssh.

Environment variables (each also accepted as RASH_*, which takes precedence):

    AUTOSSH_DEBUG        log at maximum verbosity, and to stderr
    AUTOSSH_FIRST_POLL   seconds before the first connection check
    AUTOSSH_GATETIME     seconds ssh must stay up to count as established (30)
    AUTOSSH_LOGFILE      log to this file rather than syslog
    AUTOSSH_LOGLEVEL     syslog verbosity, 0-7
    AUTOSSH_MAXLIFETIME  maximum seconds to live before shutting down
    AUTOSSH_MAXSTART     how many times to start ssh; negative means no limit
    AUTOSSH_MESSAGE      message appended to the echo string (max 64 bytes)
    AUTOSSH_PATH         path to ssh, if not /usr/bin/ssh
    AUTOSSH_PIDFILE      write the rash pid to this file
    AUTOSSH_POLL         seconds between connection checks (600)
    AUTOSSH_PORT         monitor port; overrides -M

rash-only variables:

    RASH_KILL_TIMEOUT      seconds a child gets to honour SIGTERM before it is
                           killed outright (5)
    RASH_LOG               syslog, stderr, or a path
    RASH_LOG_FORMAT        text (default) or json
    RASH_MONITOR_HOST      address the monitor forwards use (127.0.0.1)
    RASH_REMOTE_SOCKET_DIR directory for the remote monitor socket (/tmp)
    RASH_SOCKET_DIR        directory for the local monitor sockets
    RASH_SSH_PATH          path to ssh, as AUTOSSH_PATH
    RASH_TOUCH_PIDFILE     touch the pid file on every poll

The config file is optional. rash reads ~/.rash.toml if it exists, and
otherwise $XDG_CONFIG_HOME/rash/config.toml (or ~/.config/rash/config.toml).
It is the lowest layer of the precedence stack: flag, then RASH_*, then
AUTOSSH_*, then [session.NAME], then [defaults], then the built-in default.

Note that [defaults] applies to every run, including ones that name no
session, so a config file changes what a bare `rash -M ... host` does.
autossh has no config file; --config /dev/null ignores yours.
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("rash: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let inv = cli::parse(std::env::args_os().skip(1))?;

    if inv.version {
        println!("rash {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }
    if inv.help {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    // A missing default config file is normal — most runs are entirely command
    // line — but one named with --config was asked for by name, so its absence
    // is a mistake worth reporting rather than silently treating as empty.
    let config_path = match &inv.config {
        Some(p) if !p.exists() => {
            return Err(format!("no such config file: {}", p.display()).into());
        }
        Some(p) => p.clone(),
        None => default_config_path(),
    };
    let file = settings::load(&config_path)?;

    if inv.list {
        list_sessions(&config_path, &file);
        return Ok(ExitCode::SUCCESS);
    }

    // autossh insists on both a monitor port and something to hand ssh
    // (autossh.c:334-335); a bare `rash` is a usage error, not a crash. A named
    // session can supply the arguments instead of the command line.
    if inv.ssh_args.is_empty() && inv.session.is_none() {
        return Ok(usage());
    }

    let mut resolved = match config::resolve_with(inv, &ProcessEnv, &file) {
        Ok(r) => r,
        Err(ConfigError::NoMonitorPort) => return Ok(usage()),
        Err(e) => return Err(e.into()),
    };

    if resolved.config.ssh_args.is_empty() {
        return Err(
            "nothing to hand ssh: give arguments on the command line, or set \
                    ssh_args in the session"
                .into(),
        );
    }

    if resolved.config.dry_run {
        print_plan(&resolved);
        return Ok(ExitCode::SUCCESS);
    }

    // daemonize() chdirs to /, so any relative path has to be pinned down first
    // or the pid file and log would land somewhere the user did not mean.
    absolutize(&mut resolved.config);

    if resolved.config.background {
        daemon::daemonize()?;
        // The UNIX monitor's socket names carry the pid, and the fork just
        // changed it. Left alone they would name the pre-fork process, which no
        // longer exists — the same reason the pid file is written below rather
        // than above.
        if resolved.config.unix.is_some() {
            resolved.config.unix = Some(config::unix_paths(&ProcessEnv)?);
        }
    }

    log::init(&resolved.config.log)?;
    for w in &resolved.warnings {
        log_info!("{w}");
    }

    // After the fork, so the file records the daemon's pid.
    let pid_file = match &resolved.config.pid_file {
        Some(p) => Some(
            PidFile::create(p)
                .map_err(|e| format!("cannot open pid file \"{}\": {e}", p.display()))?,
        ),
        None => None,
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let verdict = runtime.block_on(supervise::run(&resolved.config, pid_file.as_ref()));

    Ok(match verdict {
        Verdict::ExitOk => ExitCode::SUCCESS,
        Verdict::ExitErr | Verdict::Restart => ExitCode::FAILURE,
    })
}

fn absolutize(cfg: &mut Config) {
    // A bare name such as `ssh` is a PATH lookup, which the chdir leaves alone;
    // execvp only skips PATH when the name contains a slash, and that is
    // exactly the case that gets resolved against the working directory. So
    // `AUTOSSH_PATH=ssh` keeps working and `AUTOSSH_PATH=./bin/ssh` stops
    // meaning `/bin/ssh` the moment we daemonise.
    if cfg.ssh_path.as_os_str().as_bytes().contains(&b'/')
        && let Ok(abs) = std::path::absolute(&cfg.ssh_path)
    {
        cfg.ssh_path = abs;
    }
    if let Some(p) = &cfg.pid_file
        && let Ok(abs) = std::path::absolute(p)
    {
        cfg.pid_file = Some(abs);
    }
    if let LogTarget::File(p) = &cfg.log.target
        && let Ok(abs) = std::path::absolute(p)
    {
        cfg.log.target = LogTarget::File(abs);
    }
}

fn usage() -> ExitCode {
    let _ = write!(io::stderr(), "{USAGE}");
    ExitCode::FAILURE
}

/// The first candidate that exists.
///
/// When none does — the usual case — the last one is returned, so anything
/// reported to the user names the conventional location rather than whichever
/// happens to be searched first.
fn default_config_path() -> PathBuf {
    let candidates = config::config_file_candidates(&ProcessEnv);
    candidates
        .iter()
        .find(|p| p.exists())
        .or_else(|| candidates.last())
        .cloned()
        .unwrap_or_default()
}

fn list_sessions(path: &Path, file: &settings::File) {
    let names = file.session_names();
    if names.is_empty() {
        println!("no sessions defined in {}", path.display());
        return;
    }
    println!("sessions in {}:", path.display());
    for n in names {
        println!("    {n}");
    }
}

/// `--dry-run`: show exactly what would be executed and with what settings.
fn print_plan(r: &Resolved) {
    let c = &r.config;

    println!(
        "rash {} — dry run, nothing is executed\n",
        env!("CARGO_PKG_VERSION")
    );

    let remote = c
        .unix
        .as_ref()
        .map(config::remote_sock_example)
        .unwrap_or_default();
    let argv = c.ssh_argv(c.monitor.forwards(c.monitor_host, c.unix.as_ref(), &remote));

    print!("ssh command:\n    {}", cli::quote(c.ssh_path.as_os_str()));
    for a in &argv {
        print!(" {}", cli::quote(a));
    }
    println!("\n");

    let or_none = |s: String| if s.is_empty() { "(none)".to_owned() } else { s };

    println!("monitor:       {}", c.monitor);
    match &c.unix {
        Some(u) => {
            println!("  local out:   {}", u.local_out.display());
            println!("  local in:    {}", u.local_in.display());
            println!(
                "  remote:      {}  (fresh on every start)",
                remote.display()
            );
        }
        None => println!("monitor host:  {}", c.monitor_host),
    }
    println!(
        "poll:          {}s (first {}s)",
        c.poll.as_secs(),
        c.first_poll.as_secs()
    );
    println!("net timeout:   {}ms", c.net_timeout.as_millis());
    println!(
        "gate time:     {}",
        if c.gate_time.is_zero() {
            "disabled".to_owned()
        } else {
            format!("{}s", c.gate_time.as_secs())
        }
    );
    println!(
        "max starts:    {}",
        if c.max_start < 0 {
            "unlimited".to_owned()
        } else {
            c.max_start.to_string()
        }
    );
    println!(
        "max lifetime:  {}",
        c.max_lifetime
            .map_or("unlimited".to_owned(), |d| format!("{}s", d.as_secs()))
    );
    println!("kill timeout:  {}s", c.kill_timeout.as_secs());
    println!("background:    {}", c.background);
    println!(
        "pid file:      {}{}",
        or_none(
            c.pid_file
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ),
        if c.touch_pid_file { " (touched)" } else { "" }
    );
    println!("message:       {}", or_none(c.message.clone()));
    println!(
        "log:           {} as {} at level {}{}",
        c.log.target,
        c.log.format,
        c.log.level,
        if c.log.also_stderr {
            " (also stderr)"
        } else {
            ""
        }
    );

    for w in &r.warnings {
        println!("\nwarning: {w}");
    }
}
