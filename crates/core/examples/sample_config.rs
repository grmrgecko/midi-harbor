//! Prints a representative configuration document, so the stored format can be reviewed by eye.
//!
//! Run with `cargo run -p midi-harbor-core --example sample_config`.

use midi_harbor_core::config::{Configuration, RouteConfig};
use midi_harbor_core::endpoint::{
    Direction, Endpoint, EndpointKind, EndpointName, InvitationPolicy, NetworkSession,
    PhysicalDevice, VirtualPort,
};
use midi_harbor_core::fingerprint::{DeviceFingerprint, MatchConfidence};

/// Builds a setup covering every endpoint kind, joined by one connection.
fn sample() -> Result<Configuration, midi_harbor_core::endpoint::NameError> {
    let mut config = Configuration::default();
    config.preferences.machine_name = Some("Studio Mac".to_owned());

    let bus = Endpoint::new(
        EndpointName::new("Sequencer Bus")?,
        EndpointKind::VirtualPort(VirtualPort {
            input_ids: vec![1_296_564_226],
            output_ids: vec![1_296_564_225],
            ..VirtualPort::default()
        }),
    );

    let keyboard = Endpoint {
        direction: Direction::Input,
        ..Endpoint::new(
            EndpointName::new("Acme K61")?,
            EndpointKind::PhysicalDevice(PhysicalDevice {
                fingerprint: DeviceFingerprint {
                    usb_serial: Some("SN-001".to_owned()),
                    manufacturer: Some("Acme".to_owned()),
                    model: Some("K61".to_owned()),
                    name: "Acme K61".to_owned(),
                    ..DeviceFingerprint::default()
                },
                present: true,
                confidence: MatchConfidence::Exact,
                claimed_by: None,
                software: false,
            }),
        )
    };

    let stage = Endpoint::new(
        EndpointName::new("Stage Laptop")?,
        EndpointKind::NetworkSession(NetworkSession::new(
            EndpointName::new("Studio Mac")?,
            5004,
            InvitationPolicy::AcceptKnown,
        )),
    );

    // Repeat the attached keyboard over the network to the other machine.
    config.routes.push(RouteConfig {
        from: "Acme K61".to_owned(),
        to: "Stage Laptop".to_owned(),
        from_kind: None,
        to_kind: None,
        from_connector: None,
        to_connector: None,
        both_ways: false,
        enabled: true,
    });
    config.endpoints = vec![bus, keyboard, stage];
    Ok(config)
}

fn main() {
    let config = match sample() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("could not build the sample configuration: {error}");
            return;
        }
    };
    match serde_yaml_ng::to_string(&config) {
        Ok(text) => print!("{text}"),
        Err(error) => eprintln!("could not encode the sample configuration: {error}"),
    }
}
