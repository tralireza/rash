//! Splitting `argv` into rash's own options and ssh's.
//!
//! autossh validates every argument against a hardcoded `getopt(3)` string
//! (`OPTION_STRING`, autossh.c:112) and prints usage for anything it does not
//! recognise, so every new OpenSSH option breaks it until that string is
//! updated. On OpenSSH 10.3 it already rejects `-B bind_interface` outright and
//! mis-parses `-P tag` as a boolean.
//!
//! rash classifies only what it needs to — `-M`, `-f`, `-V` — and passes
//! everything else through untouched, so an unrecognised option is forwarded to
//! ssh rather than being an error here.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

/// Short options that take a value, from the ssh(1) synopsis (OpenSSH 10.3p1):
///
/// ```text
/// ssh [-46AaCfGgKkMNnqsTtVvXxYy] [-B bind_interface] [-b bind_address]
///     [-c cipher_spec] [-D [bind_address:]port] [-E log_file]
///     [-e escape_char] [-F configfile] [-I pkcs11] [-i identity_file]
///     [-J destination] [-L address] [-l login_name] [-m mac_spec]
///     [-O ctl_cmd] [-o option] [-P tag] [-p port] [-R address]
///     [-S ctl_path] [-T] [-W host:port] [-w local_tun[:remote_tun]]
///     destination [command [argument ...]]
/// ssh [-Q query_option]
/// ```
const SSH_VALUE_OPTS: &[u8] = b"BbcDEeFIiJLlmOoPpQRSWw";

/// Short options that are booleans, from the same synopsis. `1` and `2` are long
/// gone from OpenSSH but are kept so old scripts that still pass them work.
///
/// `f`, `M` and `V` appear here for faithfulness to ssh's grammar; rash
/// intercepts all three before this table is consulted.
const SSH_FLAG_OPTS: &[u8] = b"1246AaCfGgKkMNnqsTtVvXxYy";

/// What rash was asked to do, before any environment or config file is consulted.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Invocation {
    /// Raw `-M` spec, exactly as given. Validated later, by `config`.
    pub monitor: Option<OsString>,
    /// `--monitor SPEC`, which outranks both `-M` and the environment.
    pub monitor_long: Option<OsString>,
    pub background: bool,
    pub version: bool,
    pub help: bool,
    pub dry_run: bool,
    /// `--list`: print the config file's session names and exit.
    pub list: bool,
    /// `--session NAME`: take settings from `[session.NAME]` in the config file.
    pub session: Option<String>,
    /// `--config PATH`: use this config file rather than the default one.
    pub config: Option<PathBuf>,
    /// Arguments for ssh, in order, with `-M`, `-f` and `-V` removed.
    pub ssh_args: Vec<OsString>,
    /// Where the `-L`/`-R` monitor forwards belong: the position `-M` occupied,
    /// or 0 when the port came from the environment instead (autossh parity,
    /// autossh.c:420-427).
    pub inject_at: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    MissingValue(String),
    UnknownLongOption(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue(o) => write!(f, "option {o} requires an argument"),
            Self::UnknownLongOption(o) => write!(f, "unknown option {o}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Split `argv` (excluding argv\[0\]) into rash's options and ssh's.
pub fn parse<I, S>(argv: I) -> Result<Invocation, ParseError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let argv: Vec<OsString> = argv.into_iter().map(Into::into).collect();
    let mut inv = Invocation::default();
    let mut saw_monitor = false;
    let mut after_dashdash = false;
    let mut i = 0;

    while i < argv.len() {
        let bytes = argv[i].as_bytes();

        // Past the separator nothing is rewritten. autossh keeps stripping `f`
        // here (autossh.c:443 runs unconditionally), so `autossh -M 0 host --
        // cmd -flag` hands ssh `-lag`.
        if after_dashdash {
            inv.ssh_args.push(argv[i].clone());
            i += 1;
            continue;
        }

        // The first `--` is rash's own; a second one is passed through.
        if bytes == b"--" {
            after_dashdash = true;
            i += 1;
            continue;
        }

        if bytes.starts_with(b"--") {
            i += parse_long(&mut inv, &argv, i)?;
            continue;
        }

        // A lone `-`, or a token not starting with `-`, is not an option: it is
        // the destination, a remote command word, or a value we did not consume.
        if bytes.len() < 2 || bytes[0] != b'-' {
            inv.ssh_args.push(argv[i].clone());
            i += 1;
            continue;
        }

        let mut kept: Vec<u8> = vec![b'-'];
        let mut detached: Option<OsString> = None;
        let mut consumed_next = false;
        let mut j = 1;

        while j < bytes.len() {
            let c = bytes[j];
            j += 1;

            match c {
                // rash appropriates ssh's `-M` (ControlMaster), as autossh does.
                // Use `-o ControlMaster=yes` if you want ssh's meaning.
                b'M' => {
                    if !saw_monitor {
                        saw_monitor = true;
                        inv.inject_at = inv.ssh_args.len();
                    }
                    let rest = &bytes[j..];
                    if rest.is_empty() {
                        let v = argv
                            .get(i + 1)
                            .ok_or_else(|| ParseError::MissingValue("-M".into()))?;
                        inv.monitor = Some(v.clone());
                        consumed_next = true;
                    } else {
                        inv.monitor = Some(OsString::from_vec(rest.to_vec()));
                    }
                    break;
                }
                b'f' => inv.background = true,
                b'V' => inv.version = true,
                c if SSH_VALUE_OPTS.contains(&c) => {
                    kept.push(c);
                    let rest = &bytes[j..];
                    if rest.is_empty() {
                        if let Some(v) = argv.get(i + 1) {
                            detached = Some(v.clone());
                            consumed_next = true;
                        }
                    } else {
                        kept.extend_from_slice(rest);
                    }
                    break;
                }
                c if SSH_FLAG_OPTS.contains(&c) => kept.push(c),
                // An unknown letter: we cannot know whether it takes a value, so
                // keep it and the whole remainder of the token exactly as given
                // and stop classifying. Letters already stripped were provably
                // option letters, so stripping them stays safe.
                _ => {
                    kept.extend_from_slice(&bytes[j - 1..]);
                    break;
                }
            }
        }

        // A cluster that was entirely stripped leaves a bare `-`; drop it, as
        // autossh does (autossh.c:569-571).
        if kept.len() > 1 {
            inv.ssh_args.push(OsString::from_vec(kept));
        }
        if let Some(v) = detached {
            inv.ssh_args.push(v);
        }

        i += 1 + usize::from(consumed_next);
    }

    Ok(inv)
}

/// Handle one `--long` option. Returns how many argv entries it consumed.
fn parse_long(inv: &mut Invocation, argv: &[OsString], i: usize) -> Result<usize, ParseError> {
    let body = &argv[i].as_bytes()[2..];
    let (name, inline) = match body.iter().position(|&b| b == b'=') {
        Some(p) => (&body[..p], Some(OsString::from_vec(body[p + 1..].to_vec()))),
        None => (body, None),
    };
    let name = String::from_utf8_lossy(name).into_owned();

    match name.as_str() {
        "help" => {
            inv.help = true;
            Ok(1)
        }
        "version" => {
            inv.version = true;
            Ok(1)
        }
        "dry-run" => {
            inv.dry_run = true;
            Ok(1)
        }
        "list" => {
            inv.list = true;
            Ok(1)
        }
        "monitor" => {
            let (v, step) = value_for(&name, inline, argv, i)?;
            inv.monitor_long = Some(v);
            Ok(step)
        }
        "session" => {
            let (v, step) = value_for(&name, inline, argv, i)?;
            inv.session = Some(v.to_string_lossy().into_owned());
            Ok(step)
        }
        "config" => {
            let (v, step) = value_for(&name, inline, argv, i)?;
            inv.config = Some(PathBuf::from(v));
            Ok(step)
        }
        _ => Err(ParseError::UnknownLongOption(format!("--{name}"))),
    }
}

/// Resolve a long option's value from `--name=value` or a following `argv` entry.
fn value_for(
    name: &str,
    inline: Option<OsString>,
    argv: &[OsString],
    i: usize,
) -> Result<(OsString, usize), ParseError> {
    match inline {
        Some(v) => Ok((v, 1)),
        None => argv
            .get(i + 1)
            .map(|v| (v.clone(), 2))
            .ok_or_else(|| ParseError::MissingValue(format!("--{name}"))),
    }
}

/// Splice the monitor forwards into `args` at `at`, clamped to the end.
pub fn splice_forwards(args: &mut Vec<OsString>, at: usize, forwards: Vec<OsString>) {
    let at = at.min(args.len());
    args.splice(at..at, forwards);
}

/// Insert the monitor forwards at the position `-M` occupied.
pub fn inject_forwards(inv: &mut Invocation, forwards: Vec<OsString>) {
    splice_forwards(&mut inv.ssh_args, inv.inject_at, forwards);
}

/// Render an argv the way a shell would need it written, for `--dry-run`.
pub fn quote(arg: &OsStr) -> String {
    let s = arg.to_string_lossy();
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@%+=:,./-_".contains(&b))
    {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}
