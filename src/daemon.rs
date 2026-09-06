//! Detaching from the terminal, for `-f`.
//!
//! This has to run *before* the tokio runtime is created: `fork(2)` does not
//! carry threads into the child, so a runtime built beforehand would be left
//! with worker threads that no longer exist.
//!
//! autossh calls `daemon(3)` (autossh.c:448). macOS deprecates that function, so
//! rash performs the same steps by hand — `daemon(0, 0)` means "do chdir to /"
//! and "do close the standard descriptors".

use std::io;

/// Fork twice, start a new session, move to `/`, and point the standard
/// descriptors at `/dev/null`.
///
/// Returns only in the final grandchild; the intermediate processes call
/// `_exit(0)`.
pub fn daemonize() -> io::Result<()> {
    // First fork: the parent leaves, so the child is guaranteed not to be a
    // process-group leader and setsid() can therefore succeed.
    fork_and_leave_parent()?;

    // SAFETY: setsid takes no arguments and only affects the calling process.
    if unsafe { libc::setsid() } == -1 {
        return Err(io::Error::last_os_error());
    }

    // Second fork: now that we lead a session, forking again means this process
    // can never acquire a controlling terminal.
    fork_and_leave_parent()?;

    // SAFETY: `c"/"` is a 'static NUL-terminated string.
    if unsafe { libc::chdir(c"/".as_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }

    redirect_std_to_dev_null()
}

/// Fork; the parent exits immediately, the child returns.
fn fork_and_leave_parent() -> io::Result<()> {
    // SAFETY: fork takes no arguments. The child returns to a single-threaded
    // process — no tokio runtime exists yet — and the parent does nothing but
    // `_exit`, which runs no destructors and touches no shared state.
    match unsafe { libc::fork() } {
        -1 => Err(io::Error::last_os_error()),
        0 => Ok(()),
        // SAFETY: `_exit` is async-signal-safe and always valid to call.
        _ => unsafe { libc::_exit(0) },
    }
}

fn redirect_std_to_dev_null() -> io::Result<()> {
    // SAFETY: `c"/dev/null"` is a 'static NUL-terminated string, and O_RDWR is a
    // valid flag combination for open(2).
    let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }

    for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: `fd` is a descriptor we just opened and `target` is one of the
        // three standard descriptor numbers.
        if unsafe { libc::dup2(fd, target) } == -1 {
            let e = io::Error::last_os_error();
            // SAFETY: closing the descriptor we opened above, once.
            unsafe { libc::close(fd) };
            return Err(e);
        }
    }

    if fd > libc::STDERR_FILENO {
        // SAFETY: `fd` is still open and is not one of the three we just aliased.
        unsafe { libc::close(fd) };
    }
    Ok(())
}
