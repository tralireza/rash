//! Testing that the tunnel still carries traffic.
//!
//! Three arrangements, all set up by the `-L`/`-R` forwards this module hands
//! to each ssh start:
//!
//! * **Loop** — data written to the local monitor port travels to the remote,
//!   where the matching `-R` forward sends it straight back to a second local
//!   port that rash is listening on.
//! * **Echo** — data written to the local monitor port reaches an echo service
//!   on the remote, which returns it over the same connection.
//! * **Unix** — the loop again, but over UNIX-domain sockets, so there are no
//!   ports to choose and none to collide at either end.
//!
//! The listening socket is opened once and held for the life of the process, as
//! autossh does (autossh.c:465-472).

pub mod probe;

use crate::config::{Config, Monitor as Spec, UnixPaths};
use crate::{log_debug, log_info};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::time::timeout;

/// `MAX_CONN_TRIES`, autossh.c:96.
///
/// autossh configures 3 but performs 2: its loop is `while (tries++ < 3)` with
/// an immediate `if (tries >= 3) break` at the top (autossh.c:1285-1292). rash
/// makes all three attempts.
const MAX_TRIES: u32 = 3;

/// How long to pause between probe attempts, as a fraction of the net timeout.
///
/// Without a pause the retries are worthless in the case that most needs them:
/// a refused connection fails in microseconds, so three attempts finish in less
/// time than one round trip and nothing transient has a chance to clear. Scaled
/// rather than fixed so a short poll interval keeps a short total probe budget,
/// and capped so a long one does not sit idle.
const RETRY_PAUSE_DIVISOR: u32 = 10;
const RETRY_PAUSE_MAX: Duration = Duration::from_secs(1);

/// Whatever the monitor listens on, held for the whole run.
enum Inbound {
    Tcp(TcpListener),
    Unix(UnixListener),
}

/// The live monitor.
pub struct Monitor {
    spec: Spec,
    host: IpAddr,
    unix: Option<UnixPaths>,
    inbound: Option<Inbound>,
}

impl Monitor {
    /// Open the listening socket, if this arrangement needs one.
    ///
    /// A failure to bind is fatal, exactly as in autossh: without the listener
    /// the loop can never complete and every probe would fail.
    pub async fn bind(cfg: &Config) -> io::Result<Self> {
        let inbound = match (cfg.monitor, cfg.unix.as_ref()) {
            (Spec::Loop { port }, _) => {
                let addr = SocketAddr::new(cfg.monitor_host, port + 1);
                // Rust opens sockets with SOCK_CLOEXEC, which is what keeps the
                // forked ssh from inheriting this listener and holding the port
                // open after rash is gone. autossh sets FD_CLOEXEC by hand
                // (autossh.c:469).
                let listener = TcpListener::bind(addr).await?;
                log_debug!("monitor listening on {addr}");
                Some(Inbound::Tcp(listener))
            }
            (Spec::Unix, Some(paths)) => {
                prepare_socket_dir(&paths.local_in)?;
                // A socket file left by a previous run would stop the bind.
                let _ = fs::remove_file(&paths.local_in);
                let listener = UnixListener::bind(&paths.local_in)?;
                log_debug!("monitor listening on {}", paths.local_in.display());
                Some(Inbound::Unix(listener))
            }
            _ => None,
        };

        Ok(Self {
            spec: cfg.monitor,
            host: cfg.monitor_host,
            unix: cfg.unix.clone(),
            inbound,
        })
    }

    pub fn enabled(&self) -> bool {
        !matches!(self.spec, Spec::Disabled)
    }

    /// The descriptor the monitor listens on, if it has one.
    ///
    /// Exposed so a test can assert it is close-on-exec: were the forked ssh to
    /// inherit it, the port or socket would stay held after rash exited.
    pub fn listener_fd(&self) -> Option<RawFd> {
        match &self.inbound {
            Some(Inbound::Tcp(l)) => Some(l.as_raw_fd()),
            Some(Inbound::Unix(l)) => Some(l.as_raw_fd()),
            None => None,
        }
    }

    /// The forwards for the next ssh start.
    ///
    /// For the UNIX arrangement the remote socket path is different every time.
    /// A socket left behind on the remote by an unclean disconnect would stop
    /// sshd binding it, and `StreamLocalBindUnlink` defaults to `no` on the
    /// server — which a client cannot override. Without a fresh path, rash
    /// would restart for ever against a forward that could never come up.
    pub fn next_forwards(&self) -> Vec<OsString> {
        // ssh binds the outbound socket itself, so clear any leftover first.
        if let Some(u) = &self.unix {
            let _ = fs::remove_file(&u.local_out);
        }

        let remote = self.remote_sock();
        self.spec.forwards(self.host, self.unix.as_ref(), &remote)
    }

    fn remote_sock(&self) -> PathBuf {
        match &self.unix {
            Some(u) => u
                .remote_dir
                .join(format!("rash-{:016x}.sock", probe::nonce())),
            None => PathBuf::new(),
        }
    }

    /// Send a probe and wait for it to come back. `false` means the tunnel is
    /// not carrying traffic and ssh should be restarted.
    pub async fn probe(&self, cfg: &Config) -> bool {
        let pause = (cfg.net_timeout / RETRY_PAUSE_DIVISOR).min(RETRY_PAUSE_MAX);

        for attempt in 1..=MAX_TRIES {
            if self.attempt(cfg).await {
                log_debug!("connection ok");
                return true;
            }
            log_debug!("monitor attempt {attempt} of {MAX_TRIES} failed");
            if attempt < MAX_TRIES {
                tokio::time::sleep(pause).await;
            }
        }
        log_info!("tried connection {MAX_TRIES} times and failed");
        false
    }

    /// One probe, on connections of its own.
    ///
    /// autossh opens the write connection once and reuses it across retries
    /// while re-accepting the read side (autossh.c:1279-1299), which cannot
    /// work in loop mode: the remote end is still bound to the read connection
    /// that was just closed, so the retry waits for an accept that never comes.
    /// A fresh pair per attempt keeps the two sides paired.
    async fn attempt(&self, cfg: &Config) -> bool {
        let net = cfg.net_timeout;
        let msg = probe::message(cfg);

        match self.spec {
            Spec::Disabled => true,

            Spec::Echo { port, .. } => {
                let addr = SocketAddr::new(self.host, port);
                let Some(mut s) = connect_tcp(addr, net).await else {
                    return false;
                };
                let (mut r, mut w) = s.split();
                matches!(
                    timeout(net, probe::exchange(&mut w, &mut r, &msg)).await,
                    Ok(true)
                )
            }

            Spec::Loop { port } => {
                let Some(Inbound::Tcp(listener)) = &self.inbound else {
                    return false;
                };
                let addr = SocketAddr::new(self.host, port);
                let Some(mut w) = connect_tcp(addr, net).await else {
                    return false;
                };
                // Waiting for the accept before writing is safe because the
                // forward cascade is triggered by the connection, not by the
                // data: connecting to the local port makes ssh open a channel,
                // the remote's -R listener accept it, and a connection come
                // back to us. autossh relies on the same ordering — its poll
                // for an accept precedes any send (autossh.c:1320-1337).
                let Some(mut r) = accept_tcp(listener, net).await else {
                    return false;
                };
                matches!(
                    timeout(net, probe::exchange(&mut w, &mut r, &msg)).await,
                    Ok(true)
                )
            }

            Spec::Unix => {
                let (Some(u), Some(Inbound::Unix(listener))) = (&self.unix, &self.inbound) else {
                    return false;
                };
                let Some(mut w) = connect_unix(&u.local_out, net).await else {
                    return false;
                };
                let Some(mut r) = accept_unix(listener, net).await else {
                    return false;
                };
                matches!(
                    timeout(net, probe::exchange(&mut w, &mut r, &msg)).await,
                    Ok(true)
                )
            }
        }
    }
}

impl Drop for Monitor {
    /// Take the sockets away on the way out.
    ///
    /// Binding already unlinks a stale file, so a leftover cannot break the next
    /// run — but without this the directory accumulates a dead pair per run,
    /// indefinitely. The pid file has the same guard, and the same limitation:
    /// a process killed with SIGKILL runs no destructors, so the next bind's
    /// unlink is still the thing that guarantees correctness.
    fn drop(&mut self) {
        if let Some(u) = &self.unix {
            let _ = fs::remove_file(&u.local_in);
            // ssh owns this one, but it does not always outlive us to clean up.
            let _ = fs::remove_file(&u.local_out);
        }
    }
}

/// Create the socket's directory if it is missing, readable by nobody else.
///
/// Created with the mode in the `mkdir(2)` call rather than chmod-ed afterwards,
/// so it is never briefly world-readable — the default `/tmp/rash-<uid>` sits in
/// a world-writable directory, and the umask decides what `create_dir_all`
/// alone would leave behind.
///
/// An existing directory is left as it is, so pointing `RASH_SOCKET_DIR` at
/// something shared cannot lock other users out of it — but it must belong to
/// us. The default path is derived from the uid and therefore guessable, so
/// another local user can create it first; binding our sockets inside a
/// directory they control would let them answer probes and report a wedged
/// tunnel as healthy.
fn prepare_socket_dir(sock: &Path) -> io::Result<()> {
    let Some(dir) = sock.parent() else {
        return Ok(());
    };

    match fs::metadata(dir) {
        Ok(md) => {
            // SAFETY: getuid always succeeds and reads no memory.
            let me = unsafe { libc::getuid() };
            if md.uid() != me {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "socket directory {} belongs to uid {}, not to us ({me})",
                        dir.display(),
                        md.uid()
                    ),
                ));
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir),
        Err(e) => Err(e),
    }
}

async fn connect_tcp(addr: SocketAddr, net: Duration) -> Option<TcpStream> {
    match timeout(net, TcpStream::connect(addr)).await {
        Ok(Ok(s)) => Some(s),
        Ok(Err(e)) => {
            log_info!("{addr}: {e}");
            None
        }
        Err(_) => {
            log_info!("{addr}: connect timed out");
            None
        }
    }
}

async fn connect_unix(path: &Path, net: Duration) -> Option<UnixStream> {
    match timeout(net, UnixStream::connect(path)).await {
        Ok(Ok(s)) => Some(s),
        Ok(Err(e)) => {
            log_info!("{}: {e}", path.display());
            None
        }
        Err(_) => {
            log_info!("{}: connect timed out", path.display());
            None
        }
    }
}

async fn accept_tcp(listener: &TcpListener, net: Duration) -> Option<TcpStream> {
    match timeout(net, listener.accept()).await {
        Ok(Ok((s, _))) => Some(s),
        Ok(Err(e)) => {
            log_debug!("error accepting read connection: {e}");
            None
        }
        Err(_) => {
            log_info!("timeout polling to accept read connection");
            None
        }
    }
}

async fn accept_unix(listener: &UnixListener, net: Duration) -> Option<UnixStream> {
    match timeout(net, listener.accept()).await {
        Ok(Ok((s, _))) => Some(s),
        Ok(Err(e)) => {
            log_debug!("error accepting read connection: {e}");
            None
        }
        Err(_) => {
            log_info!("timeout polling to accept read connection");
            None
        }
    }
}
