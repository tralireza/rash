//! Golden tests for the argv splitter.
//!
//! Cases tagged `D1`..`D4` are the autossh defects rash fixes; each one is a
//! command line that autossh mishandles today.

use rash::cli::{self, Invocation, ParseError};
use std::ffi::OsString;

fn split(args: &[&str]) -> Invocation {
    cli::parse(args.iter().map(OsString::from)).expect("should parse")
}

fn ssh(args: &[&str]) -> Vec<String> {
    split(args)
        .ssh_args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn monitor(args: &[&str]) -> Option<String> {
    split(args)
        .monitor
        .map(|m| m.to_string_lossy().into_owned())
}

#[test]
fn monitor_port_forms() {
    assert_eq!(
        monitor(&["-M", "20000", "-N", "host"]).as_deref(),
        Some("20000")
    );
    assert_eq!(
        monitor(&["-M20000", "-N", "host"]).as_deref(),
        Some("20000")
    );
    assert_eq!(
        monitor(&["-M", "20000:7", "host"]).as_deref(),
        Some("20000:7")
    );
    assert_eq!(monitor(&["-M", "0", "host"]).as_deref(), Some("0"));
    assert_eq!(monitor(&["-N", "host"]), None);

    // -M is consumed by rash and never reaches ssh.
    assert_eq!(ssh(&["-M", "20000", "-N", "host"]), ["-N", "host"]);
    assert_eq!(ssh(&["-M20000", "-N", "host"]), ["-N", "host"]);

    // Last -M wins, as autossh's getopt loop does.
    assert_eq!(
        monitor(&["-M", "20000", "-M", "30000", "host"]).as_deref(),
        Some("30000")
    );
}

#[test]
fn forwards_are_injected_where_dash_m_stood() {
    assert_eq!(split(&["-M", "20000", "-N", "host"]).inject_at, 0);
    assert_eq!(split(&["-N", "-M", "20000", "host"]).inject_at, 1);
    assert_eq!(split(&["-N", "-q", "-M", "20000", "host"]).inject_at, 2);

    let mut inv = split(&["-N", "-M", "20000", "host"]);
    cli::splice_forwards(
        &mut inv.ssh_args,
        inv.inject_at,
        ["-L", "20000:127.0.0.1:20000", "-R", "20000:127.0.0.1:20001"]
            .iter()
            .map(OsString::from)
            .collect(),
    );
    let got: Vec<String> = inv
        .ssh_args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        got,
        [
            "-N",
            "-L",
            "20000:127.0.0.1:20000",
            "-R",
            "20000:127.0.0.1:20001",
            "host"
        ]
    );
}

#[test]
fn dash_f_is_rash_s_and_is_stripped_from_clusters() {
    assert!(split(&["-f", "-M", "0", "host"]).background);
    assert_eq!(ssh(&["-f", "-M", "0", "host"]), ["host"]);

    assert!(split(&["-fN", "-M", "0", "host"]).background);
    assert_eq!(ssh(&["-fN", "-M", "0", "host"]), ["-N", "host"]);

    assert!(split(&["-Nf", "-M", "0", "host"]).background);
    assert_eq!(ssh(&["-Nf", "-M", "0", "host"]), ["-N", "host"]);

    // A cluster that is nothing but -f leaves no argument at all.
    assert_eq!(ssh(&["-M", "0", "-f", "host"]), ["host"]);
}

#[test]
fn d4_f_inside_an_option_value_is_never_stripped() {
    // autossh's strip_arg() walks characters and can eat the `f` out of a value.
    assert_eq!(
        ssh(&["-i", "/home/me/.ssh/id_for_work", "-M", "0", "host"]),
        ["-i", "/home/me/.ssh/id_for_work", "host"]
    );
    // Attached form: the value is the rest of the token.
    assert_eq!(
        ssh(&["-i/home/me/.ssh/id_for_work", "-M", "0", "host"]),
        ["-i/home/me/.ssh/id_for_work", "host"]
    );
    assert_eq!(
        ssh(&["-o", "ExitOnForwardFailure=yes", "-M", "0", "host"]),
        ["-o", "ExitOnForwardFailure=yes", "host"]
    );
}

#[test]
fn d1_unknown_options_are_forwarded_not_rejected() {
    // autossh has no `B` in OPTION_STRING at all, so it prints usage and exits.
    assert_eq!(
        ssh(&["-B", "en0", "-M", "0", "host"]),
        ["-B", "en0", "host"]
    );

    // A cluster containing an unknown letter is passed through byte-for-byte,
    // because we cannot know whether that letter takes a value.
    assert_eq!(ssh(&["-Zq", "-M", "0", "host"]), ["-Zq", "host"]);

    // Letters classified before the unknown one were provably option letters, so
    // stripping -f up to that point stays safe.
    let inv = split(&["-fNZ", "-M", "0", "host"]);
    assert!(inv.background);
    assert_eq!(ssh(&["-fNZ", "-M", "0", "host"]), ["-NZ", "host"]);
}

#[test]
fn d2_p_takes_a_value_on_modern_openssh() {
    // autossh encodes `P` as a boolean; OpenSSH 10.3 has `-P tag`.
    assert_eq!(
        ssh(&["-P", "mytag", "-M", "0", "host"]),
        ["-P", "mytag", "host"]
    );
    assert_eq!(
        ssh(&["-p", "2222", "-M", "0", "host"]),
        ["-p", "2222", "host"]
    );
}

#[test]
fn d3_nothing_after_the_separator_is_rewritten() {
    // autossh sends ssh `-lag` here, because strip_arg() runs past `--`.
    assert_eq!(
        ssh(&["-M", "0", "host", "--", "cmd", "-flag"]),
        ["host", "cmd", "-flag"]
    );
    // The first `--` is rash's; any later one is ssh's.
    assert_eq!(
        ssh(&["-M", "0", "host", "--", "cmd", "--", "-f"]),
        ["host", "cmd", "--", "-f"]
    );
    // -M is not recognised after the separator either.
    let inv = split(&["-M", "0", "host", "--", "-M", "1"]);
    assert_eq!(
        inv.monitor
            .map(|m| m.to_string_lossy().into_owned())
            .as_deref(),
        Some("0")
    );
}

#[test]
fn passthrough_shapes() {
    assert_eq!(ssh(&["-4", "-M", "0", "host"]), ["-4", "host"]);
    assert_eq!(
        ssh(&["-L", "8080:localhost:80", "-M", "0", "host"]),
        ["-L", "8080:localhost:80", "host"]
    );
    // A lone `-` is not an option.
    assert_eq!(ssh(&["-M", "0", "host", "-"]), ["host", "-"]);
    // Remote command words survive in order.
    assert_eq!(
        ssh(&["-M", "0", "-t", "host", "screen", "-D", "-R"]),
        ["-t", "host", "screen", "-D", "-R"]
    );
}

#[test]
fn dash_m_inside_a_cluster() {
    let inv = split(&["-NM20000", "host"]);
    assert_eq!(
        inv.monitor
            .map(|m| m.to_string_lossy().into_owned())
            .as_deref(),
        Some("20000")
    );
    assert_eq!(ssh(&["-NM20000", "host"]), ["-N", "host"]);
    assert_eq!(inv.inject_at, 0);

    let inv = split(&["-NM", "20000", "host"]);
    assert_eq!(
        inv.monitor
            .map(|m| m.to_string_lossy().into_owned())
            .as_deref(),
        Some("20000")
    );
    assert_eq!(ssh(&["-NM", "20000", "host"]), ["-N", "host"]);
}

#[test]
fn version_is_rash_s_not_ssh_s() {
    assert!(split(&["-V"]).version);
    assert!(split(&["-M", "0", "-V", "host"]).version);
    assert_eq!(ssh(&["-M", "0", "-V", "host"]), ["host"]);
}

#[test]
fn long_options_belong_to_rash() {
    assert!(split(&["--dry-run", "-M", "0", "host"]).dry_run);
    assert!(split(&["--help"]).help);
    assert!(split(&["--version"]).version);

    let inv = split(&["--monitor", "unix", "-N", "host"]);
    assert_eq!(
        inv.monitor_long
            .map(|m| m.to_string_lossy().into_owned())
            .as_deref(),
        Some("unix")
    );
    let inv = split(&["--monitor=unix", "-N", "host"]);
    assert_eq!(
        inv.monitor_long
            .map(|m| m.to_string_lossy().into_owned())
            .as_deref(),
        Some("unix")
    );

    // Long options are not recognised after the separator.
    assert_eq!(
        ssh(&["-M", "0", "host", "--", "--dry-run"]),
        ["host", "--dry-run"]
    );
    assert!(!split(&["-M", "0", "host", "--", "--dry-run"]).dry_run);
}

#[test]
fn errors() {
    assert_eq!(
        cli::parse(["-M"].iter().map(OsString::from)),
        Err(ParseError::MissingValue("-M".into()))
    );
    assert_eq!(
        cli::parse(["--nope"].iter().map(OsString::from)),
        Err(ParseError::UnknownLongOption("--nope".into()))
    );
    assert_eq!(
        cli::parse(["--monitor"].iter().map(OsString::from)),
        Err(ParseError::MissingValue("--monitor".into()))
    );
}
