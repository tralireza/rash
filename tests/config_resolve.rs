//! Configuration resolution: precedence, validation, and the derived clamps.

use rash::cli;
use rash::config::{self, ConfigError, Format, Level, LogTarget, Monitor, Resolved};
use std::ffi::OsString;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn try_resolve(args: &[&str], env: &[(&str, &str)]) -> Result<Resolved, ConfigError> {
    let inv = cli::parse(args.iter().map(OsString::from)).expect("should parse");
    config::resolve(inv, env)
}

fn resolve(args: &[&str], env: &[(&str, &str)]) -> Resolved {
    try_resolve(args, env).expect("should resolve")
}

/// The argv rash would actually exec: the user's arguments with the monitor
/// forwards spliced in where `-M` stood.
fn ssh_args(args: &[&str], env: &[(&str, &str)]) -> Vec<String> {
    let c = resolve(args, env).config;
    let remote = c
        .unix
        .as_ref()
        .map(config::remote_sock_example)
        .unwrap_or_default();
    c.ssh_argv(c.monitor.forwards(c.monitor_host, c.unix.as_ref(), &remote))
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn secs(d: Duration) -> u64 {
    d.as_secs()
}

#[test]
fn defaults_match_autossh() {
    let c = resolve(&["-M", "20000", "-N", "host"], &[]).config;
    assert_eq!(secs(c.poll), 600);
    assert_eq!(secs(c.first_poll), 600);
    assert_eq!(c.net_timeout, Duration::from_millis(15_000));
    assert_eq!(secs(c.gate_time), 30);
    assert_eq!(c.max_start, -1);
    assert_eq!(c.max_lifetime, None);
    assert_eq!(c.monitor, Monitor::Loop { port: 20000 });
    assert_eq!(c.ssh_path.to_str(), Some("/usr/bin/ssh"));
    assert_eq!(c.log.target, LogTarget::Syslog);
    assert_eq!(c.log.level, Level::Info);
    assert!(!c.log.also_stderr);
}

#[test]
fn first_poll_follows_poll_unless_set() {
    let c = resolve(&["-M", "1", "host"], &[("AUTOSSH_POLL", "300")]).config;
    assert_eq!((secs(c.poll), secs(c.first_poll)), (300, 300));

    let c = resolve(
        &["-M", "1", "host"],
        &[("AUTOSSH_POLL", "300"), ("AUTOSSH_FIRST_POLL", "10")],
    )
    .config;
    assert_eq!((secs(c.poll), secs(c.first_poll)), (300, 10));
}

#[test]
fn short_poll_shrinks_the_net_timeout() {
    let r = resolve(&["-M", "1", "host"], &[("AUTOSSH_POLL", "10")]);
    assert_eq!(r.config.net_timeout, Duration::from_millis(5_000));
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("adjusting net timeouts"))
    );

    // A poll long enough to fit two timeouts leaves them alone.
    let r = resolve(&["-M", "1", "host"], &[("AUTOSSH_POLL", "60")]);
    assert_eq!(r.config.net_timeout, Duration::from_millis(15_000));
    assert!(r.warnings.is_empty());
}

#[test]
fn max_lifetime_clamps_both_poll_times() {
    let r = resolve(
        &["-M", "1", "host"],
        &[("AUTOSSH_POLL", "600"), ("AUTOSSH_MAXLIFETIME", "100")],
    );
    assert_eq!(secs(r.config.poll), 100);
    assert_eq!(secs(r.config.first_poll), 100);
    assert_eq!(r.config.max_lifetime, Some(Duration::from_secs(100)));
    assert_eq!(
        r.warnings.iter().filter(|w| w.contains("lifetime")).count(),
        2
    );

    // 0 means no limit at all, as in autossh.
    let c = resolve(&["-M", "1", "host"], &[("AUTOSSH_MAXLIFETIME", "0")]).config;
    assert_eq!(c.max_lifetime, None);
}

#[test]
fn an_enormous_poll_time_does_not_overflow_the_timeout_clamp() {
    // `poll * 1000 / 2` overflowed u64 here: a debug build panicked, and a
    // release build wrapped to a 192ms network timeout while warning about a
    // "short poll time" for an interval of half a trillion years.
    let huge = (u64::MAX / 1000 + 1).to_string();
    let r = resolve(&["-M", "1", "host"], &[("AUTOSSH_POLL", &huge)]);
    assert_eq!(secs(r.config.poll), u64::MAX / 1000 + 1);
    assert_eq!(
        r.config.net_timeout,
        Duration::from_millis(15_000),
        "a long poll must leave the default timeout alone"
    );
    assert!(r.warnings.is_empty(), "got {:?}", r.warnings);

    // The saturating multiply must not break the clamp it guards.
    let r = resolve(&["-M", "1", "host"], &[("AUTOSSH_POLL", "10")]);
    assert_eq!(r.config.net_timeout, Duration::from_millis(5_000));
}

#[test]
fn touch_pidfile_reads_its_value() {
    // It used to be presence-based, so RASH_TOUCH_PIDFILE=0 turned it *on*.
    // AUTOSSH_DEBUG stays presence-based, because autossh's is.
    for on in ["1", "true", "yes", "on", "TRUE"] {
        let c = resolve(&["-M", "0", "host"], &[("RASH_TOUCH_PIDFILE", on)]).config;
        assert!(c.touch_pid_file, "{on:?} should enable it");
    }
    for off in ["0", "false", "no", "off", ""] {
        let c = resolve(&["-M", "0", "host"], &[("RASH_TOUCH_PIDFILE", off)]).config;
        assert!(!c.touch_pid_file, "{off:?} should not enable it");
    }
    assert!(!resolve(&["-M", "0", "host"], &[]).config.touch_pid_file);

    match try_resolve(&["-M", "0", "host"], &[("RASH_TOUCH_PIDFILE", "maybe")]) {
        Err(ConfigError::Invalid(m)) => assert!(m.contains("touch pidfile"), "got {m:?}"),
        other => panic!("expected a rejection, got {other:?}"),
    }

    // Unchanged: autossh's own switch is set-or-not, whatever the value.
    assert!(
        resolve(&["-M", "0", "host"], &[("AUTOSSH_DEBUG", "0")])
            .config
            .log
            .also_stderr
    );
}

#[test]
fn rash_variables_outrank_autossh_ones() {
    let c = resolve(
        &["-M", "1", "host"],
        &[("AUTOSSH_POLL", "600"), ("RASH_POLL", "5")],
    )
    .config;
    assert_eq!(secs(c.poll), 5);
}

#[test]
fn monitor_port_precedence() {
    // AUTOSSH_PORT beats -M. This inversion is autossh's own documented rule.
    let r = resolve(&["-M", "20000", "-N", "host"], &[("AUTOSSH_PORT", "30000")]);
    assert_eq!(r.config.monitor, Monitor::Loop { port: 30000 });

    // RASH_PORT beats AUTOSSH_PORT.
    let r = resolve(
        &["-M", "20000", "host"],
        &[("AUTOSSH_PORT", "30000"), ("RASH_PORT", "40000")],
    );
    assert_eq!(r.config.monitor, Monitor::Loop { port: 40000 });

    // --monitor beats everything.
    let r = resolve(
        &["--monitor", "50000", "-M", "20000", "host"],
        &[("AUTOSSH_PORT", "30000")],
    );
    assert_eq!(r.config.monitor, Monitor::Loop { port: 50000 });
}

#[test]
fn env_sourced_port_injects_at_the_front() {
    // -M records its own position...
    assert_eq!(
        ssh_args(&["-N", "-M", "20000", "host"], &[]),
        [
            "-N",
            "-L",
            "20000:127.0.0.1:20000",
            "-R",
            "20000:127.0.0.1:20001",
            "host"
        ]
    );
    // ...but an environment port goes in front of everything (autossh.c:420-427),
    // even when -M is also present.
    assert_eq!(
        ssh_args(&["-N", "-M", "20000", "host"], &[("AUTOSSH_PORT", "20000")]),
        [
            "-L",
            "20000:127.0.0.1:20000",
            "-R",
            "20000:127.0.0.1:20001",
            "-N",
            "host"
        ]
    );
}

#[test]
fn monitor_modes_produce_the_right_forwards() {
    assert_eq!(
        ssh_args(&["-M", "20000", "-N", "host"], &[]),
        [
            "-L",
            "20000:127.0.0.1:20000",
            "-R",
            "20000:127.0.0.1:20001",
            "-N",
            "host"
        ]
    );
    // Echo mode is one forward only; the remote service sends the data back.
    assert_eq!(
        ssh_args(&["-M", "20000:7", "-N", "host"], &[]),
        ["-L", "20000:127.0.0.1:7", "-N", "host"]
    );
    // Monitoring off means no forwards at all.
    let r = resolve(&["-M", "0", "-N", "host"], &[]);
    assert_eq!(r.config.monitor, Monitor::Disabled);
    assert_eq!(ssh_args(&["-M", "0", "-N", "host"], &[]), ["-N", "host"]);
    assert!(r.warnings.iter().any(|w| w.contains("monitoring disabled")));
}

#[test]
fn d6_ipv6_monitor_host_is_bracketed() {
    // autossh hardcodes AF_INET and cannot do this at all.
    let c = resolve(
        &["-M", "20000", "-N", "host"],
        &[("RASH_MONITOR_HOST", "::1")],
    )
    .config;
    assert_eq!(c.monitor_host, "::1".parse::<IpAddr>().unwrap());
    assert_eq!(
        ssh_args(
            &["-M", "20000", "-N", "host"],
            &[("RASH_MONITOR_HOST", "::1")]
        ),
        [
            "-L",
            "20000:[::1]:20000",
            "-R",
            "20000:[::1]:20001",
            "-N",
            "host"
        ]
    );
}

#[test]
fn background_disables_the_starting_gate() {
    // -f means nobody can type a passphrase, so the gate would only misfire.
    let c = resolve(&["-fN", "-M", "0", "host"], &[("AUTOSSH_GATETIME", "30")]).config;
    assert!(c.background);
    assert_eq!(secs(c.gate_time), 0);

    let c = resolve(&["-M", "0", "host"], &[("AUTOSSH_GATETIME", "0")]).config;
    assert_eq!(secs(c.gate_time), 0);
}

#[test]
fn debug_outranks_loglevel() {
    let c = resolve(
        &["-M", "0", "host"],
        &[("AUTOSSH_DEBUG", "yes"), ("AUTOSSH_LOGLEVEL", "3")],
    )
    .config;
    assert_eq!(c.log.level, Level::Debug);
    assert!(c.log.also_stderr);

    let c = resolve(&["-M", "0", "host"], &[("AUTOSSH_LOGLEVEL", "3")]).config;
    assert_eq!(c.log.level, Level::Err);
    assert!(!c.log.also_stderr);

    let c = resolve(&["-M", "0", "host"], &[("AUTOSSH_LOGFILE", "/tmp/x.log")]).config;
    assert_eq!(c.log.target, LogTarget::File("/tmp/x.log".into()));
}

#[test]
fn rejections() {
    let bad = |args: &[&str], env: &[(&str, &str)], needle: &str| match try_resolve(args, env) {
        Err(ConfigError::Invalid(m)) => assert!(
            m.contains(needle),
            "expected {needle:?} in error, got {m:?}"
        ),
        other => panic!("expected an Invalid error containing {needle:?}, got {other:?}"),
    };

    bad(&["-M", "65535", "host"], &[], "out of range");
    bad(&["-M", "abc", "host"], &[], "invalid port");
    // One space, where autossh.c:348 has two — a deliberate divergence, so
    // asserted exactly. Restoring autossh's spacing has to fail this test
    // rather than pass unnoticed in either direction.
    bad(&["-M", "20000:0", "host"], &[], "invalid echo port \"0\"");
    bad(&["-M", "20000:x", "host"], &[], "invalid echo port \"x\"");
    bad(
        &["-M", "1", "host"],
        &[("AUTOSSH_POLL", "0")],
        "invalid poll time",
    );
    bad(
        &["-M", "1", "host"],
        &[("AUTOSSH_POLL", "12x")],
        "invalid poll time",
    );
    bad(
        &["-M", "1", "host"],
        &[("AUTOSSH_GATETIME", "-1")],
        "invalid gate time",
    );
    bad(
        &["-M", "1", "host"],
        &[("AUTOSSH_MAXSTART", "-2")],
        "invalid max start",
    );
    bad(
        &["-M", "1", "host"],
        &[("AUTOSSH_LOGLEVEL", "8")],
        "invalid log level",
    );
    bad(
        &["-M", "1", "host"],
        &[("AUTOSSH_MESSAGE", &"x".repeat(65))],
        "64 bytes",
    );

    // 65534 is the last port that leaves room for the read port above it.
    assert_eq!(
        resolve(&["-M", "65534", "host"], &[]).config.monitor,
        Monitor::Loop { port: 65534 }
    );

    // No port anywhere is a usage error, which the binary answers with usage.
    assert_eq!(
        try_resolve(&["-N", "host"], &[]),
        Err(ConfigError::NoMonitorPort)
    );
}

const NO_ENV: &[(&str, &str)] = &[];

const CONFIG: &str = r#"
[defaults]
poll = 300
gatetime = 15

[session.homelab]
monitor  = 20000
ssh_args = ["-N", "-R", "2200:localhost:22", "me@homelab"]
poll     = 60
message  = "homelab"

[session.jump]
monitor    = "unix"
ssh_args   = ["-N", "me@jump"]
log_format = "json"
"#;

fn with_config(args: &[&str], env: &[(&str, &str)]) -> Result<Resolved, ConfigError> {
    let file = rash::settings::parse(CONFIG).expect("the test config should parse");
    let inv = cli::parse(args.iter().map(OsString::from)).expect("should parse");
    config::resolve_with(inv, env, &file)
}

#[test]
fn a_session_layers_over_defaults() {
    let c = with_config(&["--session", "homelab"], NO_ENV)
        .expect("should resolve")
        .config;

    assert_eq!(c.monitor, Monitor::Loop { port: 20000 });
    // The session's own value wins...
    assert_eq!(secs(c.poll), 60);
    // ...and anything it leaves out comes from [defaults].
    assert_eq!(secs(c.gate_time), 15);
    assert_eq!(c.message, "homelab");

    // A session may carry the ssh arguments, so nothing else is needed on the
    // command line.
    let args: Vec<String> = c
        .ssh_args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args, ["-N", "-R", "2200:localhost:22", "me@homelab"]);
}

#[test]
fn the_environment_outranks_the_config_file() {
    // The file is the bottom of the precedence stack, above only the built-ins.
    let c = with_config(&["--session", "homelab"], &[("AUTOSSH_POLL", "5")])
        .expect("should resolve")
        .config;
    assert_eq!(secs(c.poll), 5);

    // ...and a flag outranks the environment in turn.
    let c = with_config(
        &["--session", "homelab", "--monitor", "30000"],
        &[("AUTOSSH_PORT", "40000")],
    )
    .expect("should resolve")
    .config;
    assert_eq!(c.monitor, Monitor::Loop { port: 30000 });
}

#[test]
fn a_session_can_select_the_unix_monitor_and_json_logs() {
    let c = with_config(&["--session", "jump"], NO_ENV)
        .expect("should resolve")
        .config;
    assert_eq!(c.monitor, Monitor::Unix);
    assert_eq!(c.log.format, Format::Json);
    assert!(c.unix.is_some());
    // Still inherits the defaults it did not override.
    assert_eq!(secs(c.poll), 300);
}

#[test]
fn an_unknown_session_names_the_ones_that_exist() {
    match with_config(&["--session", "nope"], NO_ENV) {
        Err(ConfigError::Invalid(m)) => {
            assert!(m.contains("no session named \"nope\""), "got {m:?}");
            assert!(
                m.contains("homelab"),
                "should list what is available: {m:?}"
            );
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[test]
fn a_bad_monitor_key_says_what_was_wanted() {
    // An untagged enum reported this as "data did not match any variant of
    // untagged enum Spec", which tells nobody what to write instead.
    let e = rash::settings::parse("[session.x]\nmonitor = -1\n").expect_err("should be rejected");
    assert!(e.contains("monitor port"), "got {e:?}");
    assert!(e.contains("unix"), "should name the alternatives: {e:?}");

    // The forms that are valid still are.
    for text in ["monitor = 20000", "monitor = \"20000:7\"", "monitor = 0"] {
        let f = rash::settings::parse(&format!("[session.x]\n{text}\n"))
            .unwrap_or_else(|e| panic!("{text} should parse: {e}"));
        assert!(f.session_names().contains(&"x"));
    }

    // And a mistyped key is still a hard error rather than a silent no-op.
    let e = rash::settings::parse("[defaults]\ngate_time = 5\n").expect_err("should be rejected");
    assert!(e.contains("gate_time"), "got {e:?}");
}

#[test]
fn the_config_file_is_searched_in_order() {
    let env: &[(&str, &str)] = &[("HOME", "/home/me")];
    assert_eq!(
        config::config_file_candidates(env),
        [
            PathBuf::from("/home/me/.rash.toml"),
            PathBuf::from("/home/me/.config/rash/config.toml"),
        ]
    );

    // XDG_CONFIG_HOME moves only the second candidate.
    let env: &[(&str, &str)] = &[("HOME", "/home/me"), ("XDG_CONFIG_HOME", "/xdg")];
    assert_eq!(
        config::config_file_candidates(env),
        [
            PathBuf::from("/home/me/.rash.toml"),
            PathBuf::from("/xdg/rash/config.toml"),
        ]
    );
}

#[test]
fn an_unknown_log_format_is_rejected() {
    let inv = cli::parse(["-M", "0", "host"].iter().map(OsString::from)).expect("parse");
    let env: &[(&str, &str)] = &[("RASH_LOG_FORMAT", "yaml")];
    match config::resolve(inv, env) {
        Err(ConfigError::Invalid(m)) => assert!(m.contains("invalid log format"), "got {m:?}"),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[test]
fn log_targets_and_levels_by_name() {
    let c = resolve(&["-M", "0", "host"], &[("RASH_LOG", "stderr")]).config;
    assert_eq!(c.log.target, LogTarget::Stderr);

    let c = resolve(&["-M", "0", "host"], &[("RASH_LOG", "syslog")]).config;
    assert_eq!(c.log.target, LogTarget::Syslog);

    // Names as well as autossh's 0-7 numbers.
    let c = resolve(&["-M", "0", "host"], &[("AUTOSSH_LOGLEVEL", "debug")]).config;
    assert_eq!(c.log.level, Level::Debug);
}

#[test]
fn the_unix_monitor_forwards_sockets_not_ports() {
    let env = [
        ("RASH_SOCKET_DIR", "/tmp/rash-unit"),
        ("RASH_REMOTE_SOCKET_DIR", "/tmp"),
    ];
    let c = resolve(&["-M", "unix", "-N", "host"], &env).config;
    assert_eq!(c.monitor, Monitor::Unix);

    let pid = std::process::id();
    let u = c.unix.as_ref().expect("unix paths should be resolved");
    assert_eq!(
        u.local_out,
        PathBuf::from(format!("/tmp/rash-unit/rash-{pid}-out.sock"))
    );
    assert_eq!(
        u.local_in,
        PathBuf::from(format!("/tmp/rash-unit/rash-{pid}-in.sock"))
    );

    // -L local_socket:remote_socket and -R remote_socket:local_socket, both
    // straight out of ssh(1). No ports anywhere.
    let remote = Path::new("/tmp/rash-deadbeef.sock");
    let f: Vec<String> = c
        .monitor
        .forwards(c.monitor_host, c.unix.as_ref(), remote)
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        f,
        [
            "-L",
            &format!("/tmp/rash-unit/rash-{pid}-out.sock:/tmp/rash-deadbeef.sock"),
            "-R",
            &format!("/tmp/rash-deadbeef.sock:/tmp/rash-unit/rash-{pid}-in.sock"),
        ]
    );
}

#[test]
fn an_over_long_socket_path_is_rejected_up_front() {
    // sun_path is ~104 bytes. Left to bind() this surfaces as a baffling error
    // from inside the socket layer, so it is caught while resolving instead.
    let long = format!("/tmp/{}", "x".repeat(120));

    // Computed rather than hardcoded: the socket name carries the pid, so the
    // length depends on how many digits it has. A literal passes on a machine
    // that has been up a while and fails on a freshly booted one, where pids
    // are still short.
    let expected = format!("{long}/rash-{}-out.sock", std::process::id()).len();

    match try_resolve(&["-M", "unix", "host"], &[("RASH_SOCKET_DIR", &long)]) {
        Err(ConfigError::Invalid(m)) => {
            assert!(m.contains("sun_path"), "got {m:?}");
            assert!(
                m.contains(&format!("{expected} bytes")),
                "should name the actual size ({expected}): {m:?}"
            );
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[test]
fn ssh_path_override() {
    let c = resolve(&["-M", "0", "host"], &[("AUTOSSH_PATH", "/opt/bin/ssh")]).config;
    assert_eq!(c.ssh_path.to_str(), Some("/opt/bin/ssh"));

    let c = resolve(
        &["-M", "0", "host"],
        &[
            ("AUTOSSH_PATH", "/opt/bin/ssh"),
            ("RASH_SSH_PATH", "/usr/local/bin/ssh"),
        ],
    )
    .config;
    assert_eq!(c.ssh_path.to_str(), Some("/usr/local/bin/ssh"));
}

#[test]
fn max_start_and_message_pass_through() {
    let c = resolve(
        &["-M", "0", "host"],
        &[("AUTOSSH_MAXSTART", "5"), ("AUTOSSH_MESSAGE", "homelab")],
    )
    .config;
    assert_eq!(c.max_start, 5);
    assert_eq!(c.message, "homelab");

    let c = resolve(&["-M", "0", "host"], &[("AUTOSSH_MAXSTART", "-1")]).config;
    assert_eq!(c.max_start, -1);
}
