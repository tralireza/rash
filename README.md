[![CI](https://github.com/tralireza/rash/actions/workflows/ci.yml/badge.svg)](https://github.com/tralireza/rash/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/rash-ssh.svg)](https://crates.io/crates/rash-ssh)

# rash — Rust Auto SSH

Start an `ssh` session or tunnel, watch it, and restart it when it dies or stops passing
traffic. `rash` is a behaviour-compatible reimplementation of [autossh(1)][autossh] in Rust.

Everything autossh does, plus the additions under [Beyond autossh](#beyond-autossh).
`man ./rash.1` is the full manual.

## Usage

```
rash [-V] [-M port[:echo_port]] [-f] [--dry-run] [--monitor SPEC] [SSH_OPTIONS]
rash --session NAME [--config PATH]
rash --list [--config PATH]
rash --help | --man | --version
```

`-M`, `-f` and `-V` are rash's only short options, and every long option is rash's too.
Everything else is passed to ssh untouched.

```sh
# Keep a forward up, restarting whenever it stops carrying traffic
rash -M 20000 -N -L 8080:localhost:80 me@host

# The same forward, with no monitor ports to find at either end
rash --monitor unix -N -L 8080:localhost:80 me@host

# Show exactly what would be executed, and with what settings, without connecting
rash --dry-run -M 20000 -N me@host

# No monitoring — restart only when ssh exits
rash -M 0 -N -o ServerAliveInterval=15 me@host
```

Connections have to be established unattended, so `rash` needs some form of automatic
authentication — normally a key held by `ssh-agent`. Make sure the ssh command works on
its own before putting `rash` in front of it.

## Why

autossh still does its job, but it was last released in 2019 and it has aged in three
specific ways:

- **Its ssh option table has drifted from reality.** autossh validates arguments against a
  hardcoded option string that predates current OpenSSH. On OpenSSH 10, `ssh -B
  bind_interface` is unknown to it and `-P` is encoded as a boolean when ssh now takes
  `-P tag` — passing either makes autossh print usage and refuse to run. Every new ssh
  option breaks it again.
- **Its control flow is `sigsetjmp`/`siglongjmp` + `alarm()` + `pause()`**, with `syslog()`
  reachable from a signal handler. Racy by construction; its CHANGES file is a decade of
  patches to that one design.
- **It has no tests.**

`rash` keeps the behaviour and replaces the machinery: one `tokio::select!` over the child
process, a timer, and a signal stream, plus a real test suite.

## Compatibility

Defaults are the same, so existing wrapper scripts, systemd units, and `AUTOSSH_*`
environment variables keep working:

- the same `-M port[:echo_port]`, `-f`, and `-V` flags, with all other arguments passed
  through to ssh;
- the same exit-status policy, "starting gate" behaviour, and restart backoff curve;
- the same monitor forwarding scheme and wire protocol;
- every `AUTOSSH_*` variable except `AUTOSSH_NTSERVICE` (Cygwin support is dropped). Each
  also has a `RASH_*` alias that takes precedence.

New surface — long options, a TOML config, the UNIX-socket monitor — is opt-in and off by
default. ssh has no long options at all, which is what makes `--xxx` a safe extension point.

### Intentional differences

| | autossh | rash |
|---|---|---|
| Monitor probe attempts | 3 configured, 2 actually performed (off-by-one), back to back | 3, pausing a tenth of the net timeout (max 1s) between them |
| Killing a wedged child | `SIGTERM`, then wait forever | `SIGTERM`, wait `RASH_KILL_TIMEOUT` (5s), then `SIGKILL` |
| Numeric arguments | `strtoul` base 0, so `-M 020000` is read as octal 8192 | base 10 always, so `-M 020000` is 20000 |
| Bad echo port message | `invalid echo port··"7"` — two spaces (autossh.c:348) | one space |

The last one is cosmetic and deliberately not bug-compatible: it is a startup rejection
written to stderr before any log sink exists, so no log parser sees it.

Everything else that differs is a bug fix — notably, autossh strips `f` from arguments
that appear *after* `--`, so `autossh -M 0 host -- cmd -flag` hands ssh `-lag`; rash stops
rewriting at the first `--`.

## Migrating from autossh

Replace `autossh` with `rash`. That is the whole procedure — the flags, the environment
variables, the exit codes and the log lines are all the same, so wrapper scripts, systemd
units and launchd plists need no changes.

Only `AUTOSSH_NTSERVICE` is gone, along with Cygwin support. The four behaviours that
differ on purpose are listed above.

Once you have switched, these are worth knowing about:

| Instead of | Consider |
|---|---|
| `-M 20000`, and finding two free local ports and one on the remote | `--monitor unix` — no ports at either end |
| A wrapper script per tunnel | a `[session.<name>]` block, then `rash --session <name>` |
| Guessing what ssh will actually receive | `rash --dry-run` |
| `AUTOSSH_LOGFILE` and parsing text | `RASH_LOG` and `RASH_LOG_FORMAT=json` |

## Beyond autossh

All opt-in. Defaults are unchanged, so none of this affects a plain `rash -M 20000 …`.

### A monitor with no ports

```sh
rash --monitor unix -N -L 8080:localhost:80 me@host
```

Runs the monitor loop over UNIX-domain sockets rather than TCP, so there are no monitor
ports to choose and none to collide — at either end. The forward itself is unaffected:
`-L 8080:localhost:80` above is yours, and rash adds its own `-L` and `-R` alongside it,
as `--dry-run` will show.

The remote socket path is regenerated on **every ssh start**, and that detail is
load-bearing. `StreamLocalBindUnlink` defaults to `no` in `sshd_config` and a client
cannot override it, so a socket left behind by an unclean disconnect would block sshd from
binding it again and rash would reconnect forever against a forward that could never come
up. A fresh path sidesteps the server's configuration entirely.

The remote needs `AllowStreamLocalForwarding` (already the default) and a writable `/tmp`;
point `RASH_REMOTE_SOCKET_DIR` elsewhere if not. Socket paths are checked against the
~104-byte `sun_path` limit while resolving, rather than failing later with an opaque error
from inside the socket layer.

### Named sessions

Optional and absent by default. Two locations are searched, first that exists wins:
**`~/.rash.toml`**, then **`~/.config/rash/config.toml`** (honouring `XDG_CONFIG_HOME`).
`--config PATH` overrides both, and naming a file that isn't there is an error rather than
an empty config.

```toml
[defaults]
poll = 300
gatetime = 15

[session.homelab]
monitor  = 20000                                       # or "20000:7", "unix", 0
ssh_args = ["-N", "-R", "2200:localhost:22", "me@host"]
poll     = 60
```

```sh
rash --session homelab      # rash --list shows what is defined
```

Both blocks take the same keys, all optional. An unrecognised key is an error rather
than a setting that quietly does nothing, so this list is exhaustive — each is its
environment variable with the prefix dropped and lowercased, which is why some run
together and some do not:

| | |
|---|---|
| `monitor` | as `--monitor` |
| `ssh_args` | array of strings; the only key with no variable |
| `ssh_path` `poll` `first_poll` `gatetime` | as `AUTOSSH_PATH` `_POLL` `_FIRST_POLL` `_GATETIME` |
| `maxstart` `maxlifetime` `message` `pidfile` | as `AUTOSSH_MAXSTART` `_MAXLIFETIME` `_MESSAGE` `_PIDFILE` |
| `loglevel` `log` `log_format` | as `AUTOSSH_LOGLEVEL`, `RASH_LOG`, `RASH_LOG_FORMAT` |
| `monitor_host` `kill_timeout` | as `RASH_MONITOR_HOST` `_KILL_TIMEOUT` |

`AUTOSSH_DEBUG` and `RASH_TOUCH_PIDFILE` have no key: they are switches for one run,
not settings for a tunnel.

The file is the **lowest** layer of the precedence stack, above only the built-in
defaults: a flag beats `RASH_*`, which beats `AUTOSSH_*`, which beats `[session.<name>]`,
which beats `[defaults]`. Anything the file can set is also settable the old way.

One caveat worth knowing before you write one: **`[defaults]` applies to every run**,
including runs that name no session, so a config file changes what a bare
`rash -M 20000 host` does. autossh has no config file and so no equivalent action at a
distance. `--config /dev/null` ignores yours for a single run.

### Structured logs

`RASH_LOG_FORMAT=json` emits one object per line — `ts`, `level`, `pid`, `msg` — to
whichever sink is in use. `RASH_LOG` picks that sink: `syslog`, `stderr`, or a path. The
default text format is byte-identical to autossh's, so existing log parsing is unaffected.

## Building and installing

```sh
cargo install rash-ssh
```

The crate is **`rash-ssh`** because crates.io has had an unrelated `rash` — a file
hashing tool — since 2018. The binary it installs is `rash`, and nothing else about the
rename is visible. `cargo install` places no manual page, so take `rash.1` from a release
tarball or from a checkout:

```sh
cargo build --release

install -d ~/.local/share/man/man1
install -m 755 target/release/rash  ~/.local/bin/
install -m 644 rash.1               ~/.local/share/man/man1/
```

`~/.local/share/man` is on the default manpath on both macOS and Linux, so `man rash`
works from there with no further setup.

To read the manual without installing it — note the leading `./`, which is what makes both
BSD and GNU `man` treat the argument as a file rather than a page name:

```sh
man ./rash.1
```

No nightly features are used; stable and nightly are both tested in CI, on Linux and
macOS. The end-to-end tests need the `test-harness` feature, off by default because it
builds a stand-in ssh that no ordinary build has any use for: `cargo test --all-features`.

## Credits

autossh was written by Carson Harding and is the origin of every behaviour `rash`
reproduces. `rash` is an independent implementation written from autossh's observable
behaviour, its manual page, and its source; it is not a line-by-line translation.

## Licence

MIT — see [`LICENSE`](LICENSE), or <https://opensource.org/licenses/MIT>.

autossh itself is distributed under permissive terms — "redistribution and use in source
and binary forms, with or without modification, are freely permitted" — which this is
compatible with.

[autossh]: https://www.harding.motd.ca/autossh/
