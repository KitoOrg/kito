//! Starting the daemon so that it holds nothing of whoever started it.
//!
//! On Windows a process started by another inherits every handle the other
//! has marked inheritable, whatever it is given as its standard input and
//! output: among them the pipes of the program that ran `kito`. The daemon
//! outlives `kito`, and that program would wait for the end of a pipe that
//! the daemon keeps open (a test, a script that reads what `kito` says). So
//! before starting it, `kito`'s own standard handles stop being inheritable.
//!
//! The one place outside `kito-store` with `unsafe` code (ADR 0002): one
//! call to the system, on handles this process owns.

/// Marks this process's standard input, output and error as not inherited
/// by the processes it starts. Does nothing elsewhere than on Windows.
pub fn keep_standard_handles() {
    #[cfg(windows)]
    #[allow(unsafe_code)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
        let handles = [
            std::io::stdin().as_raw_handle(),
            std::io::stdout().as_raw_handle(),
            std::io::stderr().as_raw_handle(),
        ];
        for handle in handles {
            if handle.is_null() {
                continue;
            }
            // SAFETY: the handles are this process's own standard handles,
            // valid for its whole life; only their inheritance flag changes.
            unsafe {
                SetHandleInformation(handle as _, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}
