//! The probe payload and the exchange that carries it.
//!
//! The message is something identifiable as ours (autossh.c:1301-1311), so a
//! stray process writing to the monitor port cannot be mistaken for a healthy
//! tunnel. A probe succeeds only when the bytes that come back are byte-for-byte
//! the bytes that went out.

use crate::config::Config;
use std::fs::File;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Build one probe message: `hostname rash pid nonce message\r\n`.
pub fn message(cfg: &Config) -> Vec<u8> {
    format!(
        "{} rash {} {} {}\r\n",
        hostname(),
        std::process::id(),
        nonce(),
        cfg.message
    )
    .into_bytes()
}

/// Send `msg` on `w`, read the same number of bytes from `r`, and say whether
/// they match.
///
/// autossh hand-rolls this over `poll()` with a "too many loops without data"
/// guard for the case where traffic is black-holed and `poll()` cannot tell
/// (autossh.c:1517-1529). The caller here wraps the whole exchange in a timeout,
/// which covers that case without the heuristic.
pub async fn exchange<W, R>(w: &mut W, r: &mut R, msg: &[u8]) -> bool
where
    W: AsyncWriteExt + Unpin,
    R: AsyncReadExt + Unpin,
{
    if w.write_all(msg).await.is_err() || w.flush().await.is_err() {
        return false;
    }

    let mut back = vec![0u8; msg.len()];
    if r.read_exact(&mut back).await.is_err() {
        return false;
    }

    back == msg
}

/// This machine's name, as `uname(2)` reports it — the same source autossh uses.
fn hostname() -> String {
    // SAFETY: uname fills a caller-provided struct, and a zeroed `utsname` is a
    // valid thing to hand it.
    let uts = unsafe {
        let mut uts: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut uts) != 0 {
            return String::new();
        }
        uts
    };

    // `c_char` is i8 on x86-64 and on every Apple target, but u8 on aarch64
    // Linux, where clippy would otherwise call this cast unnecessary. It is
    // needed on the platforms where it is needed.
    #[allow(clippy::unnecessary_cast)]
    let bytes: Vec<u8> = uts
        .nodename
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 64 bits of randomness, so two probes are never confused for one another.
///
/// autossh seeds `random()` with `pid ^ tv_usec ^ tv_sec` (autossh.c:721-722),
/// which is guessable; `/dev/urandom` costs nothing here.
pub(crate) fn nonce() -> u64 {
    let mut buf = [0u8; 8];
    if let Ok(mut f) = File::open("/dev/urandom")
        && f.read_exact(&mut buf).is_ok()
    {
        return u64::from_ne_bytes(buf);
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    nanos ^ u64::from(std::process::id())
}
