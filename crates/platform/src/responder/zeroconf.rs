//! Registration through `zeroconf`, which drives Bonjour on macOS and Avahi on Linux.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::{debug, warn};

/// Registers a service with Bonjour or Avahi and keeps it published until `running` is cleared.
///
/// The responder's event loop has to be driven, which is why this blocks for the life of the
/// advertisement.
pub(super) fn advertise(
    name: &str,
    port: u16,
    running: &AtomicBool,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) {
    use ::zeroconf::prelude::*;

    let service_type = match ::zeroconf::ServiceType::new("apple-midi", "udp") {
        Ok(service_type) => service_type,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return;
        }
    };

    let mut service = ::zeroconf::MdnsService::new(service_type, port);
    service.set_name(name);

    let announced = Arc::new(AtomicBool::new(false));
    let callback_announced = Arc::clone(&announced);
    service.set_registered_callback(Box::new(
        move |result: ::zeroconf::Result<::zeroconf::ServiceRegistration>,
              _context: Option<Arc<dyn std::any::Any + Send + Sync>>| {
            if result.is_ok() {
                callback_announced.store(true, Ordering::Relaxed);
            }
        },
    ));

    let event_loop = match service.register() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return;
        }
    };

    // Drive the loop until the responder confirms, so a rejected name is reported rather than
    // leaving a session that believes it is advertised.
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    while std::time::Instant::now() < deadline && !announced.load(Ordering::Relaxed) {
        if event_loop.poll(Duration::from_millis(100)).is_err() {
            let _ = ready.send(Err("the responder stopped".to_owned()));
            return;
        }
    }
    let _ = ready.send(Ok(()));

    while running.load(Ordering::Relaxed) {
        if event_loop.poll(Duration::from_millis(200)).is_err() {
            warn!(
                name,
                "the platform responder stopped; this session is no longer advertised"
            );
            return;
        }
    }
    debug!(name, "withdrew advertisement");
}
