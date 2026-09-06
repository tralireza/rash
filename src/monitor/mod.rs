//! Testing that the tunnel still carries traffic.
//!
//! Two arrangements, both set up by the `-L`/`-R` forwards `config` injects:
//!
//! * **Loop** — data written to the local monitor port travels to the remote,
//!   where the matching `-R` forward sends it straight back to a second local
//!   port that rash is listening on.
//! * **Echo** — data written to the local monitor port reaches an echo service
//!   on the remote, which returns it over the same connection.
//!
//! The listening socket is opened once and held for the life of the process, as
//! autossh does (autossh.c:465-472).

pub mod probe;

use crate::config::{Config, Monitor as Spec};
use crate::{log_debug, log_info};
use std::io;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

/// `MAX_CONN_TRIES`, autossh.c:96.
///
/// autossh configures 3 but performs 2: its loop is `while (tries++ < 3)` with
/// an immediate `if (tries >= 3) break` at the top (autossh.c:1285-1292). rash
/// makes all three attempts.
const MAX_TRIES: u32 = 3;

/// The live monitor, holding whatever socket it needs for the whole run.
pub enum Monitor {
    Disabled,
    Loop {
        listener: TcpListener,
        write_to: SocketAddr,
    },
    Echo {
        write_to: SocketAddr,
    },
}

impl Monitor {
    /// Open the listening socket, if this arrangement needs one.
    ///
    /// A failure to bind is fatal, exactly as in autossh: without the listener
    /// the loop can never complete and every probe would fail.
    pub async fn bind(cfg: &Config) -> io::Result<Self> {
        let host = cfg.monitor_host;
        Ok(match cfg.monitor {
            Spec::Disabled => Self::Disabled,
            Spec::Echo { port, .. } => Self::Echo {
                write_to: SocketAddr::new(host, port),
            },
            Spec::Loop { port } => {
                let read_on = SocketAddr::new(host, port + 1);
                // Rust opens sockets with SOCK_CLOEXEC, which is what keeps the
                // forked ssh from inheriting this listener and holding the port
                // open after rash is gone. autossh sets FD_CLOEXEC by hand
                // (autossh.c:469).
                let listener = TcpListener::bind(read_on).await?;
                log_debug!("monitor listening on {read_on}");
                Self::Loop {
                    listener,
                    write_to: SocketAddr::new(host, port),
                }
            }
        })
    }

    pub fn enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// Send a probe and wait for it to come back. `false` means the tunnel is
    /// not carrying traffic and ssh should be restarted.
    pub async fn probe(&self, cfg: &Config) -> bool {
        for attempt in 1..=MAX_TRIES {
            if self.attempt(cfg).await {
                log_debug!("connection ok");
                return true;
            }
            log_debug!("monitor attempt {attempt} of {MAX_TRIES} failed");
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

        match self {
            Self::Disabled => true,

            Self::Echo { write_to } => {
                let Some(mut s) = connect(*write_to, net).await else {
                    return false;
                };
                let (mut r, mut w) = s.split();
                matches!(
                    timeout(net, probe::exchange(&mut w, &mut r, &msg)).await,
                    Ok(true)
                )
            }

            Self::Loop { listener, write_to } => {
                let Some(mut w) = connect(*write_to, net).await else {
                    return false;
                };

                // The remote's -R forward dials back only once our write lands,
                // so the accept has to be waited for, not polled once.
                let accepted = match timeout(net, listener.accept()).await {
                    Ok(Ok((s, _))) => Some(s),
                    Ok(Err(e)) => {
                        log_debug!("error accepting read connection: {e}");
                        None
                    }
                    Err(_) => {
                        log_info!("timeout polling to accept read connection");
                        None
                    }
                };

                // Waiting for the accept before writing anything is safe because
                // the forward cascade is triggered by the connection, not by the
                // data: connecting to the local port makes ssh open a channel,
                // the remote's -R listener accept it, and a connection come back
                // to us. autossh relies on the same ordering — its poll for an
                // accept precedes any send (autossh.c:1320-1337).
                let Some(mut r) = accepted else {
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

async fn connect(addr: SocketAddr, net: std::time::Duration) -> Option<TcpStream> {
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
