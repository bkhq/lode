#![deny(unsafe_code)]
//! lode-winproc — the Win32 process-control call lode's supervisor needs that has
//! no safe wrapper. Every other lode crate forbids unsafe code; the FFI lives here.
//!
//! The crate is empty off Windows.

/// Send `CTRL_BREAK` to the process group led by `group` (a process started with
/// `CREATE_NEW_PROCESS_GROUP`), which is how a console app is asked to stop.
///
/// The caller and the group must share a console.
///
/// # Errors
/// Returns the OS error when the event cannot be generated — for example when
/// this process has no console, or the group no longer exists.
#[cfg(windows)]
pub fn send_ctrl_break(group: u32) -> std::io::Result<()> {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};

    // SAFETY: `GenerateConsoleCtrlEvent` takes two integers by value and reads or
    // writes no memory owned by this process; any `group` value is sound, and a
    // group that does not exist is reported through the return value.
    #[allow(unsafe_code)]
    let generated = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, group) };
    if generated == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
