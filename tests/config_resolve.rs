//! Configuration resolution: precedence, validation, and the derived clamps.

use rash::cli;
use rash::config::{self, ConfigError, Level, LogTarget, Monitor, Resolved};
use std::ffi::OsString;
use std::net::IpAddr;
use std::time::Duration;

fn try_resolve(args: &[&str], env: &[(&str, &str)]) -> Result<Resolved, ConfigError> {
    let inv = cli::parse(args.iter().map(OsString::from)).expect("should parse");
    config::resolve(inv, env)
}

fn resolve(args: &[&str], env: &[(&str, &str)]) -> Resolved {
    try_resolve(args, env).expect("should resolve")
}

fn ssh_args(args: &[&str], env: &[(&str, &str)]) -> Vec<String> {
    resolve(args, env)
        .config
        .ssh_args
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
    bad(&["-M", "20000:0", "host"], &[], "invalid echo port");
    bad(&["-M", "20000:x", "host"], &[], "invalid echo port");
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
