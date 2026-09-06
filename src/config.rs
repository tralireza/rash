//! Resolving a complete configuration from the command line and the environment.
//!
//! This is a pure function: it reads no files, opens no sockets, and logs
//! nothing. Anything worth telling the user about comes back as a warning for
//! the caller to emit once a log sink exists.

use crate::cli::{self, Invocation};
use crate::settings::{self, Section};
use std::ffi::OsString;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
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
    /// `-M unix`: the same loop, over UNIX-domain sockets. No ports to pick and
    /// none to collide, at either end.
    Unix,
}

/// Where the UNIX-domain monitor's sockets live, once resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnixPaths {
    /// ssh's `-L` listener. rash connects here to send a probe, and unlinks it
    /// before each start so a leftover cannot stop ssh binding it.
    pub local_out: PathBuf,
    /// rash's own listener, where the probe arrives back.
    pub local_in: PathBuf,
    /// Directory on the remote in which sshd binds the `-R` socket. The file
    /// name within it changes on every start.
    pub remote_dir: PathBuf,
}

impl Monitor {
    /// The ssh arguments that build the monitor path (§1.3 of the plan).
    ///
    /// `unix` and `remote_sock` are consulted only for [`Monitor::Unix`], where
    /// the remote path is different on every ssh start.
    pub fn forwards(
        &self,
        host: IpAddr,
        unix: Option<&UnixPaths>,
        remote_sock: &Path,
    ) -> Vec<OsString> {
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
            // -L local_socket:remote_socket and -R remote_socket:local_socket,
            // both documented in ssh(1).
            Self::Unix => match unix {
                Some(u) => vec![
                    "-L".into(),
                    sock_pair(&u.local_out, remote_sock),
                    "-R".into(),
                    sock_pair(remote_sock, &u.local_in),
                ],
                None => vec![],
            },
        }
    }

    /// The port rash writes its probe to, if it uses one.
    pub fn write_port(&self) -> Option<u16> {
        match *self {
            Self::Disabled | Self::Unix => None,
            Self::Loop { port } | Self::Echo { port, .. } => Some(port),
        }
    }

    /// The port rash listens on for the probe to come back, if it uses one.
    pub fn read_port(&self) -> Option<u16> {
        match *self {
            Self::Loop { port } => Some(port + 1),
            Self::Disabled | Self::Echo { .. } | Self::Unix => None,
        }
    }
}

/// `a:b`, built without going through `str` so a non-UTF-8 path survives.
fn sock_pair(a: &Path, b: &Path) -> OsString {
    let mut s = OsString::from(a);
    s.push(":");
    s.push(b);
    s
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
            Self::Unix => write!(f, "loop over UNIX sockets"),
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
    /// A syslog number, as `AUTOSSH_LOGLEVEL` takes, or a level name.
    fn parse(s: &str) -> Option<Self> {
        if let Ok(n) = s.parse::<u8>() {
            return Self::from_num(n);
        }
        Some(match s.to_ascii_lowercase().as_str() {
            "emerg" => Self::Emerg,
            "alert" => Self::Alert,
            "crit" => Self::Crit,
            "err" | "error" => Self::Err,
            "warning" | "warn" => Self::Warning,
            "notice" => Self::Notice,
            "info" => Self::Info,
            "debug" => Self::Debug,
            _ => return None,
        })
    }

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

/// How a log line is shaped. Orthogonal to where it goes, except for syslog,
/// which has its own structure and always gets plain text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Text => "text",
            Self::Json => "json",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Log {
    pub target: LogTarget,
    pub format: Format,
    pub level: Level,
    /// `AUTOSSH_DEBUG` also mirrors syslog output to stderr (`LOG_PERROR`).
    pub also_stderr: bool,
}

/// Everything rash needs in order to run, fully resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub ssh_path: PathBuf,
    /// ssh's argv\[1..\] as the user wrote it, *without* the monitor forwards.
    /// Use [`Config::ssh_argv`] to get what actually gets executed — the
    /// forwards are built fresh on every start, because the UNIX arrangement
    /// needs a different remote socket path each time.
    pub ssh_args: Vec<OsString>,
    /// Where the forwards belong: the position `-M` occupied, or 0 when the
    /// port came from the environment (autossh.c:420-427).
    pub inject_at: usize,
    pub monitor: Monitor,
    pub monitor_host: IpAddr,
    /// Socket paths, resolved only when the monitor is [`Monitor::Unix`].
    pub unix: Option<UnixPaths>,
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

impl Config {
    /// What actually gets executed: the user's arguments with `forwards`
    /// spliced in where `-M` stood.
    pub fn ssh_argv(&self, forwards: Vec<OsString>) -> Vec<OsString> {
        let mut argv = self.ssh_args.clone();
        cli::splice_forwards(&mut argv, self.inject_at, forwards);
        argv
    }
}

/// The shape of the per-start remote socket name, for `--dry-run`. The literal
/// placeholder is the point: the real name is different on every ssh start.
pub fn remote_sock_example(unix: &UnixPaths) -> PathBuf {
    unix.remote_dir.join("rash-<nonce>.sock")
}

/// `sun_path` holds 104 bytes on macOS and 108 on Linux, NUL included. Leave
/// room rather than sitting on the limit.
const SUN_PATH_MAX: usize = 96;

/// Where to look for a config file, in order of preference:
///
/// 1. `~/.rash.toml`, for a single file you can keep next to your dotfiles
/// 2. `$XDG_CONFIG_HOME/rash/config.toml`, or `~/.config/rash/config.toml`
///
/// The caller takes the first that exists. Returning the candidates rather than
/// resolving them here keeps this module free of filesystem access, so
/// [`resolve_with`] stays testable without one.
pub fn config_file_candidates<E: EnvSource + ?Sized>(env: &E) -> Vec<PathBuf> {
    let home = env.var("HOME").map(PathBuf::from).unwrap_or_default();

    let xdg = match env.var("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => home.join(".config"),
    };

    vec![
        home.join(".rash.toml"),
        xdg.join("rash").join("config.toml"),
    ]
}

/// Where the UNIX monitor's local sockets live.
///
/// `$XDG_RUNTIME_DIR` when it is set, else `/tmp/rash-<uid>`. Deliberately not
/// `$TMPDIR`, which on macOS is a long `/var/folders/...` path that would eat
/// most of the `sun_path` budget on its own.
fn socket_dir<E: EnvSource + ?Sized>(env: &E) -> PathBuf {
    if let Some(d) = env.var("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    // SAFETY: getuid always succeeds and reads no memory.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/rash-{uid}"))
}

/// Resolve the local socket paths for the UNIX monitor.
///
/// The names carry the pid, so this has to be redone after a daemonising fork
/// or the sockets are named after a process that no longer exists. `main` does
/// exactly that, for the same reason it writes the pid file after the fork.
pub fn unix_paths<E: EnvSource + ?Sized>(env: &E) -> Result<UnixPaths, ConfigError> {
    let dir = match env.var("RASH_SOCKET_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => socket_dir(env),
    };
    let remote_dir = match env.var("RASH_REMOTE_SOCKET_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from("/tmp"),
    };

    let pid = std::process::id();
    let paths = UnixPaths {
        local_out: dir.join(format!("rash-{pid}-out.sock")),
        local_in: dir.join(format!("rash-{pid}-in.sock")),
        remote_dir,
    };

    // Over-long paths fail at bind time with a baffling error from deep inside
    // the socket layer, so complain here where the cause is obvious. The remote
    // name is generated per start, so check a representative one.
    check_sun_path(&paths.local_out)?;
    check_sun_path(&paths.local_in)?;
    check_sun_path(&paths.remote_dir.join("rash-0123456789abcdef.sock"))?;

    Ok(paths)
}

fn check_sun_path(p: &Path) -> Result<(), ConfigError> {
    let len = p.as_os_str().as_encoded_bytes().len();
    if len > SUN_PATH_MAX {
        return Err(ConfigError::Invalid(format!(
            "socket path is {len} bytes, over rash's {SUN_PATH_MAX}-byte limit \
             (the kernel's sun_path holds 104 on macOS, 108 on Linux): {}",
            p.display()
        )));
    }
    Ok(())
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

/// Parse a rash-only boolean variable.
///
/// `AUTOSSH_DEBUG` is deliberately not routed through here: autossh treats it
/// as set-or-not and rash matches that. rash's own switches read their value,
/// so `RASH_TOUCH_PIDFILE=0` means off rather than a surprising on.
fn boolean(what: &str, v: &OsString) -> Result<bool, ConfigError> {
    let s = as_str(what, v)?;
    match s.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" => Ok(false),
        "1" | "true" | "yes" | "on" => Ok(true),
        _ => Err(ConfigError::Invalid(format!(
            "invalid {what} \"{s}\", expected 1/0, true/false, yes/no, or on/off"
        ))),
    }
}

/// Resolve the command line and environment into a runnable configuration,
/// with no config file involved.
pub fn resolve<E: EnvSource + ?Sized>(inv: Invocation, env: &E) -> Result<Resolved, ConfigError> {
    resolve_with(inv, env, &settings::File::default())
}

/// Resolve, consulting a config file as the bottom layer.
///
/// Precedence, highest first: a long flag, `RASH_*`, `AUTOSSH_*`, the named
/// `[session.<name>]`, `[defaults]`, then the built-in default. The monitor
/// port inverts the top two rungs, because autossh documents `AUTOSSH_PORT` as
/// overriding `-M`.
pub fn resolve_with<E: EnvSource + ?Sized>(
    mut inv: Invocation,
    env: &E,
    file: &settings::File,
) -> Result<Resolved, ConfigError> {
    let mut warnings = Vec::new();
    let section: Section = file
        .section(inv.session.as_deref())
        .map_err(ConfigError::Invalid)?;

    // autossh spells this AUTOSSH_PATH; RASH_PATH would read as an override of
    // $PATH, so rash's alias is the less ambiguous RASH_SSH_PATH.
    let ssh_path = env
        .var("RASH_SSH_PATH")
        .or_else(|| env.var("AUTOSSH_PATH"))
        .map(PathBuf::from)
        .or_else(|| section.ssh_path.clone())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SSH_PATH));

    // Logging. AUTOSSH_DEBUG wins over AUTOSSH_LOGLEVEL, as in autossh.c:589-603.
    let mut level = Level::Info;
    let mut also_stderr = false;
    if dual(env, "DEBUG").is_some() {
        level = Level::Debug;
        also_stderr = true;
    } else {
        let spec = match dual(env, "LOGLEVEL") {
            Some(v) => Some(as_str("log level", &v)?),
            None => section.loglevel.clone(),
        };
        if let Some(s) = spec {
            level = Level::parse(&s)
                .ok_or_else(|| ConfigError::Invalid(format!("invalid log level \"{s}\"")))?;
        }
    }

    // AUTOSSH_LOGFILE is always a path; RASH_LOG and the config file also take
    // the keywords `syslog` and `stderr`.
    let log_spec = match env.var("RASH_LOG") {
        Some(v) => Some(as_str("log target", &v)?),
        None => match dual(env, "LOGFILE") {
            Some(v) => Some(as_str("log file", &v)?),
            None => section.log.clone(),
        },
    };
    let target = match log_spec.as_deref() {
        None | Some("syslog") => LogTarget::Syslog,
        Some("stderr") => LogTarget::Stderr,
        Some(p) => LogTarget::File(PathBuf::from(p)),
    };

    let fmt_spec = match env.var("RASH_LOG_FORMAT") {
        Some(v) => Some(as_str("log format", &v)?),
        None => section.log_format.clone(),
    };
    let format = match fmt_spec.as_deref() {
        None | Some("text") => Format::Text,
        Some("json") => Format::Json,
        Some(other) => {
            return Err(ConfigError::Invalid(format!(
                "invalid log format \"{other}\", expected text or json"
            )));
        }
    };

    let mut poll_secs: u64 = match dual(env, "POLL") {
        Some(v) => {
            let s = as_str("poll time", &v)?;
            let n: u64 = number("poll time", &s)?;
            if n == 0 {
                return Err(ConfigError::Invalid(format!("invalid poll time \"{s}\"")));
            }
            n
        }
        None => section.poll.unwrap_or(DEFAULT_POLL),
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
        None => section.first_poll.unwrap_or(poll_secs),
    };

    let mut gate_secs: u64 = match dual(env, "GATETIME") {
        Some(v) => {
            let s = as_str("gate time", &v)?;
            let n: i64 = number("gate time", &s)?;
            if n < 0 {
                return Err(ConfigError::Invalid(format!("invalid gate time \"{s}\"")));
            }
            n as u64
        }
        None => section.gatetime.unwrap_or(DEFAULT_GATE),
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
        None => section.maxstart.unwrap_or(-1),
    };

    let message = match dual(env, "MESSAGE") {
        Some(v) => as_str("message", &v)?,
        None => section.message.clone().unwrap_or_default(),
    };
    if message.len() > MAX_MESSAGE {
        return Err(ConfigError::Invalid(format!(
            "echo message may only be {MAX_MESSAGE} bytes long"
        )));
    }

    let lifetime_secs = match dual(env, "MAXLIFETIME") {
        Some(v) => {
            let s = as_str("max lifetime", &v)?;
            number::<u64>("max lifetime", &s)?
        }
        None => section.maxlifetime.unwrap_or(0),
    };
    let max_lifetime = (lifetime_secs > 0).then(|| Duration::from_secs(lifetime_secs));

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
        .map(PathBuf::from)
        .or_else(|| section.pidfile.clone());
    // The next three have no autossh counterpart — TOUCH_PIDFILE is a compile-time
    // #define there, and the other two are rash's own — so they are RASH_-only.
    let touch_pid_file = match env.var("RASH_TOUCH_PIDFILE") {
        Some(v) => boolean("touch pidfile", &v)?,
        None => false,
    };

    let kill_timeout = match env.var("RASH_KILL_TIMEOUT") {
        Some(v) => {
            let s = as_str("kill timeout", &v)?;
            Duration::from_secs(number("kill timeout", &s)?)
        }
        None => Duration::from_secs(section.kill_timeout.unwrap_or(DEFAULT_KILL_TIMEOUT)),
    };

    let monitor_host: IpAddr = match env.var("RASH_MONITOR_HOST") {
        Some(v) => {
            let s = as_str("monitor host", &v)?;
            s.parse()
                .map_err(|_| ConfigError::Invalid(format!("invalid monitor host \"{s}\"")))?
        }
        None => match &section.monitor_host {
            Some(s) => s
                .parse()
                .map_err(|_| ConfigError::Invalid(format!("invalid monitor host \"{s}\"")))?,
            None => IpAddr::V4(Ipv4Addr::LOCALHOST),
        },
    };

    // The monitor port. `--monitor` outranks the environment, which outranks
    // `-M` — the inversion is autossh's own documented behaviour for
    // AUTOSSH_PORT (autossh.c:327-329).
    let env_port = dual(env, "PORT").filter(|v| !v.is_empty());
    let (spec, from_dash_m) = match (inv.monitor_long.clone(), env_port, inv.monitor.clone()) {
        (Some(s), _, _) => (Some(s), false),
        (None, Some(s), _) => (Some(s), false),
        (None, None, Some(s)) => (Some(s), true),
        (None, None, None) => (
            section
                .monitor
                .as_ref()
                .map(|m| OsString::from(m.as_text())),
            false,
        ),
    };
    let spec = spec.ok_or(ConfigError::NoMonitorPort)?;
    let spec = as_str("monitor port", &spec)?;
    let monitor = parse_monitor(&spec, &mut warnings)?;

    // A named session can carry the ssh arguments too, so `rash --session x`
    // needs nothing else on the command line.
    if inv.ssh_args.is_empty()
        && let Some(args) = &section.ssh_args
    {
        inv.ssh_args = args.iter().map(OsString::from).collect();
    }

    // Short poll times need proportionally shorter network timeouts, or a single
    // probe could outlast the interval (autossh.c:391-396).
    //
    // Saturating, because the poll time is an unbounded u64 straight from the
    // user: `AUTOSSH_POLL=18446744073709552` overflows the multiply, which
    // panics in a debug build and — far worse — silently wraps to a 192ms
    // timeout in a release one.
    let mut net_timeout_ms = DEFAULT_NET_TIMEOUT_MS;
    let half_poll_ms = poll_secs.saturating_mul(1000) / 2;
    if half_poll_ms < net_timeout_ms {
        net_timeout_ms = half_poll_ms;
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

    let unix = match monitor {
        Monitor::Unix => Some(unix_paths(env)?),
        _ => None,
    };

    Ok(Resolved {
        config: Config {
            ssh_path,
            ssh_args: inv.ssh_args,
            inject_at: inv.inject_at,
            monitor,
            monitor_host,
            unix,
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
                format,
                level,
                also_stderr,
            },
            dry_run: inv.dry_run,
        },
        warnings,
    })
}

/// Parse `port`, `port:echo_port`, `0`, or `unix`.
fn parse_monitor(spec: &str, warnings: &mut Vec<String>) -> Result<Monitor, ConfigError> {
    // rash's own: the same loop, but over UNIX-domain sockets.
    if spec.eq_ignore_ascii_case("unix") {
        return Ok(Monitor::Unix);
    }

    // autossh splits the echo port off first (autossh.c:343-349).
    let (port_s, echo) = match spec.split_once(':') {
        Some((p, e)) => {
            let n: u32 = number("echo port", e)?;
            if n == 0 || n > u32::from(u16::MAX) {
                // One space. autossh.c:348 has two — "invalid echo port  \"%s\"" —
                // and this is a deliberate divergence from it, listed with the
                // others in rash(1). Nothing can be relying on the spacing: it
                // is a startup rejection written to stderr before a log sink
                // even exists, so no log parser ever sees it, and rash exits
                // non-zero either way.
                return Err(ConfigError::Invalid(format!("invalid echo port \"{e}\"")));
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
