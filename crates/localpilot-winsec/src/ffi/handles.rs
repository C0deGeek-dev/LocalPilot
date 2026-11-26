//! Keeping this process's standard streams out of blanket handle inheritance.
//!
//! `CreateProcess` with handle inheritance on (as Rust's `Command` uses)
//! passes every inheritable handle the parent holds to the child, not only
//! the ones given as its standard streams. A process whose standard output is
//! a caller's pipe therefore lends that pipe to every child it starts, and a
//! long-lived descendant (a model server) keeps the caller's read from ever
//! ending. Clearing the inherit flag on this process's own standard handles
//! stops that. A child that is given `Stdio::inherit` still gets its streams:
//! Rust duplicates the selected standard handle as inheritable for that one
//! child.

use std::io;

use windows_sys::Win32::Foundation::{
    SetHandleInformation, ERROR_INVALID_HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};

/// Clear the inherit flag on this process's standard input, output and error.
/// A standard handle this process does not have is skipped.
///
/// # Errors
/// Windows refused to read or change one of the handles.
pub fn keep_std_handles_from_children() -> io::Result<()> {
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        keep_from_children(which)?;
    }
    Ok(())
}

fn keep_from_children(which: STD_HANDLE) -> io::Result<()> {
    // SAFETY: GetStdHandle takes one of the three documented constants and
    // returns this process's handle for it, null when there is none, or
    // INVALID_HANDLE_VALUE on failure. It takes no pointers.
    let handle = unsafe { GetStdHandle(which) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    if handle.is_null() {
        return Ok(());
    }
    // SAFETY: `handle` is this process's current standard handle, valid for
    // the duration of the call; only its inherit flag is cleared.
    let ok = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
    if ok != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    // A standard handle value that no longer names an open handle (its stream
    // was closed) has nothing to pass on.
    if error.raw_os_error() == Some(ERROR_INVALID_HANDLE as i32) {
        return Ok(());
    }
    Err(error)
}
