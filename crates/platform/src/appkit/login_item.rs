//! Starting at login, through `SMAppService.mainApp` (research R-099).
//!
//! macOS owns the registration; it is read back every time it is shown, never stored.

use objc2_service_management::{SMAppService, SMAppServiceStatus};

/// What the Start at login switch shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginItem {
    /// Midi Harbor starts at login.
    On,
    /// Registered, and waiting for the user to allow it in System Settings > General > Login
    /// Items.
    Waiting,
    /// Midi Harbor does not start at login; registering decides whether it can.
    Off,
}

/// Maps the status macOS reports onto the switch.
///
/// `notFound` is off rather than unavailable: an installed app that has never registered reports
/// it on current macOS, and `register()` is the only way out of it (R-099).
fn from_status(status: SMAppServiceStatus) -> LoginItem {
    match status {
        SMAppServiceStatus::Enabled => LoginItem::On,
        SMAppServiceStatus::RequiresApproval => LoginItem::Waiting,
        _ => LoginItem::Off,
    }
}

/// Reads whether Midi Harbor starts at login.
pub fn status() -> LoginItem {
    // SAFETY: `mainAppService` and `status` take no arguments and have no preconditions; they
    // read this app's registration from ServiceManagement.
    from_status(unsafe { SMAppService::mainAppService().status() })
}

/// Turns starting at login on or off, returning what macOS reports afterwards.
pub fn set(enabled: bool) -> Result<LoginItem, String> {
    // SAFETY: `mainAppService` has no preconditions, and registering or unregistering the app
    // itself, which is what `mainApp` names, is permitted in the sandbox.
    let result = unsafe {
        let service = SMAppService::mainAppService();
        if enabled {
            service.registerAndReturnError()
        } else {
            service.unregisterAndReturnError()
        }
    };
    match result {
        Ok(()) => Ok(status()),
        Err(error) => Err(error.localizedDescription().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks the switch against every status macOS reports.
    ///
    /// `notFound` must read as off, with registering allowed: macOS 15 reports it for an
    /// installed app that has never registered, and a switch that took it for unavailable could
    /// never be turned on (research R-099, as martonpaulo/mailbell#49 found).
    #[test]
    fn each_status_shows_as_the_switch_should() {
        let cases = [
            ("enabled", SMAppServiceStatus::Enabled, LoginItem::On),
            (
                "requires approval",
                SMAppServiceStatus::RequiresApproval,
                LoginItem::Waiting,
            ),
            (
                "not registered",
                SMAppServiceStatus::NotRegistered,
                LoginItem::Off,
            ),
            ("not found", SMAppServiceStatus::NotFound, LoginItem::Off),
        ];
        for (name, status, want) in cases {
            assert_eq!(
                from_status(status),
                want,
                "{name}: the switch shows the wrong state"
            );
        }
    }
}
