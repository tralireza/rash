//! The two pure decision tables: what to do when ssh dies, and how long to wait
//! before starting it again. Both are ported from autossh and both are expected
//! to agree with it exactly.

use rash::backoff::Backoff;
use rash::supervise::{Death, Reason, Verdict, classify};
use std::time::Duration;

const GATE: Duration = Duration::from_secs(30);
const NO_GATE: Duration = Duration::ZERO;
const QUICK: Duration = Duration::from_secs(2);
const SETTLED: Duration = Duration::from_secs(600);

#[test]
fn a_killed_child_is_always_restarted() {
    // autossh 1.4f changed this: a signalled ssh was probably hung, not
    // deliberately stopped, so restarting is the better guess.
    for sig in [libc::SIGTERM, libc::SIGKILL, libc::SIGINT, libc::SIGHUP] {
        assert_eq!(
            classify(Death::Signal(sig), 1, QUICK, GATE),
            (Verdict::Restart, Reason::Signalled),
            "signal {sig}"
        );
    }
}

#[test]
fn the_starting_gate_catches_a_first_session_that_dies_at_once() {
    // Any exit status inside the gate, on the first run, means it never got out
    // of the gate — authentication or the connection itself failed.
    for code in [0, 1, 2, 42, 255] {
        assert_eq!(
            classify(Death::Exit(code), 1, QUICK, GATE),
            (Verdict::ExitErr, Reason::PrematureExit),
            "code {code}"
        );
    }

    // Past the gate the ordinary rules apply again.
    assert_eq!(
        classify(Death::Exit(255), 1, SETTLED, GATE),
        (Verdict::Restart, Reason::ConnectionLost)
    );

    // The gate only ever applies to the first run.
    assert_eq!(
        classify(Death::Exit(255), 2, QUICK, GATE),
        (Verdict::Restart, Reason::ConnectionLost)
    );

    // AUTOSSH_GATETIME=0 disables it entirely.
    assert_eq!(
        classify(Death::Exit(255), 1, QUICK, NO_GATE),
        (Verdict::Restart, Reason::ConnectionLost)
    );
}

#[test]
fn exit_status_table() {
    // 255 is a dropped connection as far as we can tell: restart.
    assert_eq!(
        classify(Death::Exit(255), 3, SETTLED, GATE),
        (Verdict::Restart, Reason::ConnectionLost)
    );
    // A clean exit means the user typed `exit`: stop, successfully.
    assert_eq!(
        classify(Death::Exit(0), 3, SETTLED, GATE),
        (Verdict::ExitOk, Reason::CleanExit)
    );
    // 1 and 2 on a later run mean the network went away.
    for code in [1, 2] {
        assert_eq!(
            classify(Death::Exit(code), 2, SETTLED, GATE),
            (Verdict::Restart, Reason::ConnectionLost),
            "code {code} on a restart"
        );
    }
    // ...but on the very first run, with a gate set, they mean the command line
    // is wrong, so stop and let the user fix it.
    assert_eq!(
        classify(Death::Exit(1), 1, SETTLED, GATE),
        (Verdict::ExitErr, Reason::Failed)
    );
    // Anything else is a remote command's status: report it and stop.
    for code in [3, 42, 127] {
        assert_eq!(
            classify(Death::Exit(code), 5, SETTLED, GATE),
            (Verdict::ExitErr, Reason::Failed),
            "code {code}"
        );
    }
}

#[test]
fn gate_boundary_is_inclusive() {
    // autossh compares with `<=`, so a session lasting exactly the gate time
    // still counts as premature.
    assert_eq!(
        classify(Death::Exit(1), 1, GATE, GATE),
        (Verdict::ExitErr, Reason::PrematureExit)
    );
    assert_eq!(
        classify(Death::Exit(1), 1, GATE + Duration::from_secs(1), GATE),
        (Verdict::ExitErr, Reason::Failed)
    );
}

/// The published curve for the default 600s poll: 2t² seconds, capped at 600.
#[test]
fn backoff_curve_matches_the_documented_table() {
    let poll = Duration::from_secs(600);
    let mut b = Backoff::default();

    // The first five quick restarts are free.
    for n in 1..=5 {
        assert_eq!(
            b.next_delay(Duration::ZERO, poll),
            Duration::ZERO,
            "try {n} should not sleep"
        );
        assert_eq!(b.tries(), n);
    }

    for (tries, expected) in [(6, 2), (7, 8), (8, 18), (9, 32), (10, 50)] {
        let d = b.next_delay(Duration::ZERO, poll);
        assert_eq!(b.tries(), tries);
        assert_eq!(d, Duration::from_secs(expected), "tries = {tries}");
    }

    // Further out. next_delay() increments first, so wind up to one below the
    // count being asserted.
    let wind_to = |b: &mut Backoff, n: u32| {
        while b.tries() < n - 1 {
            b.next_delay(Duration::ZERO, poll);
        }
        b.next_delay(Duration::ZERO, poll)
    };

    assert_eq!(wind_to(&mut b, 15), Duration::from_secs(200));
    assert_eq!(wind_to(&mut b, 20), Duration::from_secs(450));

    // 2t² first exceeds the 600s poll at tries = 23 (t = 18, 648s), so that is
    // where the cap starts biting.
    assert_eq!(wind_to(&mut b, 23), poll, "capped at the poll interval");
    assert_eq!(wind_to(&mut b, 40), poll, "and stays capped");
}

#[test]
fn staying_up_long_enough_resets_the_backoff() {
    let poll = Duration::from_secs(600);
    let mut b = Backoff::default();

    for _ in 0..10 {
        b.next_delay(Duration::ZERO, poll);
    }
    assert!(b.tries() > 5);

    // min_time is poll/10, so 60s here. One session that lasts that long clears
    // the whole history.
    assert_eq!(b.next_delay(Duration::from_secs(60), poll), Duration::ZERO);
    assert_eq!(b.tries(), 0);

    // A session just short of it does not.
    let mut b = Backoff::default();
    b.next_delay(Duration::from_secs(59), poll);
    assert_eq!(b.tries(), 1);
}

#[test]
fn min_time_has_a_floor_of_ten_seconds() {
    // With a very short poll, poll/10 would round to 0 and every restart would
    // count as settled. autossh floors it at 10s.
    let poll = Duration::from_secs(5);
    let mut b = Backoff::default();

    b.next_delay(Duration::from_secs(9), poll);
    assert_eq!(b.tries(), 1, "9s is below the 10s floor");

    b.next_delay(Duration::from_secs(10), poll);
    assert_eq!(b.tries(), 0, "10s meets the floor");
}

#[test]
fn the_first_start_never_sleeps() {
    // There is no predecessor, so the caller passes Duration::MAX.
    let mut b = Backoff::default();
    assert_eq!(
        b.next_delay(Duration::MAX, Duration::from_secs(600)),
        Duration::ZERO
    );
    assert_eq!(b.tries(), 0);
}
