//! Browses the local network for RTP-MIDI peers and advertises this machine.
//!
//! Run with `cargo run -p midi-harbor-daemon --example discover`.

use midi_harbor_core::ids::EndpointId;
use midi_harbor_daemon::discovery::Discovery;
use midi_harbor_daemon::identity::Identity;
use std::time::Duration;

fn main() {
    // A key of its own for the run, as every advertised session carries one.
    let discovery = match Discovery::start(&Identity::generate().public_key()) {
        Ok(discovery) => discovery,
        Err(error) => {
            eprintln!("could not start discovery: {error}");
            return;
        }
    };

    // Advertise on a port nothing else is using, so this cannot disturb a real session.
    if let Err(error) = discovery.advertise("Midi Harbor Example", 5104, EndpointId::new()) {
        eprintln!("could not advertise: {error}");
    }

    println!(
        "browsing for {} ...",
        midi_harbor_daemon::discovery::SERVICE_TYPE
    );
    std::thread::sleep(Duration::from_secs(6));

    let peers = discovery.peers();
    if peers.is_empty() {
        println!("no peers found");
    }
    for (peer, label) in &peers {
        match peer.address() {
            Some(address) => println!("  {label} -> {address}"),
            None => println!("  {label} -> no routable address"),
        }
    }
    println!(
        "{} peer(s) offered, own advertisement excluded",
        peers.len()
    );
    discovery.withdraw("Midi Harbor Example");
}
