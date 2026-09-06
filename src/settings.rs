//! The optional TOML configuration file.
//!
//! Nothing here is required: rash works entirely from the command line and the
//! environment, exactly as autossh does. The file exists so a tunnel you run
//! often can be named once and started with `rash --session homelab`.
//!
//! ```toml
//! [defaults]
//! poll = 600
//! gatetime = 30
//!
//! [session.homelab]
//! monitor  = 20000                                    # or "20000:7", "unix", 0
//! ssh_args = ["-N", "-R", "2200:localhost:22", "me@host"]
//! poll     = 300
//! message  = "homelab"
//! ```
//!
//! Everything a section can set is also settable by flag or environment
//! variable, and those win — the file is the lowest layer of the precedence
//! stack, above only the built-in defaults.
//!
//! # Key names
//!
//! A key is its environment variable with the `AUTOSSH_`/`RASH_` prefix removed
//! and lowercased, which is why `gatetime` and `maxlifetime` run together while
//! `first_poll` and `kill_timeout` do not — `AUTOSSH_GATETIME` has no
//! underscore and `AUTOSSH_FIRST_POLL` does. It looks inconsistent and is not:
//! do not "tidy" one group to match the other, or every key becomes a guess.
//! `ssh_args` is the sole exception, having no variable of its own.
//!
//! `AUTOSSH_DEBUG` and `RASH_TOUCH_PIDFILE` deliberately have no key here: both
//! are switches for a single run, not settings for a tunnel.
//!
//! `deny_unknown_fields` is on, so a mistyped key is a hard error rather than a
//! setting that silently does nothing. That makes the list in rash(1) the
//! contract; keep the two in step.

use serde::de::{self, Unexpected, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// A parsed config file.
#[derive(Debug, Default, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct File {
    #[serde(default)]
    pub defaults: Section,
    /// Written as `[session.<name>]`.
    #[serde(default, rename = "session")]
    pub sessions: BTreeMap<String, Section>,
}

/// A monitor spec, which TOML may express as a bare integer or as a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spec {
    Port(u32),
    Text(String),
}

impl Spec {
    pub fn as_text(&self) -> String {
        match self {
            Self::Port(n) => n.to_string(),
            Self::Text(s) => s.clone(),
        }
    }
}

/// Hand-written so a bad value says what was wanted.
///
/// `#[serde(untagged)]` gets the parsing right but reports a failure as "data
/// did not match any variant of untagged enum Spec", which tells a user nothing
/// about what to write instead. Every other error rash produces is a sentence.
impl<'de> Deserialize<'de> for Spec {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct SpecVisitor;

        impl Visitor<'_> for SpecVisitor {
            type Value = Spec;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a monitor port, or \"port:echo_port\", \"unix\", or 0")
            }

            fn visit_u64<E: de::Error>(self, n: u64) -> Result<Spec, E> {
                u32::try_from(n)
                    .map(Spec::Port)
                    .map_err(|_| E::invalid_value(Unexpected::Unsigned(n), &self))
            }

            fn visit_i64<E: de::Error>(self, n: i64) -> Result<Spec, E> {
                u32::try_from(n)
                    .map(Spec::Port)
                    .map_err(|_| E::invalid_value(Unexpected::Signed(n), &self))
            }

            fn visit_str<E: de::Error>(self, s: &str) -> Result<Spec, E> {
                Ok(Spec::Text(s.to_owned()))
            }
        }

        de.deserialize_any(SpecVisitor)
    }
}

/// One `[defaults]` or `[session.<name>]` block. Every field is optional; an
/// absent one simply defers to the layer below.
#[derive(Debug, Default, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub monitor: Option<Spec>,
    pub ssh_args: Option<Vec<String>>,
    pub ssh_path: Option<PathBuf>,
    pub poll: Option<u64>,
    pub first_poll: Option<u64>,
    pub gatetime: Option<u64>,
    pub maxstart: Option<i64>,
    pub maxlifetime: Option<u64>,
    pub message: Option<String>,
    pub pidfile: Option<PathBuf>,
    pub monitor_host: Option<String>,
    pub kill_timeout: Option<u64>,
    /// `syslog`, `stderr`, or a path to a file.
    pub log: Option<String>,
    /// `text` or `json`.
    pub log_format: Option<String>,
    /// A syslog level name, or the numbers 0-7 as a string.
    pub loglevel: Option<String>,
}

impl File {
    /// The effective section: `[defaults]` with the named session laid over it.
    pub fn section(&self, session: Option<&str>) -> Result<Section, String> {
        let mut merged = self.defaults.clone();
        if let Some(name) = session {
            let Some(s) = self.sessions.get(name) else {
                return Err(format!(
                    "no session named \"{name}\" in the config file{}",
                    self.hint()
                ));
            };
            merged.overlay(s);
        }
        Ok(merged)
    }

    /// The session names, sorted.
    pub fn session_names(&self) -> Vec<&str> {
        self.sessions.keys().map(String::as_str).collect()
    }

    fn hint(&self) -> String {
        let names = self.session_names();
        if names.is_empty() {
            " (it defines none)".to_owned()
        } else {
            format!(" (it has: {})", names.join(", "))
        }
    }
}

impl Section {
    /// Take every value `other` sets, leaving the rest alone.
    fn overlay(&mut self, other: &Section) {
        macro_rules! take {
            ($($field:ident),* $(,)?) => {
                $( if other.$field.is_some() { self.$field = other.$field.clone(); } )*
            };
        }
        take!(
            monitor,
            ssh_args,
            ssh_path,
            poll,
            first_poll,
            gatetime,
            maxstart,
            maxlifetime,
            message,
            pidfile,
            monitor_host,
            kill_timeout,
            log,
            log_format,
            loglevel,
        );
    }
}

/// Parse config text. Exposed so a caller need not take a TOML dependency of
/// its own just to build a `File`.
pub fn parse(text: &str) -> Result<File, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

/// Read a config file. A missing file is not an error — most runs have none.
pub fn load(path: &Path) -> Result<File, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(File::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}
