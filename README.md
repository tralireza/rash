# rash — Rust Auto SSH

Start an `ssh` session or tunnel, watch it, and restart it when it dies or stops passing
traffic. `rash` is a behaviour-compatible reimplementation of [autossh(1)][autossh] in Rust.

**Status: work in progress.** See [Milestones](#milestones) for what works today.

## Why

autossh still does its job, but it was last released in 2019 and it has aged in three
specific ways:

- **Its ssh option table has drifted from reality.** autossh validates arguments against a
  hardcoded option string that predates current OpenSSH. On OpenSSH 10.3, `ssh -B
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
| Monitor probe attempts | 3 configured, 2 actually performed (off-by-one) | 3 |
| Killing a wedged child | `SIGTERM`, then wait forever | `SIGTERM`, wait `RASH_KILL_TIMEOUT` (5s), then `SIGKILL` |
| Numeric arguments | `strtoul` base 0, so `-M 020000` is read as octal 8192 | base 10 always, so `-M 020000` is 20000 |

Both are strictly more robust. Everything else that differs is a bug fix — notably, autossh
strips `f` from arguments that appear *after* `--`, so `autossh -M 0 host -- cmd -flag`
hands ssh `-lag`; rash stops rewriting at the first `--`.

## Milestones

- [x] **M0** — scaffold
- [x] **M1** — argument splitter, config resolution, `--dry-run`
- [x] **M2** — supervisor: spawn, exit policy, backoff, signals, daemonise, pidfile
- [x] **M3** — TCP monitor, loop and echo modes
- [ ] **M4** — UNIX-socket monitor, TOML sessions, JSON logging
- [ ] **M5** — `rash.1`, migration guide

Everything autossh does is working: `rash -M port`, `rash -M port:echo_port` and
`rash -M 0`, the exit-status policy and backoff, `SIGTERM`/`SIGINT`/`SIGQUIT`/
`SIGUSR1`/`SIGHUP`, `-f`, and the pid file. What remains is new surface rather
than parity.

## Building

```sh
cargo build --release
```

No nightly features are used; stable and nightly are both tested in CI.

## Credits

autossh was written by Carson Harding and is the origin of every behaviour `rash`
reproduces. `rash` is an independent implementation written from autossh's observable
behaviour, its manual page, and its source; it is not a line-by-line translation.

## Licence

MIT — see [`LICENSE`](LICENSE), or <https://opensource.org/licenses/MIT>.

autossh itself is distributed under permissive terms — "redistribution and use in
source and binary forms, with or without modification, are freely permitted" —
which this is compatible with.

[autossh]: https://www.harding.motd.ca/autossh/
