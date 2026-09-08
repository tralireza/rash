//! `--man` recovers the manual page from the binary.
//!
//! The page is compiled in with `include_str!`, so the copy cannot drift from
//! the one in the repository. What can still break is the way back out: the
//! option being wired to the usage summary instead, the bytes going somewhere
//! other than stdout, or the early return landing after the check that a bare
//! `rash` has nothing to hand ssh.

use std::path::Path;
use std::process::Command;

const RASH: &str = env!("CARGO_BIN_EXE_rash");

#[test]
fn it_writes_the_manual_page_and_nothing_else() {
    // No monitor port, no destination: --man has to return before rash starts
    // insisting on either.
    let out = Command::new(RASH)
        .arg("--man")
        .output()
        .expect("run rash --man");

    assert!(out.status.success(), "rash --man exited {}", out.status);
    assert!(
        out.stderr.is_empty(),
        "rash --man wrote to stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let page =
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("rash.1")).expect("read rash.1");
    assert_eq!(
        out.stdout, page,
        "what came out of the binary is not rash.1"
    );
}

#[test]
fn what_comes_out_is_an_mdoc_page() {
    let out = Command::new(RASH)
        .arg("--man")
        .output()
        .expect("run rash --man");
    let page = String::from_utf8(out.stdout).expect("the page is utf-8");

    // Enough to catch the constant being pointed at some other file, without
    // pinning the wording of a page that is expected to change.
    assert!(page.contains("\n.Dt RASH 1\n"), "no mdoc title line");
    assert!(page.contains("\n.Sh SYNOPSIS\n"), "no synopsis section");
}
