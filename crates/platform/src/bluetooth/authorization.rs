//! Whether macOS has refused this process Bluetooth, as opposed to the radio being off.
//!
//! CoreBluetooth reports both as a manager that is not powered, and neither `btleplug` nor
//! `ble-peripheral-rust` passes on which. They ask different things of the user: one is a switch
//! in Control Center, the other a grant in System Settings, and telling a refused user to switch
//! their radio on sends them nowhere.

use midi_harbor_core::capability::UnavailableReason;
use objc2_core_bluetooth::{CBManager, CBManagerAuthorization};

/// Names the permission, as the capability query and its guidance show it.
const PERMISSION: &str = "Bluetooth";

/// Reports whether this process has been refused Bluetooth.
pub fn refused() -> bool {
    // SAFETY: `authorization` is a class property of CBManager, available since macOS 10.15,
    // below this build's minimum. It takes no arguments, needs no instance, and returns a plain
    // integer, so there is no pointer or lifetime to uphold.
    let authorization = unsafe { CBManager::authorization_class() };
    is_refusal(authorization)
}

/// Explains a radio CoreBluetooth reports as not powered.
pub fn unpowered_reason() -> UnavailableReason {
    if refused() {
        permission_denied()
    } else {
        UnavailableReason::AdapterOff
    }
}

/// Returns the reason for a refused permission.
pub fn permission_denied() -> UnavailableReason {
    UnavailableReason::PermissionDenied {
        what: PERMISSION.to_owned(),
    }
}

/// Reports whether an authorization means the user, or a policy, refused.
///
/// Not yet asked is not a refusal: the system asks the first time the radio is used.
fn is_refusal(authorization: CBManagerAuthorization) -> bool {
    authorization == CBManagerAuthorization::Denied
        || authorization == CBManagerAuthorization::Restricted
}
