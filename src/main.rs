//! rash — Rust Auto SSH.
//!
//! Starts an `ssh` session or tunnel, monitors it, and restarts it if it dies or
//! stops passing traffic. A behaviour-compatible reimplementation of autossh(1)
//! by Carson Harding: the same `-M`/`-f`/`-V` flags, the same `AUTOSSH_*`
//! environment variables, the same exit-status policy and backoff curve.

use rash::config::{self, Config, ConfigError, LogTarget, ProcessEnv, Resolved};
use rash::pidfile::PidFile;
use rash::supervise::{self, Verdict};
use rash::{cli, daemon, log, log_info};
use std::io::{self, Write};
use std::process::ExitCode;

const USAGE: &str = "\
usage: rash [-V] [-M monitor_port[:echo_port]] [-f] [SSH_OPTIONS]

    -M  monitor port. May be overridden by AUTOSSH_PORT (or RASH_PORT).
        0 turns the monitoring loop off. Alternatively a port for an echo
        service on the remote machine may be given (normally port 7).
    -f  run in background. rash handles this itself and does not pass it
        to ssh. Implies a gate time of 0.
    -V  print version and exit.

    --dry-run  print the ssh command and resolved settings, then exit.
    --monitor SPEC  as -M, but takes precedence over it.

All other options are passed through to ssh unchanged.

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

rash-only variables: RASH_KILL_TIMEOUT, RASH_MONITOR_HOST, RASH_TOUCH_PIDFILE.
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
    // autossh insists on both a monitor port and something to hand ssh
    // (autossh.c:334-335); a bare `rash` is a usage error, not a crash.
    if inv.ssh_args.is_empty() {
        return Ok(usage());
    }

    let mut resolved = match config::resolve(inv, &ProcessEnv) {
        Ok(r) => r,
        Err(ConfigError::NoMonitorPort) => return Ok(usage()),
        Err(e) => return Err(e.into()),
    };

    if resolved.config.dry_run {
        print_plan(&resolved);
        return Ok(ExitCode::SUCCESS);
    }

    // daemonize() chdirs to /, so any relative path has to be pinned down first
    // or the pid file and log would land somewhere the user did not mean.
    absolutize(&mut resolved.config);

    if resolved.config.background {
        daemon::daemonize()?;
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

/// `--dry-run`: show exactly what would be executed and with what settings.
fn print_plan(r: &Resolved) {
    let c = &r.config;

    println!(
        "rash {} — dry run, nothing is executed\n",
        env!("CARGO_PKG_VERSION")
    );

    print!("ssh command:\n    {}", cli::quote(c.ssh_path.as_os_str()));
    for a in &c.ssh_args {
        print!(" {}", cli::quote(a));
    }
    println!("\n");

    let or_none = |s: String| if s.is_empty() { "(none)".to_owned() } else { s };

    println!("monitor:       {}", c.monitor);
    println!("monitor host:  {}", c.monitor_host);
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
        "log:           {} at level {}{}",
        c.log.target,
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
