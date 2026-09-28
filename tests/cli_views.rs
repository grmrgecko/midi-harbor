//! The binary against a running daemon.
//!
//! `status` called an idle session "listening" and `session list` called the same session
//! "disconnected", which reads as a fault that is not there. And a second `daemon` started
//! beside a running one took its socket over, leaving two daemons with the same ports and
//! sessions and only one of them reachable.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use midi_harbor_core::endpoint::InvitationPolicy;
use midi_harbor_core::paths::Paths;
use midi_harbor_daemon::Daemon;
use midi_harbor_platform::fake::FakeMidiPlatform;
use midi_harbor_platform::midi::MidiPlatform;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

/// Runs the binary against the given socket, returning its exit code and standard output.
fn cli(socket: &Path, arguments: &[&str]) -> (i32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_midi-harbor"))
        .arg("--socket")
        .arg(socket)
        .args(arguments)
        .output()
        .expect("the binary runs");
    (
        output.status.code().expect("an exit code"),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/// Returns the directory every daemon in this file keeps its files under.
///
/// One directory, emptied once per run, rather than one per daemon left behind: that grew by
/// three directories and three sockets every run, and a Linux gate machine had a hundred of them.
fn scratch() -> PathBuf {
    static EMPTIED: std::sync::Once = std::sync::Once::new();
    let root = std::env::temp_dir().join("mh-cli-views");
    EMPTIED.call_once(|| {
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::create_dir_all(&root);
    });
    root
}

/// Starts a daemon serving on a socket of its own.
async fn serving() -> (Arc<Daemon>, PathBuf) {
    serving_with(None).await
}

/// Starts a daemon serving on a socket of its own, from the given configuration.
async fn serving_with(config: Option<&str>) -> (Arc<Daemon>, PathBuf) {
    static SERVED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let unique = format!(
        "{}-{}",
        std::process::id(),
        SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let root = scratch().join(&unique);
    let _ = std::fs::remove_dir_all(&root);
    if let Some(config) = config {
        let paths = Paths::rooted_at(&root);
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.config_file(), config).unwrap();
    }
    // Short, because a socket path has a length limit well below a temporary directory's.
    let socket = scratch().join(format!("{unique}.sock"));
    let daemon = Daemon::start(
        Paths::rooted_at(&root).with_socket(Some(socket.clone())),
        Arc::new(FakeMidiPlatform::new()) as Arc<dyn MidiPlatform>,
    )
    .await
    .expect("a daemon");
    let served = Arc::clone(&daemon);
    tokio::spawn(async move {
        if let Err(error) = midi_harbor_daemon::run(served).await {
            panic!("the daemon stopped serving: {error}");
        }
    });
    for _ in 0..100 {
        let probe = socket.clone();
        let (code, _) = tokio::task::spawn_blocking(move || cli(&probe, &["status"]))
            .await
            .unwrap();
        if code == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (daemon, socket)
}

/// Locks that `status` and `session list` give an idle session the same state, "listening".
///
/// `status` called it "listening" and `session list` called it "disconnected", which reads as a
/// fault that is not there. Both are JSON a script may read, so they must agree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_is_described_alike_in_status_and_in_the_session_list() {
    let (daemon, socket) = serving().await;
    daemon
        .create_network_session("Stage", 0, InvitationPolicy::Prompt)
        .await
        .unwrap();

    let (status, listed) = tokio::task::spawn_blocking(move || {
        (
            cli(&socket, &["status", "--json"]),
            cli(&socket, &["session", "list", "--json"]),
        )
    })
    .await
    .unwrap();
    assert_eq!(
        status.0, 0,
        "status must succeed against a running daemon: {status:?}"
    );
    assert_eq!(
        listed.0, 0,
        "session list must succeed against a running daemon: {listed:?}"
    );

    let state_in = |json: &str| -> String {
        let value: serde_json::Value = serde_json::from_str(json).expect("json");
        let entries = value
            .as_array()
            .or_else(|| value.get("detail").and_then(|e| e.as_array()))
            .expect("a list");
        entries
            .iter()
            .find(|entry| entry["name"] == "Stage")
            .and_then(|entry| entry["state"].as_str())
            .expect("the session and its state")
            .to_owned()
    };
    assert_eq!(
        state_in(&listed.1),
        state_in(&status.1),
        "the two views must not disagree about one session"
    );
    assert_eq!(
        state_in(&listed.1),
        "listening",
        "a session waiting to be invited is resting, not disconnected"
    );
}

/// Locks that a second `daemon` refuses to start beside a running one, and leaves it reachable.
///
/// A second daemon once took the socket over, leaving two daemons with the same ports and
/// sessions and only one of them reachable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_daemon_does_not_start_beside_a_running_one() {
    let (_daemon, socket) = serving().await;
    let home = scratch().join("second");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    let probe = socket.clone();
    let second = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_midi-harbor"))
            .arg("--socket")
            .arg(&probe)
            .arg("daemon")
            // Its own home, so if it did start it would not touch the user's configuration.
            .env("HOME", &home)
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .expect("the binary runs")
    })
    .await
    .unwrap();

    assert_eq!(
        second.status.code(),
        Some(1),
        "a second daemon must exit with the generic failure: {second:?}"
    );
    // Refused before opening anything, not by the socket after its ports and sessions were up.
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("already running"),
        "the refusal must say a daemon is already running: {second:?}"
    );
    let (code, _) = tokio::task::spawn_blocking(move || cli(&socket, &["status"]))
        .await
        .unwrap();
    assert_eq!(code, 0, "the running daemon lost its socket");
}

/// Locks that a session created without `--policy` takes the configuration's default policy.
///
/// The preference was written to every configuration file and never read: a session created
/// without a policy was always set to prompt, whatever the file said.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_created_without_a_policy_takes_the_configured_default() {
    let (daemon, socket) = serving_with(Some(
        "preferences:\n  default_invitation_policy: accept_all\n",
    ))
    .await;

    let (code, _) =
        tokio::task::spawn_blocking(move || cli(&socket, &["session", "create", "Stage"]))
            .await
            .unwrap();
    assert_eq!(code, 0, "creating a session must succeed");

    let policy = daemon
        .read(|config, _| {
            config
                .endpoints
                .iter()
                .find_map(|endpoint| match &endpoint.kind {
                    midi_harbor_core::endpoint::EndpointKind::NetworkSession(session)
                        if endpoint.name.as_str() == "Stage" =>
                    {
                        Some(session.invitation_policy)
                    }
                    _ => None,
                })
        })
        .await;
    assert_eq!(
        policy,
        Some(InvitationPolicy::AcceptAll),
        "the session must take the policy the configuration file names"
    );
}

/// Locks that `bluetooth advertise` warns on macOS when the name will not be advertised.
///
/// On macOS a name longer than five bytes is dropped from the advertisement without an error,
/// and devices show the computer's own name. The command once reported only success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_that_will_not_be_sent_is_said_so() {
    let (_daemon, socket) = serving().await;
    let advertise = |name: &'static str| {
        let socket = socket.clone();
        move || {
            let output = Command::new(env!("CARGO_BIN_EXE_midi-harbor"))
                .arg("--socket")
                .arg(&socket)
                .args(["bluetooth", "advertise", "--name", name])
                .output()
                .expect("the binary runs");
            (
                output.status.code(),
                String::from_utf8_lossy(&output.stdout).into_owned()
                    + &String::from_utf8_lossy(&output.stderr),
            )
        }
    };

    let (code, long) = tokio::task::spawn_blocking(advertise("Harbor Mac"))
        .await
        .unwrap();
    assert_eq!(code, Some(0), "a long name is still accepted: {long}");
    let (code, short) = tokio::task::spawn_blocking(advertise("HM")).await.unwrap();
    assert_eq!(code, Some(0), "a short name is accepted: {short}");

    let warned = |text: &str| text.contains("does not fit beside the MIDI service");
    assert_eq!(
        warned(&long),
        cfg!(target_os = "macos"),
        "a long name must be warned about on macOS and only there: {long}"
    );
    assert!(
        !warned(&short),
        "a name that fits must not be warned about: {short}"
    );
}

/// Locks that every command under `--json` writes exactly one JSON document, refusals included
/// (FR-039d).
///
/// Commands that change something reported in a sentence, and under `--json` printed it anyway,
/// so a script got text where it asked for JSON.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_command_under_json_writes_one_document() {
    let (_daemon, socket) = serving().await;
    let json = |arguments: &[&str]| {
        let mut all = vec!["--json"];
        all.extend_from_slice(arguments);
        let (code, out) = cli(&socket, &all);
        let document: serde_json::Value = serde_json::from_str(&out)
            .unwrap_or_else(|error| panic!("{arguments:?} wrote {out:?}: {error}"));
        (code, document)
    };

    let (code, created) = json(&["port", "create", "Scripted"]);
    assert_eq!(code, 0, "creating a port must succeed: {created}");
    assert_eq!(created["ok"], true, "a success must say ok: {created}");
    assert_eq!(
        created["result"]["name"], "Scripted",
        "the result must name what was created: {created}"
    );
    assert!(
        created["result"]["id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "the result must carry the new identifier for a script to use: {created}"
    );

    for arguments in [
        &["port", "disable", "Scripted"][..],
        &["port", "enable", "Scripted"][..],
        &["session", "create", "Scripted Stage"][..],
        &["route", "create", "Scripted", "Scripted Stage"][..],
        &["port", "rename", "Scripted", "Renamed", "--yes"][..],
        &["port", "delete", "Renamed", "--yes"][..],
    ] {
        let (code, document) = json(arguments);
        assert_eq!(code, 0, "{arguments:?} must succeed: {document}");
        assert_eq!(
            document["ok"], true,
            "{arguments:?} must report success in its document: {document}"
        );
    }

    // A refusal is a document too, saying so.
    let (code, refused) = json(&["route", "create", "Nothing", "Nowhere"]);
    assert_ne!(code, 0, "a route between missing endpoints must fail");
    assert_eq!(
        refused["ok"], false,
        "a refusal must say not ok in its document: {refused}"
    );
}

/// Locks that `session disable` and `session enable` switch a session in the daemon's
/// configuration (SC-014b).
///
/// The window switches any endpoint on and off, and the command line offered it for ports only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_can_be_switched_off_and_on_from_the_command_line() {
    let (daemon, socket) = serving().await;
    let stage = daemon
        .create_network_session("Switched", 0, InvitationPolicy::Prompt)
        .await
        .expect("a session");

    let (code, _) = cli(&socket, &["session", "disable", "Switched"]);
    assert_eq!(code, 0, "disabling a session must succeed");
    let enabled = |daemon: Arc<Daemon>| async move {
        daemon
            .read(|config, _| {
                config
                    .endpoints
                    .iter()
                    .find(|endpoint| endpoint.id == stage.id)
                    .map(|endpoint| endpoint.enabled)
            })
            .await
    };
    assert_eq!(
        enabled(Arc::clone(&daemon)).await,
        Some(false),
        "the daemon must hold the session switched off"
    );

    let (code, _) = cli(&socket, &["session", "enable", "Switched"]);
    assert_eq!(code, 0, "enabling a session must succeed");
    assert_eq!(
        enabled(Arc::clone(&daemon)).await,
        Some(true),
        "the daemon must hold the session switched on"
    );
}

/// Locks that `diagnostics export` carries every setting of the configuration it was run with.
///
/// The report named each endpoint and its state but none of its settings, so a report about a
/// session that would not connect did not say which machine it was calling.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_diagnostic_report_carries_the_whole_configuration() {
    let (_daemon, socket) = serving_with(Some(concat!(
        "preferences:\n",
        "  default_invitation_policy: accept_all\n",
        "endpoints:\n",
        "  - name: Stage\n",
        "    kind: network_session\n",
        "    local_name: Stage Left\n",
        "    invitation_policy: accept_known\n",
        "    peer: 2a7026ac-9b34-4e57-b19b-918962a74e8f\n",
        "peers:\n",
        "  - id: 2a7026ac-9b34-4e57-b19b-918962a74e8f\n",
        "    name: Studio PC\n",
        "    addresses:\n",
        "      - 192.0.2.13:5004\n",
    )))
    .await;

    let (code, written) =
        tokio::task::spawn_blocking(move || cli(&socket, &["diagnostics", "export"]))
            .await
            .unwrap();
    assert_eq!(code, 0, "exporting a report must succeed");
    let report: serde_json::Value = serde_json::from_str(&written).unwrap();
    let configuration = &report["configuration"];
    assert_eq!(
        configuration["preferences"]["default_invitation_policy"], "accept_all",
        "the report must carry the preferences: {configuration}"
    );
    let stage = configuration["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|endpoint| endpoint["name"] == "Stage")
        .unwrap();
    assert_eq!(
        stage["local_name"], "Stage Left",
        "the report must carry the session's advertised name: {stage}"
    );
    assert_eq!(
        stage["invitation_policy"], "accept_known",
        "the report must carry the session's own policy: {stage}"
    );
    assert_eq!(
        stage["peer"], "2a7026ac-9b34-4e57-b19b-918962a74e8f",
        "the report must say which machine the session calls: {stage}"
    );
    assert_eq!(
        configuration["peers"][0]["addresses"][0], "192.0.2.13:5004",
        "the report must carry where that machine was last reached: {configuration}"
    );
}

/// Locks that `config import-apple` reads Apple's setup through the daemon, and exits 4 where
/// there is none.
///
/// The command line queried CoreMIDI itself, which Principle II reserves for the daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apples_setup_is_read_through_the_daemon() {
    let (_daemon, socket) = serving().await;
    let (code, written) =
        tokio::task::spawn_blocking(move || cli(&socket, &["config", "import-apple"]))
            .await
            .unwrap();
    if cfg!(target_os = "macos") {
        // Without --yes it shows a plan and asks, or has nothing to do.
        assert!(
            code == 0 || code == 7,
            "without --yes the import must show its plan and ask, or finish: exit {code}: {written}"
        );
        assert!(
            written.contains("to create") || written.contains("nothing to import"),
            "the import must describe its plan: {written}"
        );
    } else {
        assert_eq!(code, 4, "a platform without Apple's setup says so");
    }
}

/// Locks what `status` and `events` say after the daemon replaced one that lost the MIDI
/// service, that `dismiss-warning` clears it for everyone, and that none of it appears otherwise.
///
/// A USB MIDI device dropping off its hub can crash `MIDIServer`. The daemon replaces itself and
/// recovers in seconds, but a program that plays cues stays attached to the dead service and misses
/// the next cue; before this, the only trace was an event nobody read. `status` now warns above its table, scripts read the time from
/// `midi_server_replaced_at`, and the event has a kind of its own. The time is when the loss was
/// found, handed over by the process that found it, not when the replacement started; and
/// dismissing the warning, from the window or here, clears it in the daemon, so a warning
/// dismissed in one place no longer shows in another. 1_790_521_263 seconds after the epoch is
/// 2026-09-27 15:01:03 UTC.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_that_replaced_one_after_the_midi_service_died_warns_until_dismissed() {
    let lost_at = jiff::Timestamp::from_second(1_790_521_263).unwrap();
    for (name, replaced) in [("replaced", true), ("started plainly", false)] {
        let (daemon, socket) = serving().await;
        if replaced {
            daemon
                .note_replaced_after_midi_server_lost(Some(lost_at))
                .await;
        }

        let (json, table, events, dismissed, dismissed_again, after) =
            tokio::task::spawn_blocking(move || {
                (
                    cli(&socket, &["status", "--json"]),
                    cli(&socket, &["status"]),
                    cli(&socket, &["events", "--json"]),
                    cli(&socket, &["dismiss-warning", "--json"]),
                    cli(&socket, &["dismiss-warning", "--json"]),
                    cli(&socket, &["status", "--json"]),
                )
            })
            .await
            .unwrap();
        assert_eq!(json.0, 0, "{name}: status must succeed: {json:?}");
        let parse = |out: &str| -> serde_json::Value { serde_json::from_str(out).expect("json") };
        let status = parse(&json.1);
        let events = parse(&events.1);
        let recorded = events
            .as_array()
            .expect("a list of events")
            .iter()
            .any(|event| event["kind"] == "midi_server_replaced");

        assert_eq!(
            status["midi_server_replaced_at"].as_str(),
            replaced.then_some("2026-09-27T15:01:03Z"),
            "{name}: status --json must carry when the MIDI service was found gone, only after it \
             was: {status}"
        );
        assert_eq!(
            table.1.starts_with("warning: the MIDI service stopped at "),
            replaced,
            "{name}: status must warn above its table only after the MIDI service was lost: {}",
            table.1
        );
        assert_eq!(
            recorded, replaced,
            "{name}: the history must hold a midi_server_replaced event only after the MIDI \
             service was lost: {events}"
        );
        assert_eq!(
            (
                parse(&dismissed.1)["result"]["dismissed"].as_bool(),
                parse(&dismissed_again.1)["result"]["dismissed"].as_bool(),
            ),
            (Some(replaced), Some(false)),
            "{name}: dismissing must clear a warning there is, once, and say when there is none"
        );
        assert!(
            parse(&after.1)["midi_server_replaced_at"].is_null(),
            "{name}: a dismissed warning must be gone from status: {}",
            after.1
        );
    }
}
