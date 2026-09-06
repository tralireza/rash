//! Resolving a complete configuration from the command line and the environment.
//!
//! This is a pure function: it reads no files, opens no sockets, and logs
//! nothing. Anything worth telling the user about comes back as a warning for
//! the caller to emit once a log sink exists.

use crate::cli::{self, Invocation};
use std::ffi::OsString;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::Duration;

/// autossh's compiled-in default (`SSH_PATH`, autossh.c:88-90).
const DEFAULT_SSH_PATH: &str = "/usr/bin/ssh";
/// `POLL_TIME`, autossh.c:92.
const DEFAULT_POLL: u64 = 600;
/// `GATE_TIME`, autossh.c:93.
const DEFAULT_GATE: u64 = 30;
/// `TIMEO_NET`, autossh.c:95 — milliseconds.
const DEFAULT_NET_TIMEOUT_MS: u64 = 15_000;
/// `MAX_MESSAGE`, autossh.c:98.
const MAX_MESSAGE: usize = 64;
/// rash's own: how long a child gets to honour SIGTERM before SIGKILL.
const DEFAULT_KILL_TIMEOUT: u64 = 5;

/// How the connection is monitored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Monitor {
    /// `-M 0`: no monitoring; react only to ssh exiting and to signals.
    Disabled,
    /// `-M port`: a loop of forwardings. Write to `port`, read back on `port + 1`.
    Loop { port: u16 },
    /// `-M port:echo`: a remote echo service. `port` carries both directions.
    Echo { port: u16, echo: u16 },
}

impl Monitor {
    /// The ssh arguments that build the monitor path (§1.3 of the plan).
    pub fn forwards(&self, host: IpAddr) -> Vec<OsString> {
        let host = host_literal(host);
        match *self {
            Self::Disabled => vec![],
            Self::Loop { port } => vec![
                "-L".into(),
                format!("{port}:{host}:{port}").into(),
                "-R".into(),
                format!("{port}:{host}:{}", port + 1).into(),
            ],
            Self::Echo { port, echo } => {
                vec!["-L".into(), format!("{port}:{host}:{echo}").into()]
            }
        }
    }

    /// The port rash writes its probe to, if any.
    pub fn write_port(&self) -> Option<u16> {
        match *self {
            Self::Disabled => None,
            Self::Loop { port } | Self::Echo { port, .. } => Some(port),
        }
    }

    /// The port rash listens on for the probe to come back, if any.
    pub fn read_port(&self) -> Option<u16> {
        match *self {
            Self::Loop { port } => Some(port + 1),
            Self::Disabled | Self::Echo { .. } => None,
        }
    }
}

/// ssh's forwarding specs are colon-separated, so an IPv6 literal has to be
/// bracketed or it is unparseable: `-L 20000:[::1]:20000`, never `20000:::1:20000`.
/// autossh cannot reach this case at all — it hardcodes `AF_INET` (autossh.c:1624).
fn host_literal(host: IpAddr) -> String {
    match host {
        IpAddr::V4(a) => a.to_string(),
        IpAddr::V6(a) => format!("[{a}]"),
    }
}

impl fmt::Display for Monitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Disabled => write!(f, "disabled"),
            Self::Loop { port } => write!(f, "loop, write {port}, read {}", port + 1),
            Self::Echo { port, echo } => write!(f, "echo, write {port} to remote echo {echo}"),
        }
    }
}

/// Log verbosity, numbered as syslog(3) levels so `AUTOSSH_LOGLEVEL` carries over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Emerg = 0,
    Alert = 1,
    Crit = 2,
    Err = 3,
    Warning = 4,
    Notice = 5,
    Info = 6,
    Debug = 7,
}

impl Level {
    fn from_num(n: u8) -> Option<Self> {
        Some(match n {
            0 => Self::Emerg,
            1 => Self::Alert,
            2 => Self::Crit,
            3 => Self::Err,
            4 => Self::Warning,
            5 => Self::Notice,
            6 => Self::Info,
            7 => Self::Debug,
            _ => return None,
        })
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Emerg => "emerg",
            Self::Alert => "alert",
            Self::Crit => "crit",
            Self::Err => "err",
            Self::Warning => "warning",
            Self::Notice => "notice",
            Self::Info => "info",
            Self::Debug => "debug",
        };
        f.write_str(s)
    }
}

/// Where log lines go. autossh's `L_SYSLOG` / `L_FILELOG` split (autossh.c:105-106).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogTarget {
    Syslog,
    File(PathBuf),
    Stderr,
}

impl fmt::Display for LogTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syslog => f.write_str("syslog"),
            Self::File(p) => write!(f, "file {}", p.display()),
            Self::Stderr => f.write_str("stderr"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Log {
    pub target: LogTarget,
    pub level: Level,
    /// `AUTOSSH_DEBUG` also mirrors syslog output to stderr (`LOG_PERROR`).
    pub also_stderr: bool,
}

/// Everything rash needs in order to run, fully resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub ssh_path: PathBuf,
    /// ssh's argv\[1..\], with the monitor forwards already injected.
    pub ssh_args: Vec<OsString>,
    pub monitor: Monitor,
    pub monitor_host: IpAddr,
    pub poll: Duration,
    pub first_poll: Duration,
    pub net_timeout: Duration,
    pub gate_time: Duration,
    /// Negative means no limit (`MAX_START`, autossh.c:97).
    pub max_start: i64,
    pub max_lifetime: Option<Duration>,
    pub message: String,
    pub pid_file: Option<PathBuf>,
    pub touch_pid_file: bool,
    pub background: bool,
    pub kill_timeout: Duration,
    pub log: Log,
    pub dry_run: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// Neither `-M`, `--monitor`, nor `AUTOSSH_PORT` gave a monitor port.
    /// autossh answers this with usage rather than a message (autossh.c:334-335).
    NoMonitorPort,
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMonitorPort => f.write_str("no monitor port given"),
            Self::Invalid(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ConfigError {}

/// A resolved configuration plus anything the user should be told about it.
#[derive(Debug, PartialEq, Eq)]
pub struct Resolved {
    pub config: Config,
    pub warnings: Vec<String>,
}

/// A source of environment variables, so resolution can be tested without
/// touching the real process environment.
pub trait EnvSource {
    fn var(&self, key: &str) -> Option<OsString>;
}

/// The real process environment.
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn var(&self, key: &str) -> Option<OsString> {
        std::env::var_os(key)
    }
}

impl<S: AsRef<str>> EnvSource for [(S, S)] {
    fn var(&self, key: &str) -> Option<OsString> {
        self.iter()
            .find(|(k, _)| k.as_ref() == key)
            .map(|(_, v)| OsString::from(v.as_ref()))
    }
}

/// `RASH_<name>` if set, else `AUTOSSH_<name>`.
fn dual<E: EnvSource + ?Sized>(env: &E, name: &str) -> Option<OsString> {
    env.var(&format!("RASH_{name}"))
        .or_else(|| env.var(&format!("AUTOSSH_{name}")))
}

/// Environment values must be text; a non-UTF-8 one is a configuration error.
fn as_str(name: &str, v: &OsString) -> Result<String, ConfigError> {
    v.to_str()
        .map(str::to_owned)
        .ok_or_else(|| ConfigError::Invalid(format!("{name} is not valid text")))
}

/// Parse an integer the whole of which must be consumed, as autossh's
/// `strtoul`/`strtol` checks require (`*s == '\0' || *t != '\0'`).
///
/// Unlike autossh this is base 10 only. autossh passes base 0 to `strtoul`, so
/// it reads `-M 020000` as octal; rash reads it as 20000.
fn number<T: std::str::FromStr>(what: &str, s: &str) -> Result<T, ConfigError> {
    s.parse::<T>()
        .map_err(|_| ConfigError::Invalid(format!("invalid {what} \"{s}\"")))
}

/// Resolve the command line and environment into a runnable configuration.
pub fn resolve<E: EnvSource + ?Sized>(
    mut inv: Invocation,
    env: &E,
) -> Result<Resolved, ConfigError> {
    let mut warnings = Vec::new();

    // autossh spells this AUTOSSH_PATH; RASH_PATH would read as an override of
    // $PATH, so rash's alias is the less ambiguous RASH_SSH_PATH.
    let ssh_path = env
        .var("RASH_SSH_PATH")
        .or_else(|| env.var("AUTOSSH_PATH"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SSH_PATH));

    // Logging. AUTOSSH_DEBUG wins over AUTOSSH_LOGLEVEL, as in autossh.c:589-603.
    let mut level = Level::Info;
    let mut also_stderr = false;
    if dual(env, "DEBUG").is_some() {
        level = Level::Debug;
        also_stderr = true;
    } else if let Some(v) = dual(env, "LOGLEVEL") {
        let s = as_str("log level", &v)?;
        let n: u8 = number("log level", &s)?;
        level = Level::from_num(n)
            .ok_or_else(|| ConfigError::Invalid(format!("invalid log level \"{s}\"")))?;
    }
    let target = match dual(env, "LOGFILE") {
        Some(p) => LogTarget::File(PathBuf::from(p)),
        None => LogTarget::Syslog,
    };

    let poll_secs: u64 = match dual(env, "POLL") {
        Some(v) => {
            let s = as_str("poll time", &v)?;
            let n: u64 = number("poll time", &s)?;
            if n == 0 {
                return Err(ConfigError::Invalid(format!("invalid poll time \"{s}\"")));
            }
            n
        }
        None => DEFAULT_POLL,
    };

    // Unless set explicitly the first poll matches the poll time (autossh.c:621-627).
    let mut first_poll_secs: u64 = match dual(env, "FIRST_POLL") {
        Some(v) => {
            let s = as_str("first poll time", &v)?;
            let n: u64 = number("first poll time", &s)?;
            if n == 0 {
                return Err(ConfigError::Invalid(format!(
                    "invalid first poll time \"{s}\""
                )));
            }
            n
        }
        None => poll_secs,
    };
    let mut poll_secs = poll_secs;

    let mut gate_secs: u64 = match dual(env, "GATETIME") {
        Some(v) => {
            let s = as_str("gate time", &v)?;
            let n: i64 = number("gate time", &s)?;
            if n < 0 {
                return Err(ConfigError::Invalid(format!("invalid gate time \"{s}\"")));
            }
            n as u64
        }
        None => DEFAULT_GATE,
    };

    let max_start: i64 = match dual(env, "MAXSTART") {
        Some(v) => {
            let s = as_str("max start number", &v)?;
            let n: i64 = number("max start number", &s)?;
            if n < -1 {
                return Err(ConfigError::Invalid(format!(
                    "invalid max start number \"{s}\""
                )));
            }
            n
        }
        None => -1,
    };

    let message = match dual(env, "MESSAGE") {
        Some(v) => {
            let s = as_str("message", &v)?;
            if s.len() > MAX_MESSAGE {
                return Err(ConfigError::Invalid(format!(
                    "echo message may only be {MAX_MESSAGE} bytes long"
                )));
            }
            s
        }
        None => String::new(),
    };

    let max_lifetime = match dual(env, "MAXLIFETIME") {
        Some(v) => {
            let s = as_str("max lifetime", &v)?;
            let n: u64 = number("max lifetime", &s)?;
            (n > 0).then(|| Duration::from_secs(n))
        }
        None => None,
    };

    // A lifetime shorter than a poll interval would mean never polling at all
    // (autossh.c:661-677).
    if let Some(life) = max_lifetime {
        let life_secs = life.as_secs();
        if poll_secs > life_secs {
            warnings.push(format!(
                "poll time is greater than lifetime, dropping poll time to {life_secs}"
            ));
            poll_secs = life_secs;
        }
        if first_poll_secs > life_secs {
            warnings.push(format!(
                "first poll time is greater than lifetime, dropping first poll time to {life_secs}"
            ));
            first_poll_secs = life_secs;
        }
    }

    let pid_file = dual(env, "PIDFILE")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    // The next three have no autossh counterpart — TOUCH_PIDFILE is a compile-time
    // #define there, and the other two are rash's own — so they are RASH_-only.
    let touch_pid_file = env.var("RASH_TOUCH_PIDFILE").is_some();

    let kill_timeout = match env.var("RASH_KILL_TIMEOUT") {
        Some(v) => {
            let s = as_str("kill timeout", &v)?;
            Duration::from_secs(number("kill timeout", &s)?)
        }
        None => Duration::from_secs(DEFAULT_KILL_TIMEOUT),
    };

    let monitor_host: IpAddr = match env.var("RASH_MONITOR_HOST") {
        Some(v) => {
            let s = as_str("monitor host", &v)?;
            s.parse()
                .map_err(|_| ConfigError::Invalid(format!("invalid monitor host \"{s}\"")))?
        }
        None => IpAddr::V4(Ipv4Addr::LOCALHOST),
    };

    // The monitor port. `--monitor` outranks the environment, which outranks
    // `-M` — the inversion is autossh's own documented behaviour for
    // AUTOSSH_PORT (autossh.c:327-329).
    let env_port = dual(env, "PORT").filter(|v| !v.is_empty());
    let (spec, from_dash_m) = match (inv.monitor_long.clone(), env_port, inv.monitor.clone()) {
        (Some(s), _, _) => (Some(s), false),
        (None, Some(s), _) => (Some(s), false),
        (None, None, Some(s)) => (Some(s), true),
        (None, None, None) => (None, false),
    };
    let spec = spec.ok_or(ConfigError::NoMonitorPort)?;
    let spec = as_str("monitor port", &spec)?;
    let monitor = parse_monitor(&spec, &mut warnings)?;

    // Short poll times need proportionally shorter network timeouts, or a single
    // probe could outlast the interval (autossh.c:391-396).
    let mut net_timeout_ms = DEFAULT_NET_TIMEOUT_MS;
    if poll_secs * 1000 / 2 < net_timeout_ms {
        net_timeout_ms = poll_secs * 1000 / 2;
        warnings.push(format!(
            "short poll time: adjusting net timeouts to {net_timeout_ms}"
        ));
    }

    // Backgrounding means nobody is there to type a passphrase, so the starting
    // gate would only cause a spurious exit (autossh.c:447-458).
    if inv.background {
        gate_secs = 0;
    }

    // autossh injects the forwards where -M stood, but at the very front when the
    // port came from the environment instead (autossh.c:420-427).
    if !from_dash_m {
        inv.inject_at = 0;
    }
    cli::inject_forwards(&mut inv, monitor.forwards(monitor_host));

    Ok(Resolved {
        config: Config {
            ssh_path,
            ssh_args: inv.ssh_args,
            monitor,
            monitor_host,
            poll: Duration::from_secs(poll_secs),
            first_poll: Duration::from_secs(first_poll_secs),
            net_timeout: Duration::from_millis(net_timeout_ms),
            gate_time: Duration::from_secs(gate_secs),
            max_start,
            max_lifetime,
            message,
            pid_file,
            touch_pid_file,
            background: inv.background,
            kill_timeout,
            log: Log {
                target,
                level,
                also_stderr,
            },
            dry_run: inv.dry_run,
        },
        warnings,
    })
}

/// Parse `port`, `port:echo_port`, or `0`.
fn parse_monitor(spec: &str, warnings: &mut Vec<String>) -> Result<Monitor, ConfigError> {
    // autossh splits the echo port off first (autossh.c:343-349).
    let (port_s, echo) = match spec.split_once(':') {
        Some((p, e)) => {
            let n: u32 = number("echo port", e)?;
            if n == 0 || n > u32::from(u16::MAX) {
                return Err(ConfigError::Invalid(format!("invalid echo port  \"{e}\"")));
            }
            (p, Some(n as u16))
        }
        None => (spec, None),
    };

    let port: u32 = number("port", port_s)?;
    if port == 0 {
        warnings.push("port set to 0, monitoring disabled".into());
        return Ok(Monitor::Disabled);
    }
    // The loop mode needs port + 1 as well, so autossh caps both modes at 65534.
    if port > 65534 {
        return Err(ConfigError::Invalid(format!(
            "monitor port ({port}) out of range"
        )));
    }
    let port = port as u16;

    Ok(match echo {
        Some(echo) => Monitor::Echo { port, echo },
        None => Monitor::Loop { port },
    })
}
