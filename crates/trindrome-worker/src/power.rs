//! Power source and sleep behaviour.
//!
//! Normal mode leaves sleep entirely to Windows: progress is checkpointed, and
//! the worker continues after wake when the policy allows. Overnight mode
//! (`power.prevent_automatic_sleep`) asks Windows not to *idle*-sleep while
//! computing on AC power. That never blocks explicit sleep, shutdown or lid
//! close, and never keeps the display on.

/// Whether the machine runs on AC power (`None` if Windows does not know).
#[cfg(windows)]
pub fn on_ac_power() -> Option<bool> {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: `status` is a valid out-parameter.
    unsafe { GetSystemPowerStatus(&mut status) }.ok()?;
    match status.ACLineStatus {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

#[cfg(not(windows))]
pub fn on_ac_power() -> Option<bool> {
    None
}

/// Holds or releases the "system required" execution state for the calling
/// thread. The request lasts until changed or the thread exits, so call it from
/// one long-lived thread.
#[cfg(windows)]
pub fn keep_system_awake(awake: bool) {
    use windows::Win32::System::Power::{ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState};
    let flags = if awake {
        ES_CONTINUOUS | ES_SYSTEM_REQUIRED
    } else {
        ES_CONTINUOUS
    };
    // SAFETY: plain Win32 call with documented flags.
    unsafe { SetThreadExecutionState(flags) };
}

#[cfg(not(windows))]
pub fn keep_system_awake(_awake: bool) {}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn power_status_is_known() {
        assert!(super::on_ac_power().is_some());
        super::keep_system_awake(true);
        super::keep_system_awake(false);
    }
}
