//! Offering to set up the background service when there is no daemon to reach (FR-042).
//!
//! A window that can only say "the daemon is not running, run this command" leaves a user who
//! opened the GUI precisely to avoid a terminal with nowhere to go. What is offered depends on
//! what the service manager reports, and each offer says what it will do before it is taken.

use midi_harbor_service::{ServiceSpec, ServiceStatus};
use std::path::PathBuf;

/// What the window can do about a missing daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer {
    /// Nothing is registered, so register the service and start it.
    Install,
    /// The service is registered but not running, so start it.
    Start,
    /// The registration points at an executable that is no longer there, so register this one.
    Reinstall {
        /// The executable the old registration named.
        missing: Option<PathBuf>,
    },
    /// The service is installed and running, yet unreachable, which setting up again will not
    /// fix.
    Nothing,
}

/// What the service manager said, and so what to offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    /// What to offer.
    pub offer: Offer,
    /// The service manager's name, for saying where the service is registered.
    pub manager: &'static str,
}

/// Decides what to offer from what the service manager reports.
pub fn offer_for(status: &ServiceStatus) -> Offer {
    match (status.installed, status.stale, status.running) {
        (false, _, _) => Offer::Install,
        (true, true, _) => Offer::Reinstall {
            missing: status.registered_executable.clone(),
        },
        (true, false, false) => Offer::Start,
        (true, false, true) => Offer::Nothing,
    }
}

/// Returns the button's label for an offer, or nothing when there is nothing to offer.
pub fn action_label(offer: &Offer) -> Option<&'static str> {
    match offer {
        Offer::Install => Some("Install and start"),
        Offer::Start => Some("Start"),
        Offer::Reinstall { .. } => Some("Reinstall and start"),
        Offer::Nothing => None,
    }
}

/// Says what taking an offer will do, in terms a user can agree to before agreeing.
pub fn explanation(offer: &Offer, manager: &str) -> String {
    const KEPT: &str = "Your ports, routes and settings are kept, and 'midi-harbor service \
                        uninstall' removes the registration again.";
    match offer {
        Offer::Install => format!(
            "This registers Midi Harbor with {manager} so it starts whenever you log in, and \
             starts it now. {KEPT}"
        ),
        Offer::Start => {
            format!("Midi Harbor is registered with {manager} but not running. This starts it now.")
        }
        Offer::Reinstall { missing } => {
            let old = missing.as_ref().map_or_else(
                || "an executable that is no longer there".to_owned(),
                |path| path.display().to_string(),
            );
            format!(
                "Midi Harbor is registered with {manager} to run {old}, which is missing. This \
                 registers this copy instead and starts it. {KEPT}"
            )
        }
        Offer::Nothing => format!(
            "Midi Harbor is registered with {manager} and running, but not answering. \
             'midi-harbor service status' says more."
        ),
    }
}

/// Asks the service manager what state the service is in.
///
/// Runs off the window's thread, because asking the service manager means running a command.
pub async fn check() -> Result<Checked, String> {
    run_blocking(|| {
        let manager = midi_harbor_service::detect().map_err(|error| error.to_string())?;
        let status = manager.status().map_err(|error| error.to_string())?;
        Ok(Checked {
            offer: offer_for(&status),
            manager: manager.name(),
        })
    })
    .await
}

/// Takes an offer: registers the service when needed, then starts it.
pub async fn take(offer: Offer) -> Result<(), String> {
    run_blocking(move || {
        let manager = midi_harbor_service::detect().map_err(|error| error.to_string())?;
        if matches!(offer, Offer::Install | Offer::Reinstall { .. }) {
            let spec = ServiceSpec::for_current_executable().map_err(|error| error.to_string())?;
            manager
                .install(&spec)
                .map_err(|error| format!("could not install the service: {error}"))?;
        }
        manager
            .start()
            .map_err(|error| format!("could not start the service: {error}"))
    })
    .await
}

/// Runs a blocking service-manager call on a thread that may block.
async fn run_blocking<T: Send + 'static>(
    call: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(call)
        .await
        .map_err(|error| format!("the service check did not finish: {error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks which setup step is offered for each state the service manager reports.
    ///
    /// A registration pointing at an executable that no longer exists is stale and needs
    /// reinstalling, whatever else is true, and the offer names the missing path. A daemon that
    /// is registered and running but not answering is offered nothing, because setting up again
    /// would not reach it.
    #[test]
    fn what_is_offered_follows_what_the_service_manager_reports() {
        let old = PathBuf::from("/old/midi-harbor");
        let status = |installed, running, stale| ServiceStatus {
            installed,
            running,
            stale,
            registered_executable: stale.then(|| old.clone()),
            ..ServiceStatus::default()
        };
        let reinstall = Offer::Reinstall {
            missing: Some(old.clone()),
        };
        let cases = [
            ("not installed", status(false, false, false), Offer::Install),
            (
                "installed and stopped",
                status(true, false, false),
                Offer::Start,
            ),
            (
                "installed for a moved executable",
                status(true, false, true),
                reinstall.clone(),
            ),
            (
                "running a moved executable",
                status(true, true, true),
                reinstall,
            ),
            (
                "running and silent",
                status(true, true, false),
                Offer::Nothing,
            ),
        ];
        for (name, status, want) in cases {
            assert_eq!(
                offer_for(&status),
                want,
                "a service {name} must be offered {want:?}"
            );
        }
    }
}
