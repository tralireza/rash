//! Log sinks: syslog, a file, or stderr.
//!
//! Lines written to a file or to stderr keep autossh's format,
//! `%Y/%m/%d %H:%M:%S rash[pid]: message` (autossh.c:1783-1819), so anything
//! already parsing autossh logs keeps working.
//!
//! The syslog call is always `syslog(level, "%s", msg)`. autossh's fallback path
//! passes the message *as* the format string (autossh.c:1797), which misbehaves
//! on any log line containing a `%`.

use crate::config::{Level, Log, LogTarget};
use std::ffi::CString;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::sync::{Mutex, OnceLock};

static LOGGER: OnceLock<Logger> = OnceLock::new();

struct Logger {
    level: Level,
    also_stderr: bool,
    sink: Mutex<Sink>,
}

enum Sink {
    Syslog,
    File(File),
    Stderr,
}

/// Set up the process-wide sink. Call once, and only after any daemonising fork.
pub fn init(cfg: &Log) -> io::Result<()> {
    let sink = match &cfg.target {
        LogTarget::Syslog => {
            // syslog(3) retains the ident pointer, so it has to be 'static.
            // SAFETY: `c"rash"` is a 'static NUL-terminated string, and the flag
            // and facility constants come from libc.
            unsafe { libc::openlog(c"rash".as_ptr(), libc::LOG_PID, libc::LOG_USER) };
            Sink::Syslog
        }
        LogTarget::File(p) => Sink::File(OpenOptions::new().create(true).append(true).open(p)?),
        LogTarget::Stderr => Sink::Stderr,
    };

    let _ = LOGGER.set(Logger {
        level: cfg.level,
        also_stderr: cfg.also_stderr,
        sink: Mutex::new(sink),
    });
    Ok(())
}

/// Emit one line, if the configured verbosity allows it.
///
/// Prefer the `log_err!` / `log_info!` / `log_debug!` macros.
pub fn emit(level: Level, args: fmt::Arguments<'_>) {
    let Some(logger) = LOGGER.get() else {
        // Errors raised before init() still have to reach someone, exactly as
        // autossh's doerrlog() falls back to stderr.
        let _ = writeln!(io::stderr(), "rash: {args}");
        return;
    };

    // Higher numbers are chattier, so a level above the configured one is
    // filtered out (autossh.c:1793).
    if level > logger.level {
        return;
    }

    let msg = args.to_string();
    // A poisoned mutex only means some other thread panicked mid-log; the sink
    // itself is still usable, and dropping log lines would be worse.
    let mut sink = logger.sink.lock().unwrap_or_else(|e| e.into_inner());

    match &mut *sink {
        Sink::Syslog => {
            if let Ok(c) = CString::new(msg.as_bytes()) {
                // SAFETY: both pointers are valid NUL-terminated strings that
                // outlive the call. The message is an argument to "%s", never
                // the format string itself.
                unsafe { libc::syslog(level as i32, c"%s".as_ptr(), c.as_ptr()) };
            }
        }
        Sink::File(f) => {
            let _ = writeln!(f, "{} {msg}", prefix());
            let _ = f.flush();
        }
        Sink::Stderr => {
            let _ = writeln!(io::stderr(), "{} {msg}", prefix());
        }
    }

    // AUTOSSH_DEBUG mirrors everything to stderr as well.
    if logger.also_stderr && !matches!(&*sink, Sink::Stderr) {
        let _ = writeln!(io::stderr(), "{} {msg}", prefix());
    }
}

fn prefix() -> String {
    format!("{} rash[{}]:", timestamp(), std::process::id())
}

/// Local time as `%Y/%m/%d %H:%M:%S`, matching autossh's `timestr()`.
fn timestamp() -> String {
    let mut buf = [0u8; 32];

    // SAFETY: `time(NULL)` is always valid; `tm` is a plain C struct that
    // localtime_r fills in, and a zeroed one is a valid starting value.
    // strftime writes at most `buf.len()` bytes, including the NUL.
    let n = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return String::new();
        }
        libc::strftime(
            buf.as_mut_ptr().cast(),
            buf.len(),
            c"%Y/%m/%d %H:%M:%S".as_ptr(),
            &tm,
        )
    };

    String::from_utf8_lossy(&buf[..n]).into_owned()
}

#[macro_export]
macro_rules! logmsg {
    ($level:expr, $($arg:tt)*) => {
        $crate::log::emit($level, ::core::format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_err {
    ($($arg:tt)*) => { $crate::logmsg!($crate::config::Level::Err, $($arg)*) };
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::logmsg!($crate::config::Level::Info, $($arg)*) };
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => { $crate::logmsg!($crate::config::Level::Debug, $($arg)*) };
}
