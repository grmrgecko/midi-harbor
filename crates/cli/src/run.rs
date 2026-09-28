//! Executing the parsed command.

use crate::client::{Client, ClientError};
use crate::commands::{
    BluetoothCommand, Cli, Command, ConfigCommand, DeviceCommand, DiagnosticsCommand, ImportMode,
    NetworkCommand, PeerCommand, PortCommand, RouteCommand,
};
use crate::exit::{self, ExitCode};
use crate::output::{Format, table};
use crate::service_cmd;
use midi_harbor_core::paths::Paths;
use midi_harbor_ipc::pb;

/// How often `status --watch` redraws when nothing has changed, for the traffic counts.
const WATCH_REFRESH: std::time::Duration = std::time::Duration::from_secs(1);

/// Runs everything except the daemon and the graphical interface, which the binary handles.
pub async fn dispatch(cli: Cli) -> ExitCode {
    let format = Format::new(cli.json, cli.quiet);
    let outcome = run_command(&cli, &format).await;
    format.finish(outcome == ExitCode::Success);
    outcome
}

/// Runs the command the invocation names.
async fn run_command(cli: &Cli, format: &Format) -> ExitCode {
    match &cli.command {
        Some(Command::Service(command)) => {
            service_cmd::run(command, format, cli.socket.clone()).await
        }
        // Refused before connecting, like a delete, so the answer does not depend on whether
        // the daemon is up.
        Some(Command::Config(ConfigCommand::Import {
            path,
            mode: ImportMode::Replace,
            yes: false,
        })) => {
            eprintln!(
                "replacing removes every endpoint, route and peer that {} does not mention",
                path.display()
            );
            eprintln!("re-run with --yes to confirm, or use --mode merge to only add");
            ExitCode::ConfirmationRequired
        }
        Some(Command::Config(ConfigCommand::ImportApple { yes })) => {
            let yes = *yes;
            connected(cli, format, move |client, format| {
                Box::pin(import_apple(client, format, yes))
            })
            .await
        }
        Some(Command::Config(
            command @ (ConfigCommand::Export { .. }
            | ConfigCommand::Import { .. }
            | ConfigCommand::Reload),
        )) => {
            let command = command.clone();
            connected(cli, format, move |client, format| {
                Box::pin(transfer_config(client, format, command))
            })
            .await
        }
        Some(Command::Config(command)) => config(command, format),
        Some(Command::Status { watch }) => {
            let watch = *watch;
            connected(cli, format, move |client, format| {
                Box::pin(async move {
                    if !watch {
                        return status(client, format).await;
                    }
                    // The whole table is redrawn together, so a state and a count never come from
                    // different moments. A change on the state stream redraws it at once, and the
                    // timer covers traffic counts, which the stream does not carry. If the stream
                    // ends, the timer alone keeps the table current.
                    let mut changes = client
                        .rpc()
                        .watch_state(pb::WatchStateRequest {})
                        .await
                        .ok()
                        .map(tonic::Response::into_inner);
                    loop {
                        print!("\x1b[2J\x1b[H");
                        let code = status(client, format).await;
                        if code != ExitCode::Success {
                            return code;
                        }
                        let tick = tokio::time::sleep(WATCH_REFRESH);
                        let ended = match &mut changes {
                            Some(stream) => tokio::select! {
                                () = tick => false,
                                changed = stream.message() => !matches!(changed, Ok(Some(_))),
                            },
                            None => {
                                tick.await;
                                false
                            }
                        };
                        if ended {
                            changes = None;
                        }
                    }
                })
            })
            .await
        }
        Some(Command::Capabilities) => connected(cli, format, capabilities).await,
        Some(Command::Events {
            since,
            limit,
            follow,
        }) => {
            let (since, limit, follow) = (*since, *limit, *follow);
            connected(cli, format, move |client, format| {
                Box::pin(events(client, format, since, limit, follow))
            })
            .await
        }
        Some(Command::Monitor { endpoint, raw }) => {
            let (endpoint, raw) = (endpoint.clone(), *raw);
            connected(cli, format, move |client, format| {
                Box::pin(monitor(client, format, endpoint, raw))
            })
            .await
        }
        Some(Command::DismissWarning) => {
            connected(cli, format, |client, format| {
                Box::pin(dismiss_warning(client, format))
            })
            .await
        }
        Some(Command::SendNote {
            endpoint,
            note,
            channel,
            velocity,
            length,
        }) => {
            let request = pb::SendTestNoteRequest {
                endpoint_id: endpoint.clone(),
                channel: *channel,
                note: *note,
                velocity: *velocity,
                length_ms: Some(*length),
            };
            connected(cli, format, move |client, format| {
                Box::pin(send_note(client, format, request))
            })
            .await
        }
        Some(Command::Diagnostics(DiagnosticsCommand::Export { output })) => {
            let output = output.clone();
            connected(cli, format, move |client, format| {
                Box::pin(diagnostics(client, format, output))
            })
            .await
        }
        Some(Command::Route(command)) => {
            let command = clone_route_command(command);
            connected(cli, format, move |client, format| {
                Box::pin(route(client, format, command))
            })
            .await
        }
        Some(Command::Device(command)) => {
            let command = match command {
                DeviceCommand::List { all } => DeviceCommand::List { all: *all },
                DeviceCommand::Forget { device } => DeviceCommand::Forget {
                    device: device.clone(),
                },
                DeviceCommand::Resolve { device, chosen } => DeviceCommand::Resolve {
                    device: device.clone(),
                    chosen: chosen.clone(),
                },
                DeviceCommand::Enable { target } => DeviceCommand::Enable {
                    target: target.clone(),
                },
                DeviceCommand::Disable { target } => DeviceCommand::Disable {
                    target: target.clone(),
                },
            };
            connected(cli, format, move |client, format| {
                Box::pin(device(client, format, command))
            })
            .await
        }
        Some(Command::Bluetooth(command)) => {
            let command = clone_bluetooth_command(command);
            connected(cli, format, move |client, format| {
                Box::pin(bluetooth(client, format, command))
            })
            .await
        }
        Some(Command::Network(NetworkCommand::Delete {
            session,
            yes: false,
        })) => {
            eprintln!(
                "deleting '{session}' disconnects every machine connected to it, removes its port \
                 from applications on this computer, and leaves any route naming it with a missing \
                 endpoint"
            );
            eprintln!("re-run with --yes to confirm");
            ExitCode::ConfirmationRequired
        }
        Some(Command::Network(command)) => {
            let command = clone_session_command(command);
            connected(cli, format, move |client, format| {
                Box::pin(session(client, format, command))
            })
            .await
        }
        // Refused before connecting, so a script learns it needs --yes whether or not the daemon
        // is up. The flag was once accepted and ignored, and a delete went ahead unasked.
        Some(Command::Port(PortCommand::Delete { target, yes: false })) => {
            eprintln!(
                "deleting '{target}' removes it from every application using it, and leaves any \
                 route naming it with a missing endpoint"
            );
            eprintln!("re-run with --yes to confirm");
            ExitCode::ConfirmationRequired
        }
        Some(Command::Port(command)) => {
            let command = clone_port_command(command);
            connected(cli, format, move |client, format| {
                Box::pin(port(client, format, command))
            })
            .await
        }
        // The binary handles these before dispatching, so reaching here is a programming error
        // rather than something a user can cause.
        Some(Command::Daemon { .. }) | Some(Command::Gui) | None => ExitCode::Usage,
    }
}

/// Connects to the daemon and runs a command against it.
async fn connected<F>(cli: &Cli, format: &Format, body: F) -> ExitCode
where
    F: for<'a> FnOnce(
        &'a mut Client,
        &'a Format,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ExitCode> + 'a>>,
{
    let mut client = match Client::connect(cli.socket.clone()).await {
        Ok(client) => client,
        Err(ClientError::NotRunning) => {
            eprintln!("{}", ClientError::NotRunning);
            return ExitCode::DaemonUnreachable;
        }
        Err(ClientError::Version(guidance)) => {
            eprintln!("{guidance}");
            return ExitCode::VersionMismatch;
        }
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::DaemonUnreachable;
        }
    };
    body(&mut client, format).await
}

/// Shows endpoints and their health.
fn status<'a>(
    client: &'a mut Client,
    format: &'a Format,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ExitCode> + 'a>> {
    Box::pin(async move {
        let summary = match client.rpc().get_status(pb::GetStatusRequest {}).await {
            Ok(response) => response.into_inner(),
            Err(status) => return report(&status),
        };
        let endpoints = match client
            .rpc()
            .list_endpoints(pb::ListEndpointsRequest {
                kind: None,
                include_absent: true,
            })
            .await
        {
            Ok(response) => response.into_inner().endpoints,
            Err(status) => return report(&status),
        };

        let replaced_at = instant(summary.midi_server_replaced_at.as_ref());
        if format.json {
            format.emit(&serde_json::json!({
                "endpoints": endpoints.len(),
                "connected": summary.connected_count,
                "routes": summary.route_count,
                "broken_routes": summary.broken_route_count,
                "midi_server_replaced_at": replaced_at.map(|at| at.to_string()),
                "detail": endpoints.iter().map(|endpoint| serde_json::json!({
                    "id": endpoint.id,
                    "name": endpoint.name,
                    "kind": endpoint_kind(endpoint),
                    "state": endpoint_state(endpoint),
                    "next_retry": retry_at(endpoint).map(|at| at.to_string()),
                    "received": endpoint.counters.as_ref().map(|c| c.messages_received),
                    "sent": endpoint.counters.as_ref().map(|c| c.messages_sent),
                    "lost": endpoint.counters.as_ref().map(|c| c.messages_lost),
                    // Next to loss because it answers it: what the recovery journal rebuilt.
                    "recovered": endpoint.counters.as_ref().map(|c| c.messages_recovered),
                    "dropped": endpoint.counters.as_ref().map(|c| c.messages_dropped),
                    "malformed": endpoint.counters.as_ref().map(|c| c.packets_malformed),
                    "last_received": endpoint.counters.as_ref()
                        .and_then(|c| instant(c.last_received.as_ref()))
                        .map(|at| at.to_string()),
                    "last_sent": endpoint.counters.as_ref()
                        .and_then(|c| instant(c.last_sent.as_ref()))
                        .map(|at| at.to_string()),
                    "automatic_port": automatic_port_counters(endpoint).map(|c| serde_json::json!({
                        "received": c.messages_received,
                        "sent": c.messages_sent,
                        "dropped": c.messages_dropped,
                        "last_received": instant(c.last_received.as_ref()).map(|at| at.to_string()),
                        "last_sent": instant(c.last_sent.as_ref()).map(|at| at.to_string()),
                    })),
                })).collect::<Vec<_>>(),
            }));
            return ExitCode::Success;
        }

        let zone = jiff::tz::TimeZone::system();
        let now = jiff::Timestamp::now();
        // Said before the table, because other applications may be deaf to everything the
        // table shows arriving, and nothing in it would say so.
        if let Some(at) = replaced_at {
            format.line(format!(
                "warning: the MIDI service stopped at {} and was restarted. Midi Harbor \
                 recovered, but other apps may have lost their MIDI connection; relaunch any \
                 that stop sending or receiving MIDI. Clear this with \
                 'midi-harbor dismiss-warning'.\n",
                clock_time(Some(at), &zone, now)
            ));
        }

        let traffic = |counters: Option<&pb::TrafficCounters>| {
            [
                counters
                    .map(|c| c.messages_received.to_string())
                    .unwrap_or_default(),
                counters
                    .map(|c| c.messages_sent.to_string())
                    .unwrap_or_default(),
                counters
                    .map(|c| clock_time(instant(c.last_received.as_ref()), &zone, now))
                    .unwrap_or_default(),
                counters
                    .map(|c| clock_time(instant(c.last_sent.as_ref()), &zone, now))
                    .unwrap_or_default(),
            ]
        };
        let mut rows: Vec<Vec<String>> = Vec::with_capacity(endpoints.len());
        for endpoint in &endpoints {
            let reason = endpoint
                .state
                .as_ref()
                .and_then(|state| state.last_error.as_ref())
                .map(|error| error.message.clone())
                .unwrap_or_default();
            let state = match retry_in(endpoint, now) {
                Some(next) => format!("{}, {next}", endpoint_state(endpoint)),
                None => endpoint_state(endpoint),
            };
            let mut row = vec![
                endpoint.name.clone(),
                endpoint_kind(endpoint).to_owned(),
                state,
            ];
            row.extend(traffic(endpoint.counters.as_ref()));
            row.push(reason);
            rows.push(row);

            // A network port's own counts say what the network carried; its automatic port's
            // say what reached the applications on this computer and what they sent.
            if let Some(counters) = automatic_port_counters(endpoint) {
                let mut row = vec![
                    "  automatic port".to_owned(),
                    "to apps".to_owned(),
                    String::new(),
                ];
                row.extend(traffic(Some(counters)));
                row.push(String::new());
                rows.push(row);
            }
        }

        if rows.is_empty() {
            format.line("no endpoints configured");
            format.line("create one with: midi-harbor port create \"Sequencer Bus\"");
        } else {
            // Traffic sits beside state, because "connected" and "connected but carrying
            // nothing" are different problems and a state column cannot tell them apart.
            format.line(
                table(
                    &[
                        "NAME",
                        "KIND",
                        "STATE",
                        "IN",
                        "OUT",
                        "LAST IN",
                        "LAST OUT",
                        "LAST ERROR",
                    ],
                    &rows,
                )
                .trim_end(),
            );
        }
        ExitCode::Success
    })
}

/// Shows which capabilities this machine has.
fn capabilities<'a>(
    client: &'a mut Client,
    format: &'a Format,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ExitCode> + 'a>> {
    Box::pin(async move {
        let response = match client
            .rpc()
            .get_capabilities(pb::GetCapabilitiesRequest {})
            .await
        {
            Ok(response) => response.into_inner(),
            Err(status) => return report(&status),
        };

        if format.json {
            let rendered: Vec<_> = response
                .capabilities
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "name": c.name,
                        "available": c.available,
                        "reason": c.reason,
                    })
                })
                .collect();
            format.emit(&rendered);
            return ExitCode::Success;
        }

        let rows: Vec<Vec<String>> = response
            .capabilities
            .iter()
            .map(|c| {
                vec![
                    c.name.clone(),
                    if c.available { "yes" } else { "no" }.to_owned(),
                    c.reason.clone(),
                ]
            })
            .collect();
        format.line(table(&["CAPABILITY", "AVAILABLE", "REASON"], &rows).trim_end());
        ExitCode::Success
    })
}

/// Watches the MIDI passing through an endpoint.
async fn monitor(client: &mut Client, format: &Format, endpoint: String, raw: bool) -> ExitCode {
    let request = pb::MonitorEndpointRequest {
        endpoint_id: endpoint.clone(),
    };
    let mut stream = match client.rpc().monitor_endpoint(request).await {
        Ok(response) => response.into_inner(),
        Err(status) => return report(&status),
    };

    format.note(format!("watching '{endpoint}'; press ctrl-c to stop"));
    loop {
        match stream.message().await {
            Ok(Some(message)) => {
                // A watcher that fell behind is told, rather than shown a gap it cannot see.
                if message.dropped > 0 {
                    format.note(format!(
                        "... {} messages missed while behind",
                        message.dropped
                    ));
                }
                let direction = if message.outbound { "out" } else { "in " };
                if raw {
                    let hex: Vec<String> = message
                        .data
                        .iter()
                        .map(|byte| format!("{byte:02X}"))
                        .collect();
                    format.line(format!(
                        "{direction}  {:<28} {}",
                        message.decoded,
                        hex.join(" ")
                    ));
                } else {
                    format.line(format!("{direction}  {}", message.decoded));
                }
            }
            Ok(None) => return ExitCode::Success,
            Err(status) => return report(&status),
        }
    }
}

/// Clears the warning that the MIDI service stopped.
async fn dismiss_warning(client: &mut Client, format: &Format) -> ExitCode {
    match client
        .rpc()
        .dismiss_midi_server_warning(pb::DismissMidiServerWarningRequest {})
        .await
    {
        Ok(response) => {
            let dismissed = response.into_inner().dismissed;
            format.result(serde_json::json!({ "dismissed": dismissed }));
            format.line(if dismissed {
                "dismissed the warning that the MIDI service stopped"
            } else {
                "there is no warning to dismiss"
            });
            ExitCode::Success
        }
        Err(status) => report(&status),
    }
}

/// Sends one note out of an endpoint, for testing.
async fn send_note(
    client: &mut Client,
    format: &Format,
    request: pb::SendTestNoteRequest,
) -> ExitCode {
    let said = format!(
        "sent note {} on channel {} at velocity {} to '{}' for {} ms",
        request.note,
        request.channel,
        request.velocity,
        request.endpoint_id,
        request.length_ms.unwrap_or_default()
    );
    match client.rpc().send_test_note(request).await {
        Ok(_) => {
            format.line(said);
            ExitCode::Success
        }
        Err(status) => report(&status),
    }
}

/// Writes a diagnostic report.
async fn diagnostics(
    client: &mut Client,
    format: &Format,
    output: Option<std::path::PathBuf>,
) -> ExitCode {
    let request = pb::ExportDiagnosticsRequest {
        include_message_log: false,
    };
    let written = match client.rpc().export_diagnostics(request).await {
        Ok(response) => response.into_inner().report,
        Err(status) => return report(&status),
    };

    match output {
        Some(path) => match std::fs::write(&path, written) {
            Ok(()) => {
                format.line(format!("wrote {}", path.display()));
                ExitCode::Success
            }
            Err(error) => {
                eprintln!("could not write {}: {error}", path.display());
                ExitCode::Failure
            }
        },
        None => {
            format.line(written.trim_end());
            ExitCode::Success
        }
    }
}

/// Shows recent connection events.
async fn events(
    client: &mut Client,
    format: &Format,
    since: Option<u64>,
    limit: u32,
    follow: bool,
) -> ExitCode {
    let zone = jiff::tz::TimeZone::system();
    // Following streams the history forward instead of printing a page and exiting, which is
    // what makes a failure that happens while watching visible as it happens.
    if follow {
        let mut stream = match client
            .rpc()
            .watch_events(pb::WatchEventsRequest { after_id: since })
            .await
        {
            Ok(response) => response.into_inner(),
            Err(status) => return report(&status),
        };
        loop {
            match stream.message().await {
                Ok(Some(event)) => {
                    format.line(format!(
                        "{:<5} {} {:<28} {}",
                        event.id,
                        event_time(&event, &zone),
                        event.kind,
                        event.detail
                    ));
                }
                Ok(None) => return ExitCode::Success,
                Err(status) => return report(&status),
            }
        }
    }

    let response = match client
        .rpc()
        .list_events(pb::ListEventsRequest {
            after_id: since,
            limit,
            endpoint_id: None,
        })
        .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return report(&status),
    };

    if format.json {
        let rendered: Vec<_> = response
            .events
            .iter()
            .map(|e| {
                serde_json::json!({
                    "id": e.id,
                    "at": event_instant(e).map(|at| at.to_string()),
                    "kind": e.kind,
                    "endpoint_id": e.endpoint_id,
                    "route_id": e.route_id,
                    "detail": e.detail,
                })
            })
            .collect();
        format.emit(&rendered);
        return ExitCode::Success;
    }

    if response.events.is_empty() {
        format.line("no events recorded yet");
        return ExitCode::Success;
    }
    let rows: Vec<Vec<String>> = response
        .events
        .iter()
        .map(|e| {
            vec![
                e.id.to_string(),
                event_time(e, &zone),
                e.kind.clone(),
                e.detail.clone(),
            ]
        })
        .collect();
    format.line(table(&["ID", "TIME", "KIND", "DETAIL"], &rows).trim_end());
    ExitCode::Success
}

/// Returns when an event happened, when the daemon said.
///
/// Every event carries its time, and the history printed none of it, so it could say a link
/// dropped and recovered but not when, or for how long.
fn event_instant(event: &pb::Event) -> Option<jiff::Timestamp> {
    instant(event.at.as_ref())
}

/// Converts a time from the wire, when there is one.
fn instant(at: Option<&prost_types::Timestamp>) -> Option<jiff::Timestamp> {
    let at = at?;
    jiff::Timestamp::new(at.seconds, at.nanos).ok()
}

/// Renders a time in the given zone to the second, with its date only when that is not today,
/// or "-" for none.
fn clock_time(
    at: Option<jiff::Timestamp>,
    zone: &jiff::tz::TimeZone,
    now: jiff::Timestamp,
) -> String {
    let Some(at) = at else {
        return "-".to_owned();
    };
    let at = at.to_zoned(zone.clone());
    if at.date() == now.to_zoned(zone.clone()).date() {
        at.strftime("%H:%M:%S").to_string()
    } else {
        at.strftime("%Y-%m-%d %H:%M:%S").to_string()
    }
}

/// Returns a network port's automatic port's traffic, while it is open.
fn automatic_port_counters(endpoint: &pb::Endpoint) -> Option<&pb::TrafficCounters> {
    match &endpoint.detail {
        Some(pb::endpoint::Detail::NetworkSession(detail)) => {
            detail.automatic_port_counters.as_ref()
        }
        _ => None,
    }
}

/// Renders when an event happened in the given zone, to the second.
fn event_time(event: &pb::Event, zone: &jiff::tz::TimeZone) -> String {
    event_instant(event)
        .map(|at| {
            at.to_zoned(zone.clone())
                .strftime("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_owned())
}

/// Returns a virtual port's MIDI In and MIDI Out connector counts, one of each for anything the
/// daemon does not say.
fn connector_counts(endpoint: &pb::Endpoint) -> (u32, u32) {
    match &endpoint.detail {
        Some(pb::endpoint::Detail::VirtualPort(port)) => (port.inputs.max(1), port.outputs.max(1)),
        _ => (1, 1),
    }
}

/// Names one end of a route, with its connector when it is past the first.
fn route_end(name: &str, kind: &str, connector: u32) -> String {
    if connector > 1 {
        format!("{name} ({kind} {connector})")
    } else {
        name.to_owned()
    }
}

/// Says how many connectors of one kind a port has, as "2 MIDI Outs".
fn connectors_phrase(count: u8, kind: &str) -> String {
    if count == 1 {
        format!("1 {kind}")
    } else {
        format!("{count} {kind}s")
    }
}

/// Runs a virtual port subcommand.
async fn port(client: &mut Client, format: &Format, command: PortCommand) -> ExitCode {
    match command {
        PortCommand::List => {
            let kind = Some(pb::EndpointKind::VirtualPort as i32);
            let response = match client
                .rpc()
                .list_endpoints(pb::ListEndpointsRequest {
                    kind,
                    include_absent: true,
                })
                .await
            {
                Ok(response) => response.into_inner(),
                Err(status) => return report(&status),
            };

            if format.json {
                let rendered: Vec<_> = response
                    .endpoints
                    .iter()
                    .map(|e| {
                        let (inputs, outputs) = connector_counts(e);
                        serde_json::json!({
                            "id": e.id,
                            "name": e.name,
                            "enabled": e.enabled,
                            "state": e.state.as_ref().map(|s| phase_name(s.phase)),
                            "inputs": inputs,
                            "outputs": outputs,
                        })
                    })
                    .collect();
                format.emit(&rendered);
                return ExitCode::Success;
            }
            if response.endpoints.is_empty() {
                format.line("no virtual ports configured");
                return ExitCode::Success;
            }
            let rows: Vec<Vec<String>> = response
                .endpoints
                .iter()
                .map(|e| {
                    let (inputs, outputs) = connector_counts(e);
                    vec![
                        e.name.clone(),
                        inputs.to_string(),
                        outputs.to_string(),
                        if e.enabled { "enabled" } else { "disabled" }.to_owned(),
                        e.state
                            .as_ref()
                            .map(|s| phase_name(s.phase))
                            .unwrap_or("unknown")
                            .to_owned(),
                        e.id.clone(),
                    ]
                })
                .collect();
            format.line(
                table(
                    &["NAME", "MIDI IN", "MIDI OUT", "ENABLED", "STATE", "ID"],
                    &rows,
                )
                .trim_end(),
            );
            ExitCode::Success
        }

        PortCommand::Create {
            name,
            inputs,
            outputs,
            direction,
        } => {
            if direction.is_some() {
                format.note(
                    "--direction is no longer used: every virtual port has a MIDI In and a MIDI Out",
                );
            }
            let request = pb::CreateVirtualPortRequest {
                name,
                direction: pb::Direction::Bidirectional as i32,
                inputs: u32::from(inputs),
                outputs: u32::from(outputs),
            };
            match client.rpc().create_virtual_port(request).await {
                Ok(response) => {
                    let endpoint = response.into_inner();
                    format.result(endpoint_result(&endpoint));
                    format.line(format!(
                        "created '{}'; other applications can see it now",
                        endpoint.name
                    ));
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }

        PortCommand::Connectors {
            target,
            inputs,
            outputs,
        } => {
            let request = pb::SetVirtualPortConnectorsRequest {
                id: target,
                inputs: u32::from(inputs),
                outputs: u32::from(outputs),
            };
            match client.rpc().set_virtual_port_connectors(request).await {
                Ok(response) => {
                    let endpoint = response.into_inner();
                    format.result(endpoint_result(&endpoint));
                    format.line(format!(
                        "'{}' now has {} and {}; applications using it may need to choose it again",
                        endpoint.name,
                        connectors_phrase(inputs, "MIDI In"),
                        connectors_phrase(outputs, "MIDI Out"),
                    ));
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }

        PortCommand::Rename {
            target,
            new_name,
            yes,
        } => {
            let request = pb::RenameEndpointRequest {
                id: target,
                new_name,
                confirm: yes,
            };
            match client.rpc().rename_endpoint(request).await {
                Ok(response) => {
                    let endpoint = response.into_inner();
                    format.result(endpoint_result(&endpoint));
                    format.line(format!("renamed to '{}'", endpoint.name));
                    ExitCode::Success
                }
                Err(status) if status.code() == tonic::Code::FailedPrecondition => {
                    eprintln!("{}", status.message());
                    eprintln!("re-run with --yes to confirm");
                    ExitCode::ConfirmationRequired
                }
                Err(status) => report(&status),
            }
        }

        PortCommand::Delete { target, yes: _ } => {
            match client
                .rpc()
                .delete_virtual_port(pb::DeleteVirtualPortRequest { id: target })
                .await
            {
                Ok(response) => {
                    let orphaned = response.into_inner().orphaned_routes;
                    format.line("deleted");
                    for route in orphaned {
                        format.note(format!("route '{route}' now has a missing endpoint"));
                    }
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }

        // Enabling and disabling differ only in the flag, so they share one round trip.
        PortCommand::Enable { target } => set_enabled(client, format, target, true).await,
        PortCommand::Disable { target } => set_enabled(client, format, target, false).await,
    }
}

/// Switches an endpoint on or off.
async fn set_enabled(
    client: &mut Client,
    format: &Format,
    target: String,
    enabled: bool,
) -> ExitCode {
    let request = pb::SetEndpointEnabledRequest {
        id: target,
        enabled,
    };
    match client.rpc().set_endpoint_enabled(request).await {
        Ok(response) => {
            let endpoint = response.into_inner();
            format.line(format!(
                "'{}' is now {}",
                endpoint.name,
                if enabled { "enabled" } else { "disabled" }
            ));
            ExitCode::Success
        }
        Err(status) => report(&status),
    }
}

/// Binds a remembered device entry to one particular piece of attached hardware.
async fn resolve_device(
    client: &mut Client,
    format: &Format,
    device: String,
    chosen: Option<String>,
) -> ExitCode {
    let endpoints = match client
        .rpc()
        .list_endpoints(pb::ListEndpointsRequest {
            kind: Some(pb::EndpointKind::PhysicalDevice as i32),
            include_absent: true,
        })
        .await
    {
        Ok(response) => response.into_inner().endpoints,
        Err(status) => return report(&status),
    };

    let Some(target) = endpoints
        .iter()
        .find(|endpoint| endpoint.name == device || endpoint.id == device)
    else {
        eprintln!("no remembered device called '{device}'");
        return ExitCode::NotFound;
    };

    // Without a choice, the candidates are printed: a command cannot ask a question and wait for
    // an answer, and picking one for the user is the thing this whole feature exists to avoid.
    let Some(chosen) = chosen else {
        let candidates: Vec<&pb::Endpoint> = endpoints
            .iter()
            .filter(|endpoint| {
                endpoint.id != target.id && endpoint.name.starts_with(target.name.as_str())
            })
            .collect();
        if candidates.is_empty() {
            format.line(format!("nothing attached looks like '{device}'"));
            return ExitCode::Success;
        }
        format.line(format!("which of these is '{device}'?"));
        for candidate in candidates {
            format.line(format!("  {}", candidate.name));
        }
        format.line("");
        format.line(format!(
            "then: midi-harbor device resolve \"{device}\" \"<name>\""
        ));
        return ExitCode::Success;
    };

    let Some(picked) = endpoints
        .iter()
        .find(|endpoint| endpoint.name == chosen || endpoint.id == chosen)
    else {
        eprintln!("no attached device called '{chosen}'");
        return ExitCode::NotFound;
    };
    let fingerprint = match &picked.detail {
        Some(pb::endpoint::Detail::PhysicalDevice(detail)) => detail.fingerprint.clone(),
        _ => None,
    };

    let request = pb::ResolveAmbiguousDeviceRequest {
        id: target.id.clone(),
        chosen: fingerprint,
    };
    match client.rpc().resolve_ambiguous_device(request).await {
        Ok(response) => {
            format.line(format!(
                "'{}' now means the hardware you chose",
                response.into_inner().name
            ));
            ExitCode::Success
        }
        Err(status) => report(&status),
    }
}

/// Runs a connection subcommand.
async fn route(client: &mut Client, format: &Format, command: RouteCommand) -> ExitCode {
    match command {
        RouteCommand::List { broken } => {
            let response = match client
                .rpc()
                .list_routes(pb::ListRoutesRequest {
                    only_broken: broken,
                })
                .await
            {
                Ok(response) => response.into_inner(),
                Err(status) => return report(&status),
            };

            if format.json {
                let rendered: Vec<_> = response
                    .routes
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id,
                            "from": r.from_name,
                            "to": r.to_name,
                            "from_connector": r.from_connector,
                            "to_connector": r.to_connector,
                            "both_ways": r.both_ways,
                            "enabled": r.enabled,
                            "validity": validity_name(r.validity),
                            "missing": r.missing,
                            "waiting_on": r.waiting_on,
                            "carried": r.counters.as_ref().map(|c| c.messages_sent),
                            "undelivered": r.counters.as_ref().map(|c| c.messages_dropped),
                        })
                    })
                    .collect();
                format.emit(&rendered);
                return ExitCode::Success;
            }
            if response.routes.is_empty() {
                // The message has to reflect the filter, or "no broken connections" reads as
                // "nothing is configured" and sends the user looking for a lost setup.
                if broken {
                    format.line("no connections are broken");
                } else {
                    format.line("no connections configured");
                    format.line("create one with: midi-harbor route create \"<from>\" \"<to>\"");
                }
                return ExitCode::Success;
            }

            let rows: Vec<Vec<String>> = response
                .routes
                .iter()
                .map(|r| {
                    // Missing and waiting never appear together: one says something is gone,
                    // the other that something is here but not running.
                    let counters = r.counters.as_ref();
                    let note = if !r.missing.is_empty() {
                        format!("missing: {}", r.missing.join(", "))
                    } else if !r.waiting_on.is_empty() {
                        format!("waiting for: {}", r.waiting_on.join(", "))
                    } else {
                        // A route whose destination refuses what it is sent is valid, enabled
                        // and ok by every other column, so this is the only place that says so.
                        counters
                            .map(|c| c.messages_dropped)
                            .filter(|dropped| *dropped > 0)
                            .map(|dropped| format!("{dropped} undelivered"))
                            .unwrap_or_default()
                    };
                    vec![
                        route_end(&r.from_name, "MIDI In", r.from_connector),
                        // Two-way routes are marked where the direction is read.
                        if r.both_ways {
                            format!("↔ {}", route_end(&r.to_name, "MIDI Out", r.to_connector))
                        } else {
                            route_end(&r.to_name, "MIDI Out", r.to_connector)
                        },
                        if r.enabled { "on" } else { "off" }.to_owned(),
                        validity_name(r.validity).to_owned(),
                        counters
                            .map(|c| c.messages_sent.to_string())
                            .unwrap_or_default(),
                        note,
                        r.id.clone(),
                    ]
                })
                .collect();
            // Carried sits beside state for the same reason it does on endpoints: a route can be
            // valid, enabled, and carrying nothing, and a state column cannot say so.
            format.line(
                table(
                    &["FROM", "TO", "ENABLED", "STATE", "CARRIED", "NOTE", "ID"],
                    &rows,
                )
                .trim_end(),
            );
            ExitCode::Success
        }

        RouteCommand::Create {
            from,
            to,
            from_connector,
            to_connector,
            both_ways,
        } => {
            match client
                .rpc()
                .create_route(pb::CreateRouteRequest {
                    from: from.clone(),
                    to: to.clone(),
                    from_connector: u32::from(from_connector),
                    to_connector: u32::from(to_connector),
                    both_ways,
                })
                .await
            {
                Ok(response) => {
                    let created = response.into_inner();
                    format.result(serde_json::json!({
                        "id": created.route.as_ref().map(|route| route.id.clone()),
                        "from": from,
                        "to": to,
                        "from_connector": from_connector,
                        "to_connector": to_connector,
                        "both_ways": both_ways,
                        "loop": created.loop_warning,
                    }));
                    format.line(format!(
                        "connected '{}' {} '{}'",
                        route_end(&from, "MIDI In", u32::from(from_connector)),
                        if both_ways { "both ways with" } else { "to" },
                        route_end(&to, "MIDI Out", u32::from(to_connector))
                    ));
                    // A cycle still delivers, because delivery is direct; the user is told
                    // because drawing one is almost always a mistake.
                    if !created.loop_warning.is_empty() {
                        format.note(format!(
                            "this completes a loop: {}",
                            created.loop_warning.join(", ")
                        ));
                    }
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }

        RouteCommand::Edit {
            id,
            from,
            to,
            from_connector,
            to_connector,
            both_ways,
            one_way,
        } => {
            // Start from the route as it is, so only what was given changes.
            let current = match client
                .rpc()
                .list_routes(pb::ListRoutesRequest::default())
                .await
            {
                Ok(response) => response
                    .into_inner()
                    .routes
                    .into_iter()
                    .find(|route| route.id == id),
                Err(status) => return report(&status),
            };
            let Some(current) = current else {
                eprintln!("no route has the identifier {id}; see `midi-harbor route list`");
                return ExitCode::NotFound;
            };
            let request = pb::UpdateRouteRequest {
                id,
                from: from.unwrap_or_else(|| {
                    current.from_id.clone().unwrap_or(current.from_name.clone())
                }),
                to: to.unwrap_or_else(|| current.to_id.clone().unwrap_or(current.to_name.clone())),
                from_connector: from_connector.map_or(current.from_connector, u32::from),
                to_connector: to_connector.map_or(current.to_connector, u32::from),
                both_ways: (current.both_ways || both_ways) && !one_way,
            };
            match client.rpc().update_route(request).await {
                Ok(response) => {
                    let updated = response.into_inner();
                    let route = updated.route.unwrap_or_default();
                    format.result(serde_json::json!({
                        "id": route.id,
                        "from": route.from_name,
                        "to": route.to_name,
                        "from_connector": route.from_connector,
                        "to_connector": route.to_connector,
                        "both_ways": route.both_ways,
                        "loop": updated.loop_warning,
                    }));
                    format.line(format!(
                        "route now connects '{}' {} '{}'",
                        route_end(&route.from_name, "MIDI In", route.from_connector),
                        if route.both_ways {
                            "both ways with"
                        } else {
                            "to"
                        },
                        route_end(&route.to_name, "MIDI Out", route.to_connector)
                    ));
                    if !updated.loop_warning.is_empty() {
                        format.note(format!(
                            "this completes a loop: {}",
                            updated.loop_warning.join(", ")
                        ));
                    }
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }

        RouteCommand::Delete { id } => {
            match client
                .rpc()
                .delete_route(pb::DeleteRouteRequest { id })
                .await
            {
                Ok(_) => {
                    format.line("removed");
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }

        RouteCommand::Enable { id } => set_route(client, format, id, true).await,
        RouteCommand::Disable { id } => set_route(client, format, id, false).await,
    }
}

/// Switches a connection on or off.
async fn set_route(client: &mut Client, format: &Format, id: String, enabled: bool) -> ExitCode {
    match client
        .rpc()
        .set_route_enabled(pb::SetRouteEnabledRequest { id, enabled })
        .await
    {
        Ok(response) => {
            let route = response.into_inner();
            format.line(format!(
                "'{}' to '{}' is now {}",
                route.from_name,
                route.to_name,
                if enabled { "on" } else { "off" }
            ));
            ExitCode::Success
        }
        Err(status) => report(&status),
    }
}

/// Lists attached MIDI hardware.
async fn device(client: &mut Client, format: &Format, command: DeviceCommand) -> ExitCode {
    let all = match command {
        DeviceCommand::List { all } => all,
        DeviceCommand::Enable { target } => return set_enabled(client, format, target, true).await,
        DeviceCommand::Disable { target } => {
            return set_enabled(client, format, target, false).await;
        }
        DeviceCommand::Resolve { device, chosen } => {
            return resolve_device(client, format, device, chosen).await;
        }
        DeviceCommand::Forget { device } => {
            let request = pb::ForgetPhysicalDeviceRequest { id: device };
            return match client.rpc().forget_physical_device(request).await {
                Ok(response) => {
                    let response = response.into_inner();
                    let orphaned = response.orphaned_routes;
                    if response.still_attached {
                        format.line(
                            "forgotten; it is still plugged in, so it has been listed again \
                             as new hardware",
                        );
                    } else {
                        format.line("forgotten");
                    }
                    // Named rather than counted: a route that has stopped working is worth
                    // knowing about by name, and it is still there to be repaired or removed.
                    if !orphaned.is_empty() {
                        format.note(format!(
                            "these connections have nothing to carry now: {}",
                            orphaned.join(", ")
                        ));
                    }
                    ExitCode::Success
                }
                Err(status) => report(&status),
            };
        }
    };

    let kind = Some(pb::EndpointKind::PhysicalDevice as i32);
    let response = match client
        .rpc()
        .list_endpoints(pb::ListEndpointsRequest {
            kind,
            include_absent: all,
        })
        .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return report(&status),
    };

    let listed: Vec<&pb::Endpoint> = response
        .endpoints
        .iter()
        .filter(|endpoint| {
            all || matches!(
                &endpoint.detail,
                Some(pb::endpoint::Detail::PhysicalDevice(device)) if device.present
            )
        })
        .collect();

    if format.json {
        let rendered: Vec<_> = listed
            .iter()
            .map(|e| {
                let present = matches!(
                    &e.detail,
                    Some(pb::endpoint::Detail::PhysicalDevice(d)) if d.present
                );
                serde_json::json!({
                    "id": e.id,
                    "name": e.name,
                    "kind": endpoint_kind(e),
                    "present": present,
                    "enabled": e.enabled,
                })
            })
            .collect();
        format.emit(&rendered);
        return ExitCode::Success;
    }
    if listed.is_empty() {
        format.line("no MIDI hardware attached");
        return ExitCode::Success;
    }

    let rows: Vec<Vec<String>> = listed
        .iter()
        .map(|e| {
            let held = match &e.detail {
                Some(pb::endpoint::Detail::PhysicalDevice(device)) => {
                    device.claimed_by.clone().unwrap_or_default()
                }
                _ => String::new(),
            };
            vec![
                e.name.clone(),
                endpoint_kind(e).to_owned(),
                endpoint_state(e),
                held,
            ]
        })
        .collect();
    format.line(table(&["DEVICE", "KIND", "STATE", "IN USE BY"], &rows).trim_end());
    ExitCode::Success
}

/// Runs one Bluetooth command.
async fn bluetooth(client: &mut Client, format: &Format, command: BluetoothCommand) -> ExitCode {
    match command {
        BluetoothCommand::Enable { target } => set_enabled(client, format, target, true).await,
        BluetoothCommand::Disable { target } => set_enabled(client, format, target, false).await,
        BluetoothCommand::Scan { seconds } => {
            let request = pb::StartBluetoothScanRequest {
                duration_seconds: seconds,
            };
            if let Err(status) = client.rpc().start_bluetooth_scan(request).await {
                return refused(format, &status);
            }
            // Waiting here rather than returning immediately is the whole point of the command:
            // a scan that returns before it has heard anything has nothing to show.
            format.note(format!("listening for {seconds} seconds"));
            tokio::time::sleep(std::time::Duration::from_secs(u64::from(seconds))).await;
            let _ = client
                .rpc()
                .stop_bluetooth_scan(pb::StopBluetoothScanRequest {})
                .await;
            list_bluetooth(client, format).await
        }
        BluetoothCommand::List => list_bluetooth(client, format).await,
        BluetoothCommand::Connect { address } => {
            let request = pb::ConnectBluetoothDeviceRequest {
                address: address.clone(),
            };
            match client.rpc().connect_bluetooth_device(request).await {
                Ok(response) => {
                    let endpoint = response.into_inner();
                    format.result(endpoint_result(&endpoint));
                    await_link(client, format, &endpoint).await
                }
                // An address the radio cannot hear now. "Does not exist" read as a mistyped
                // address, when the device had usually gone quiet or not been scanned for yet.
                Err(status) if status.code() == tonic::Code::NotFound => {
                    eprintln!("{address} is not in range");
                    format
                        .note("scan for it with 'midi-harbor bluetooth scan', then connect again");
                    ExitCode::NotFound
                }
                Err(status) => refused(format, &status),
            }
        }
        BluetoothCommand::Disconnect { device } => {
            let request = pb::DisconnectBluetoothDeviceRequest { id: device };
            match client.rpc().disconnect_bluetooth_device(request).await {
                Ok(response) => {
                    format.line(format!("disconnected {}", response.into_inner().name));
                    format.note("it is still remembered, so it will reconnect when it returns");
                    ExitCode::Success
                }
                Err(status) => refused(format, &status),
            }
        }
        BluetoothCommand::Forget { device } => {
            let request = pb::ForgetBluetoothDeviceRequest { id: device };
            match client.rpc().forget_bluetooth_device(request).await {
                Ok(_) => {
                    format.line("forgotten");
                    ExitCode::Success
                }
                Err(status) => refused(format, &status),
            }
        }
        BluetoothCommand::Advertise { off, name } => {
            let request = pb::SetPeripheralAdvertisingRequest {
                enabled: !off,
                name,
            };
            match client.rpc().set_peripheral_advertising(request).await {
                Ok(response) => {
                    let response = response.into_inner();
                    if response.advertising {
                        format
                            .line("advertising; this machine is now visible to phones and tablets");
                        if !response.name_sent {
                            let room = midi_harbor_platform::bluetooth::ADVERTISED_NAME_ROOM
                                .unwrap_or_default();
                            format.note(format!(
                                "'{}' does not fit beside the MIDI service, which leaves room for \
                                 {room} bytes of name here, so phones and tablets will show this \
                                 machine's own name; use --name with {room} characters or fewer \
                                 to have one shown",
                                response.name
                            ));
                        }
                    } else {
                        format.line("no longer advertising");
                    }
                    ExitCode::Success
                }
                Err(status) => refused(format, &status),
            }
        }
    }
}

/// How long `bluetooth connect` waits for the link: a little past the radio's own ten seconds.
const LINK_WAIT: std::time::Duration = std::time::Duration::from_secs(12);

/// Waits for a requested Bluetooth link to come up or fail, and says which.
///
/// The daemon answers once the link is asked for, and the radio answers later. Reporting
/// "connected" at the first answer told the user a link was up that then failed, and exited 0.
async fn await_link(client: &mut Client, format: &Format, endpoint: &pb::Endpoint) -> ExitCode {
    let started = std::time::Instant::now();
    while started.elapsed() < LINK_WAIT {
        let current = match client
            .rpc()
            .get_endpoint(pb::GetEndpointRequest {
                id: endpoint.id.clone(),
            })
            .await
        {
            Ok(response) => response.into_inner(),
            Err(status) => return report(&status),
        };
        let state = current.state.as_ref();
        let phase = state.and_then(|state| pb::ConnectionPhase::try_from(state.phase).ok());
        match phase {
            Some(pb::ConnectionPhase::Connected) => {
                format.line(format!(
                    "connected {}; it will reconnect on its own when it comes back",
                    endpoint.name
                ));
                return ExitCode::Success;
            }
            Some(pb::ConnectionPhase::Retrying | pb::ConnectionPhase::Unavailable) => {
                let reason = state
                    .and_then(|state| state.last_error.as_ref())
                    .map(|error| error.message.clone())
                    .unwrap_or_else(|| "it did not answer".to_owned());
                eprintln!("could not connect {}: {reason}", endpoint.name);
                format.note(
                    "it is remembered, so it will be tried again when it is next heard; the \
                     daemon's log has the radio's own reason",
                );
                return ExitCode::Failure;
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
        }
    }
    format.line(format!(
        "still connecting to {}; follow it with 'midi-harbor status --watch'",
        endpoint.name
    ));
    ExitCode::Success
}

/// Reports a refused Bluetooth request, saying where the specific reason can be read.
///
/// The failure reason is a closed set shared with every other endpoint kind, so it can say that
/// the adapter is unavailable but not whether that means absent, switched off, or unpermitted.
/// The capability query keeps that distinction, and it is the difference between buying an
/// adapter and granting a permission.
fn refused(format: &Format, status: &tonic::Status) -> ExitCode {
    let code = report(status);
    if code == ExitCode::Unavailable {
        format.note("run 'midi-harbor capabilities' to see why Bluetooth is unavailable here");
    }
    code
}

/// Shows what the radio can currently hear.
async fn list_bluetooth(client: &mut Client, format: &Format) -> ExitCode {
    let response = match client
        .rpc()
        .list_bluetooth_devices(pb::ListBluetoothDevicesRequest {})
        .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return refused(format, &status),
    };

    if format.json {
        let rendered: Vec<_> = response
            .devices
            .iter()
            .map(|device| {
                serde_json::json!({
                    "address": device.address,
                    "name": device.name,
                    "rssi": device.rssi,
                    "paired": device.paired,
                    "endpoint_id": device.endpoint_id,
                })
            })
            .collect();
        format.emit(&rendered);
        return ExitCode::Success;
    }
    if response.devices.is_empty() {
        format.line("no Bluetooth MIDI devices in range");
        return ExitCode::Success;
    }

    let rows: Vec<Vec<String>> = response
        .devices
        .iter()
        .map(|device| {
            vec![
                device.name.clone().unwrap_or_else(|| "-".to_owned()),
                device.address.clone(),
                // Signal strength is the only distance information BLE offers, and the
                // difference between -50 and -90 is the difference between working and not.
                device
                    .rssi
                    .map(|rssi| format!("{rssi} dBm"))
                    .unwrap_or_default(),
                if device.paired { "paired" } else { "new" }.to_owned(),
            ]
        })
        .collect();
    format.line(table(&["DEVICE", "ADDRESS", "SIGNAL", "STATE"], &rows).trim_end());
    ExitCode::Success
}

/// Copies a Bluetooth command, since dispatch needs to move it into an async body.
fn clone_bluetooth_command(command: &BluetoothCommand) -> BluetoothCommand {
    match command {
        BluetoothCommand::Scan { seconds } => BluetoothCommand::Scan { seconds: *seconds },
        BluetoothCommand::List => BluetoothCommand::List,
        BluetoothCommand::Connect { address } => BluetoothCommand::Connect {
            address: address.clone(),
        },
        BluetoothCommand::Disconnect { device } => BluetoothCommand::Disconnect {
            device: device.clone(),
        },
        BluetoothCommand::Enable { target } => BluetoothCommand::Enable {
            target: target.clone(),
        },
        BluetoothCommand::Disable { target } => BluetoothCommand::Disable {
            target: target.clone(),
        },
        BluetoothCommand::Forget { device } => BluetoothCommand::Forget {
            device: device.clone(),
        },
        BluetoothCommand::Advertise { off, name } => BluetoothCommand::Advertise {
            off: *off,
            name: name.clone(),
        },
    }
}

/// Copies a connection command, since dispatch needs to move it into an async body.
fn clone_route_command(command: &RouteCommand) -> RouteCommand {
    match command {
        RouteCommand::List { broken } => RouteCommand::List { broken: *broken },
        RouteCommand::Create {
            from,
            to,
            from_connector,
            to_connector,
            both_ways,
        } => RouteCommand::Create {
            from: from.clone(),
            to: to.clone(),
            from_connector: *from_connector,
            to_connector: *to_connector,
            both_ways: *both_ways,
        },
        RouteCommand::Edit {
            id,
            from,
            to,
            from_connector,
            to_connector,
            both_ways,
            one_way,
        } => RouteCommand::Edit {
            id: id.clone(),
            from: from.clone(),
            to: to.clone(),
            from_connector: *from_connector,
            to_connector: *to_connector,
            both_ways: *both_ways,
            one_way: *one_way,
        },
        RouteCommand::Delete { id } => RouteCommand::Delete { id: id.clone() },
        RouteCommand::Enable { id } => RouteCommand::Enable { id: id.clone() },
        RouteCommand::Disable { id } => RouteCommand::Disable { id: id.clone() },
    }
}

/// Returns a readable name for a wire route validity.
fn validity_name(value: i32) -> &'static str {
    match pb::RouteValidity::try_from(value) {
        Ok(pb::RouteValidity::Valid) => "ok",
        Ok(pb::RouteValidity::Broken) => "broken",
        Ok(pb::RouteValidity::LoopDetected) => "loop",
        Ok(pb::RouteValidity::Suspended) => "waiting",
        _ => "unknown",
    }
}

/// Runs a network session subcommand.
async fn session(client: &mut Client, format: &Format, command: NetworkCommand) -> ExitCode {
    match command {
        NetworkCommand::Enable { target } => set_enabled(client, format, target, true).await,
        NetworkCommand::Disable { target } => set_enabled(client, format, target, false).await,
        NetworkCommand::List => {
            let kind = Some(pb::EndpointKind::NetworkSession as i32);
            let response = match client
                .rpc()
                .list_endpoints(pb::ListEndpointsRequest {
                    kind,
                    include_absent: true,
                })
                .await
            {
                Ok(response) => response.into_inner(),
                Err(status) => return report(&status),
            };

            if format.json {
                let rendered: Vec<_> = response
                    .endpoints
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "id": e.id,
                            "name": e.name,
                            // Described as `status` describes it, so the two never disagree about
                            // one session: an idle one is listening, not disconnected.
                            "state": endpoint_state(e),
                            "guests": session_guests(e),
                        })
                    })
                    .collect();
                format.emit(&rendered);
                return ExitCode::Success;
            }
            if response.endpoints.is_empty() {
                format.line("no network ports configured");
                format.line("create one with: midi-harbor network create \"Studio\"");
                return ExitCode::Success;
            }

            let rows: Vec<Vec<String>> = response
                .endpoints
                .iter()
                .map(|e| {
                    let detail = match &e.detail {
                        Some(pb::endpoint::Detail::NetworkSession(session)) => {
                            session.control_port.to_string()
                        }
                        _ => "-".to_owned(),
                    };
                    let automatic = match &e.detail {
                        Some(pb::endpoint::Detail::NetworkSession(session))
                            if session.automatic_port =>
                        {
                            "on"
                        }
                        _ => "off",
                    };
                    vec![
                        e.name.clone(),
                        detail,
                        automatic.to_owned(),
                        endpoint_state(e),
                        session_guests(e).join(", "),
                    ]
                })
                .collect();
            format.line(
                table(
                    &["NAME", "PORT", "AUTOMATIC PORT", "STATE", "ALSO WITH"],
                    &rows,
                )
                .trim_end(),
            );
            ExitCode::Success
        }

        NetworkCommand::Create {
            name,
            port,
            policy,
            no_automatic_port,
            bonjour_name,
        } => {
            let request = pb::CreateNetworkSessionRequest {
                name,
                control_port: u32::from(port),
                // Unspecified when not given, so the daemon applies the configured default.
                invitation_policy: policy.map_or(0, crate::commands::PolicyArg::to_proto),
                automatic_port: Some(!no_automatic_port),
                local_name: bonjour_name,
            };
            match client.rpc().create_network_session(request).await {
                Ok(response) => {
                    let endpoint = response.into_inner();
                    // The port is only known once the session has bound, which has not happened
                    // by the time create returns. Reporting the unbound zero would print a port
                    // number that is not the one 'session list' shows a moment later.
                    let listening = match &endpoint.detail {
                        Some(pb::endpoint::Detail::NetworkSession(session))
                            if session.control_port != 0 =>
                        {
                            format!(" on port {}", session.control_port)
                        }
                        _ => String::new(),
                    };
                    format.result(endpoint_result(&endpoint));
                    format.line(format!(
                        "created '{}'{listening}; other machines can find it now",
                        endpoint.name
                    ));
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }

        NetworkCommand::Discover => {
            let response = match client.rpc().list_peers(pb::ListPeersRequest {}).await {
                Ok(response) => response.into_inner(),
                Err(status) => return report(&status),
            };

            if format.json {
                let rendered: Vec<_> = response
                    .peers
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "name": p.advertised_name,
                            "addresses": p.addresses,
                            "discovered": p.discovered,
                            "trusted": p.trusted,
                        })
                    })
                    .collect();
                format.emit(&rendered);
                return ExitCode::Success;
            }
            if response.peers.is_empty() {
                format.line("no machines found advertising network MIDI, and none remembered");
                return ExitCode::Success;
            }

            // Remembered machines are listed beside the ones advertising now. A machine the
            // user has trusted is worth seeing whether or not it is switched on this minute, and
            // it cannot be forgotten if it cannot be seen.
            let rows: Vec<Vec<String>> = response
                .peers
                .iter()
                .map(|p| {
                    vec![
                        p.advertised_name.clone(),
                        p.addresses
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "-".to_owned()),
                        if p.discovered { "here" } else { "remembered" }.to_owned(),
                        if p.trusted { "yes" } else { "" }.to_owned(),
                    ]
                })
                .collect();
            format.line(table(&["PEER", "ADDRESS", "SEEN", "TRUSTED"], &rows).trim_end());
            ExitCode::Success
        }

        NetworkCommand::Connect {
            session,
            peer,
            alongside,
        } => {
            let request = pb::ConnectPeerRequest {
                session_endpoint_id: session,
                peer_id: peer.clone(),
                alongside,
            };
            match client.rpc().connect_peer(request).await {
                Ok(response) => {
                    format.line(format!(
                        "connecting '{}' to {peer}; watch progress with: midi-harbor status",
                        response.into_inner().name
                    ));
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }

        NetworkCommand::Disconnect {
            session,
            machine: Some(machine),
        } => {
            let request = pb::DisconnectMachineRequest {
                session_endpoint_id: session,
                address: machine.clone(),
            };
            match client.rpc().disconnect_machine(request).await {
                Ok(response) => {
                    format.line(format!(
                        "disconnected {machine} from '{}'",
                        response.into_inner().name
                    ));
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }

        NetworkCommand::Disconnect {
            session,
            machine: None,
        } => {
            let request = pb::DisconnectPeerRequest {
                session_endpoint_id: session,
            };
            match client.rpc().disconnect_peer(request).await {
                Ok(response) => {
                    format.line(format!("disconnected '{}'", response.into_inner().name));
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }

        NetworkCommand::Delete { session, yes: _ } => {
            match client
                .rpc()
                .delete_network_port(pb::DeleteNetworkPortRequest { id: session })
                .await
            {
                Ok(response) => {
                    let orphaned = response.into_inner().orphaned_routes;
                    format.line("deleted");
                    for route in orphaned {
                        format.note(format!("route '{route}' now has a missing endpoint"));
                    }
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }

        NetworkCommand::Machines { session } => {
            let kind = Some(pb::EndpointKind::NetworkSession as i32);
            let endpoints = match client
                .rpc()
                .list_endpoints(pb::ListEndpointsRequest {
                    kind,
                    include_absent: true,
                })
                .await
            {
                Ok(response) => response.into_inner().endpoints,
                Err(status) => return report(&status),
            };
            let Some(endpoint) = endpoints
                .iter()
                .find(|e| e.id == session || e.name == session)
            else {
                eprintln!("no network port is called {session}; see `midi-harbor network list`");
                return ExitCode::NotFound;
            };
            let machines = match &endpoint.detail {
                Some(pb::endpoint::Detail::NetworkSession(detail)) => detail.machines.clone(),
                _ => Vec::new(),
            };
            if format.json {
                let rendered: Vec<_> = machines
                    .iter()
                    .map(|machine| {
                        serde_json::json!({
                            "address": machine.address,
                            "name": machine.name,
                            "invited": machine.invited,
                            "joined": machine.joined,
                            "round_trip_us": machine.round_trip_us,
                        })
                    })
                    .collect();
                format.result(serde_json::Value::Array(rendered));
                return ExitCode::Success;
            }
            if machines.is_empty() {
                format.line(format!("no machine is connected to '{}'", endpoint.name));
                return ExitCode::Success;
            }
            let rows: Vec<Vec<String>> = machines
                .iter()
                .map(|machine| {
                    vec![
                        machine.name.clone().unwrap_or_else(|| "-".to_owned()),
                        machine.address.clone(),
                        if machine.invited {
                            "connected by this machine"
                        } else {
                            "connected to this machine"
                        }
                        .to_owned(),
                        if machine.joined { "joined" } else { "joining" }.to_owned(),
                        machine.round_trip_us.map_or_else(
                            || "-".to_owned(),
                            |micros| format!("{:.1} ms", micros as f64 / 1000.0),
                        ),
                    ]
                })
                .collect();
            format.line(table(&["NAME", "ADDRESS", "HOW", "STATE", "LATENCY"], &rows).trim_end());
            ExitCode::Success
        }

        NetworkCommand::Invitations => {
            // The stream replays what is already waiting and then ends the moment it has caught
            // up, because this command answers "who is knocking" rather than watching the door.
            let mut stream = match client
                .rpc()
                .watch_invitations(pb::WatchInvitationsRequest {})
                .await
            {
                Ok(response) => response.into_inner(),
                Err(status) => return report(&status),
            };

            let mut waiting = Vec::new();
            loop {
                match tokio::time::timeout(std::time::Duration::from_millis(250), stream.message())
                    .await
                {
                    Ok(Ok(Some(invitation))) => waiting.push(invitation),
                    // Caught up, or the stream ended; either way there is nothing more waiting.
                    Ok(Ok(None)) | Err(_) => break,
                    Ok(Err(status)) => return report(&status),
                }
            }

            if format.json {
                let rendered: Vec<_> = waiting
                    .iter()
                    .map(|invitation| {
                        serde_json::json!({
                            "id": invitation.invitation_id,
                            "peer": invitation.peer_name,
                            "address": invitation.peer_address,
                        })
                    })
                    .collect();
                format.emit(&rendered);
                return ExitCode::Success;
            }
            if waiting.is_empty() {
                format.line("no machines are waiting to be let in");
                return ExitCode::Success;
            }

            let rows: Vec<Vec<String>> = waiting
                .iter()
                .map(|invitation| {
                    vec![
                        invitation.peer_name.clone(),
                        invitation.peer_address.clone(),
                        invitation.invitation_id.clone(),
                    ]
                })
                .collect();
            format.line(table(&["MACHINE", "ADDRESS", "ID"], &rows).trim_end());
            format.line("");
            format.line("let one in with: midi-harbor network respond <ID> --accept --always");
            ExitCode::Success
        }

        NetworkCommand::Respond {
            invitation,
            accept,
            refuse,
            always,
        } => {
            if accept == refuse {
                // Neither given, or both: refusing to guess is the only safe reading of "answer
                // this connection request" when the answer is missing.
                eprintln!("say which: --accept or --refuse");
                return ExitCode::Usage;
            }
            let request = pb::RespondToInvitationRequest {
                invitation_id: invitation,
                accept,
                always,
            };
            match client.rpc().respond_to_invitation(request).await {
                Ok(_) if accept && always => {
                    format.line("let in, and remembered for next time");
                    ExitCode::Success
                }
                Ok(_) if accept => {
                    format.line("let in for as long as the daemon is running");
                    ExitCode::Success
                }
                Ok(_) => {
                    format.line("turned away");
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }

        NetworkCommand::Peer(PeerCommand::Add {
            address,
            name,
            no_trust,
        }) => {
            let request = pb::AddManualPeerRequest {
                address,
                // The address may already carry a port, so none is added here.
                port: 0,
                name,
                trusted: Some(!no_trust),
            };
            match client.rpc().add_manual_peer(request).await {
                Ok(response) => {
                    let peer = response.into_inner();
                    format.result(serde_json::json!({
                        "id": peer.id,
                        "name": peer.advertised_name,
                        "addresses": peer.addresses,
                    }));
                    format.line(trust_line(&peer));
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }

        NetworkCommand::Peer(PeerCommand::Trust { peer, state }) => {
            let request = pb::SetPeerTrustedRequest {
                peer_id: peer,
                trusted: state == crate::commands::Switch::On,
            };
            match client.rpc().set_peer_trusted(request).await {
                Ok(response) => {
                    let peer = response.into_inner();
                    format.result(serde_json::json!({
                        "id": peer.id,
                        "name": peer.advertised_name,
                        "trusted": peer.trusted,
                    }));
                    format.line(trust_line(&peer));
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }

        NetworkCommand::Peer(PeerCommand::Remove { peer }) => {
            match client
                .rpc()
                .remove_peer(pb::RemovePeerRequest { peer_id: peer })
                .await
            {
                Ok(_) => {
                    format.line("forgotten; invitations from it will be asked about again");
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }

        NetworkCommand::Edit {
            session,
            bonjour_name,
            udp_port,
            policy,
            automatic_port,
        } => {
            let request = pb::UpdateNetworkPortRequest {
                id: session,
                automatic_port: automatic_port.map(|on| on == crate::commands::Switch::On),
                local_name: bonjour_name,
                control_port: udp_port.map(u32::from),
                invitation_policy: policy.map(crate::commands::PolicyArg::to_proto),
            };
            match client.rpc().update_network_port(request).await {
                Ok(response) => {
                    let endpoint = response.into_inner();
                    format.result(endpoint_result(&endpoint));
                    if let Some(pb::endpoint::Detail::NetworkSession(held)) = &endpoint.detail {
                        let automatic = if held.automatic_port { "on" } else { "off" };
                        format.line(format!(
                            "'{}': other machines see '{}', UDP port {}, automatic port {automatic}",
                            endpoint.name, held.local_name, held.control_port
                        ));
                    }
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }

        NetworkCommand::Policy { session, policy } => {
            let request = pb::SetInvitationPolicyRequest {
                session_endpoint_id: session,
                policy: policy.to_proto(),
            };
            match client.rpc().set_invitation_policy(request).await {
                Ok(response) => {
                    let endpoint = response.into_inner();
                    format.line(format!(
                        "'{}' now treats invitations as: {}",
                        endpoint.name,
                        policy_name(policy)
                    ));
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }
    }
}

/// Says whether a known machine is let in without being asked about.
fn trust_line(peer: &pb::Peer) -> String {
    if peer.trusted {
        format!(
            "'{}' will be let in without being asked about",
            peer.advertised_name
        )
    } else {
        format!(
            "'{}' is remembered; its invitations will be asked about",
            peer.advertised_name
        )
    }
}

/// Returns the wording for a policy, matching what the command accepts.
fn policy_name(policy: crate::commands::PolicyArg) -> &'static str {
    match policy {
        crate::commands::PolicyArg::Prompt => "ask first",
        crate::commands::PolicyArg::Known => "accept machines already trusted",
        crate::commands::PolicyArg::All => "accept anyone",
        crate::commands::PolicyArg::Reject => "refuse everything",
    }
}

/// Copies a session command, since dispatch needs to move it into an async body.
fn clone_session_command(command: &NetworkCommand) -> NetworkCommand {
    match command {
        NetworkCommand::List => NetworkCommand::List,
        NetworkCommand::Create {
            name,
            port,
            policy,
            no_automatic_port,
            bonjour_name,
        } => NetworkCommand::Create {
            name: name.clone(),
            port: *port,
            policy: *policy,
            no_automatic_port: *no_automatic_port,
            bonjour_name: bonjour_name.clone(),
        },
        NetworkCommand::Edit {
            session,
            bonjour_name,
            udp_port,
            policy,
            automatic_port,
        } => NetworkCommand::Edit {
            session: session.clone(),
            bonjour_name: bonjour_name.clone(),
            udp_port: *udp_port,
            policy: *policy,
            automatic_port: *automatic_port,
        },
        NetworkCommand::Discover => NetworkCommand::Discover,
        NetworkCommand::Enable { target } => NetworkCommand::Enable {
            target: target.clone(),
        },
        NetworkCommand::Disable { target } => NetworkCommand::Disable {
            target: target.clone(),
        },
        NetworkCommand::Connect {
            session,
            peer,
            alongside,
        } => NetworkCommand::Connect {
            session: session.clone(),
            peer: peer.clone(),
            alongside: *alongside,
        },
        NetworkCommand::Disconnect { session, machine } => NetworkCommand::Disconnect {
            session: session.clone(),
            machine: machine.clone(),
        },
        NetworkCommand::Machines { session } => NetworkCommand::Machines {
            session: session.clone(),
        },
        NetworkCommand::Delete { session, yes } => NetworkCommand::Delete {
            session: session.clone(),
            yes: *yes,
        },
        NetworkCommand::Invitations => NetworkCommand::Invitations,
        NetworkCommand::Respond {
            invitation,
            accept,
            refuse,
            always,
        } => NetworkCommand::Respond {
            invitation: invitation.clone(),
            accept: *accept,
            refuse: *refuse,
            always: *always,
        },
        NetworkCommand::Peer(PeerCommand::Add {
            address,
            name,
            no_trust,
        }) => NetworkCommand::Peer(PeerCommand::Add {
            address: address.clone(),
            name: name.clone(),
            no_trust: *no_trust,
        }),
        NetworkCommand::Peer(PeerCommand::Trust { peer, state }) => {
            NetworkCommand::Peer(PeerCommand::Trust {
                peer: peer.clone(),
                state: *state,
            })
        }
        NetworkCommand::Peer(PeerCommand::Remove { peer }) => {
            NetworkCommand::Peer(PeerCommand::Remove { peer: peer.clone() })
        }
        NetworkCommand::Policy { session, policy } => NetworkCommand::Policy {
            session: session.clone(),
            policy: *policy,
        },
    }
}

/// Exports, imports or reloads configuration through the daemon.
async fn transfer_config(client: &mut Client, format: &Format, command: ConfigCommand) -> ExitCode {
    match command {
        ConfigCommand::Export { output } => {
            let yaml = match client
                .rpc()
                .export_configuration(pb::ExportConfigurationRequest {})
                .await
            {
                Ok(response) => response.into_inner().yaml,
                Err(status) => return report(&status),
            };
            match output {
                None => {
                    print!("{yaml}");
                    ExitCode::Success
                }
                Some(path) => match std::fs::write(&path, yaml) {
                    Ok(()) => {
                        format.line(format!("configuration written to {}", path.display()));
                        ExitCode::Success
                    }
                    Err(error) => {
                        eprintln!("could not write {}: {error}", path.display());
                        ExitCode::Failure
                    }
                },
            }
        }
        // A replacing import without --yes was refused before connecting.
        ConfigCommand::Import { path, mode, yes: _ } => {
            let replace = mode == ImportMode::Replace;
            let yaml = match std::fs::read_to_string(&path) {
                Ok(yaml) => yaml,
                Err(error) => {
                    eprintln!("could not read {}: {error}", path.display());
                    return ExitCode::Failure;
                }
            };
            match client
                .rpc()
                .import_configuration(pb::ImportConfigurationRequest { yaml, replace })
                .await
            {
                Ok(response) => {
                    let applied = response.into_inner();
                    format.line(format!(
                        "imported: {} and {} added",
                        count(applied.endpoints_added as usize, "endpoint"),
                        count(applied.routes_added as usize, "route")
                    ));
                    report_disturbed(
                        format,
                        &applied.restarted_endpoints,
                        &applied.removed_endpoints,
                        &applied.pending_restart,
                    );
                    ExitCode::Success
                }
                Err(status) => report(&status),
            }
        }
        ConfigCommand::Reload => match client
            .rpc()
            .reload_configuration(pb::ReloadConfigurationRequest {})
            .await
        {
            Ok(response) => {
                let applied = response.into_inner();
                let untouched = applied.added_endpoints.is_empty()
                    && applied.removed_endpoints.is_empty()
                    && applied.restarted_endpoints.is_empty()
                    && applied.routes_added == 0;
                if untouched {
                    format.line("reloaded; no connection needed changing");
                } else {
                    format.line(format!(
                        "reloaded: {} and {} added",
                        count(applied.added_endpoints.len(), "endpoint"),
                        count(applied.routes_added as usize, "route")
                    ));
                }
                report_disturbed(
                    format,
                    &applied.restarted_endpoints,
                    &applied.removed_endpoints,
                    &applied.pending_restart,
                );
                ExitCode::Success
            }
            Err(status) => report(&status),
        },
        // These need no daemon, and dispatch handles them without connecting.
        ConfigCommand::Path | ConfigCommand::Show | ConfigCommand::ImportApple { .. } => {
            ExitCode::Usage
        }
    }
}

/// Formats a count with its noun, which takes an `s` for anything but one.
fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Names what applying a configuration disturbed, and what waits for a restart.
fn report_disturbed(format: &Format, restarted: &[String], removed: &[String], pending: &[String]) {
    for name in restarted {
        format.note(format!(
            "'{name}' was reopened because its settings changed"
        ));
    }
    for name in removed {
        format.note(format!("'{name}' was removed"));
    }
    for setting in pending {
        format.note(format!(
            "the {setting} takes effect when the daemon next starts"
        ));
    }
}

/// Shows configuration locations and contents.
fn config(command: &ConfigCommand, format: &Format) -> ExitCode {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::Failure;
        }
    };

    match command {
        ConfigCommand::Path => {
            format.line(paths.config_file().display().to_string());
            ExitCode::Success
        }
        ConfigCommand::Show => match std::fs::read_to_string(paths.config_file()) {
            Ok(text) => {
                format.line(text.trim_end());
                ExitCode::Success
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                format.note("no configuration file exists yet");
                ExitCode::Success
            }
            Err(error) => {
                eprintln!("could not read {}: {error}", paths.config_file().display());
                ExitCode::Failure
            }
        },
        // These need the daemon, and dispatch sends them there rather than here.
        ConfigCommand::Export { .. }
        | ConfigCommand::Import { .. }
        | ConfigCommand::Reload
        | ConfigCommand::ImportApple { .. } => ExitCode::Usage,
    }
}

/// What importing Apple's setup would create, and what it leaves because it is already here.
#[derive(Debug, Default, PartialEq, Eq)]
struct ApplePlan {
    ports: Vec<String>,
    sessions: Vec<String>,
    already_here: Vec<String>,
    /// Names the other kind already has, which the daemon would refuse.
    name_taken: Vec<String>,
}

/// Decides what to create from Apple's setup, given the names this machine already uses.
///
/// A name already taken is left alone rather than made unique, because a second port called
/// "Bus 1 (2)" is not what anyone moving over from the IAC Driver wants. A port and a network
/// port may not share a name either, since other applications see both as ports.
fn plan_apple_import(
    setup: &pb::ReadAppleSetupResponse,
    ports_here: &[String],
    sessions_here: &[String],
) -> ApplePlan {
    let mut plan = ApplePlan::default();
    for bus in &setup.buses {
        if ports_here.contains(bus) || plan.ports.contains(bus) {
            plan.already_here.push(bus.clone());
        } else if sessions_here.contains(bus) {
            plan.name_taken.push(bus.clone());
        } else {
            plan.ports.push(bus.clone());
        }
    }
    for session in &setup.sessions {
        if sessions_here.contains(session) || plan.sessions.contains(session) {
            plan.already_here.push(session.clone());
        } else if ports_here.contains(session) || plan.ports.contains(session) {
            plan.name_taken.push(session.clone());
        } else {
            plan.sessions.push(session.clone());
        }
    }
    plan
}

/// Shows what importing Apple's setup would create, and creates it when confirmed.
async fn import_apple(client: &mut Client, format: &Format, yes: bool) -> ExitCode {
    // Read Apple's setup, which the daemon does so that no client opens CoreMIDI itself.
    let setup = match client
        .rpc()
        .read_apple_setup(pb::ReadAppleSetupRequest {})
        .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return report(&status),
    };
    if !setup.available {
        eprintln!("there is no Apple MIDI setup to import on this platform");
        return ExitCode::Unavailable;
    }

    // Learn what is here already.
    let endpoints = match client
        .rpc()
        .list_endpoints(pb::ListEndpointsRequest {
            kind: None,
            include_absent: true,
        })
        .await
    {
        Ok(response) => response.into_inner().endpoints,
        Err(status) => return report(&status),
    };
    let named = |kind: pb::EndpointKind| -> Vec<String> {
        endpoints
            .iter()
            .filter(|endpoint| endpoint.kind == kind as i32)
            .map(|endpoint| endpoint.name.clone())
            .collect()
    };
    let plan = plan_apple_import(
        &setup,
        &named(pb::EndpointKind::VirtualPort),
        &named(pb::EndpointKind::NetworkSession),
    );

    // Show the plan.
    let name_taken = format!(
        "left out, a port and a network port cannot share a name: {}",
        plan.name_taken.join(", ")
    );
    if plan.ports.is_empty() && plan.sessions.is_empty() {
        format.line("nothing to import from Apple's MIDI setup");
        if !plan.already_here.is_empty() {
            format.note(format!("already here: {}", plan.already_here.join(", ")));
        }
        if !plan.name_taken.is_empty() {
            format.note(name_taken);
        }
        return ExitCode::Success;
    }
    let list = |names: &[String]| {
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        }
    };
    format.line(format!("virtual ports to create: {}", list(&plan.ports)));
    format.line(format!("network ports to create: {}", list(&plan.sessions)));
    if !plan.already_here.is_empty() {
        format.line(format!(
            "already here, left alone: {}",
            list(&plan.already_here)
        ));
    }
    if !plan.name_taken.is_empty() {
        format.line(name_taken);
    }
    if !plan.sessions.is_empty() {
        format.note(
            "the network ports get UDP ports of their own, since Apple's session holds the one it uses",
        );
    }
    if setup.iac_online && !plan.ports.is_empty() {
        format.note(
            "the IAC Driver is switched on, so applications will see its buses as well as these \
             ports; switch it off in Audio MIDI Setup once you have moved over",
        );
    }
    if !yes {
        format.note("nothing has been changed; run again with --yes to create them");
        return ExitCode::ConfirmationRequired;
    }

    // Create what was listed.
    let mut failed = false;
    for name in plan.ports {
        // One of each, as an IAC bus has.
        let request = pb::CreateVirtualPortRequest {
            name: name.clone(),
            direction: pb::Direction::Bidirectional as i32,
            inputs: 1,
            outputs: 1,
        };
        match client.rpc().create_virtual_port(request).await {
            Ok(_) => format.line(format!("created port '{name}'")),
            Err(status) => {
                eprintln!("could not create port '{name}': {}", status.message());
                failed = true;
            }
        }
    }
    for name in plan.sessions {
        let request = pb::CreateNetworkSessionRequest {
            name: name.clone(),
            control_port: 0,
            invitation_policy: pb::InvitationPolicy::Unspecified as i32,
            automatic_port: None,
            local_name: None,
        };
        match client.rpc().create_network_session(request).await {
            Ok(_) => format.line(format!("created network port '{name}'")),
            Err(status) => {
                eprintln!(
                    "could not create network port '{name}': {}",
                    status.message()
                );
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::Failure
    } else {
        ExitCode::Success
    }
}

/// Reports a daemon failure with its guidance, and returns the matching exit code.
fn report(status: &tonic::Status) -> ExitCode {
    eprintln!("{}", status.message());
    if let Some(guidance) = midi_harbor_ipc::status::reason_guidance(status) {
        eprintln!("{guidance}");
    }
    exit::from_status(status)
}

/// Describes an endpoint's state in the terms its kind makes sense of.
///
/// Attached hardware has no connection lifecycle, so reporting a phase for it says "unknown"
/// where the useful answer is whether the device is plugged in.
fn endpoint_state(endpoint: &pb::Endpoint) -> String {
    if let Some(pb::endpoint::Detail::PhysicalDevice(device)) = &endpoint.detail {
        if !device.present {
            return "absent".to_owned();
        }
        // A device switched off is still plugged in, and calling it attached hid why its routes
        // carry nothing.
        if !endpoint.enabled {
            return "disabled".to_owned();
        }
        // Attached is not the same as usable: a device another application holds is plugged in
        // and still carries nothing, and calling it attached hid that.
        let failing = endpoint.state.as_ref().is_some_and(|state| {
            matches!(
                pb::ConnectionPhase::try_from(state.phase),
                Ok(pb::ConnectionPhase::Retrying | pb::ConnectionPhase::Unavailable)
            )
        });
        return if failing { "unavailable" } else { "attached" }.to_owned();
    }
    // A session waiting to be invited is resting, not disconnected by some fault. The GUI uses
    // the same word, so the two never disagree about one session.
    let idle = endpoint.state.as_ref().is_some_and(|state| {
        pb::ConnectionPhase::try_from(state.phase) == Ok(pb::ConnectionPhase::Disconnected)
    });
    if matches!(
        endpoint.detail,
        Some(pb::endpoint::Detail::NetworkSession(_))
    ) && endpoint.enabled
        && idle
    {
        return "listening".to_owned();
    }
    // This machine's own advertised port, waiting for a device to connect.
    if matches!(
        &endpoint.detail,
        Some(pb::endpoint::Detail::BluetoothDevice(device)) if device.peripheral_role
    ) && endpoint.enabled
        && idle
    {
        return "advertising".to_owned();
    }
    endpoint
        .state
        .as_ref()
        .map(|state| {
            // A link that keeps dropping reads as connected or retrying at any one moment, and
            // either alone hides why it keeps going quiet.
            if state.waiting_for_network {
                "waiting for network".to_owned()
            } else if state.unstable {
                format!("{} (unstable)", phase_name(state.phase))
            } else {
                phase_name(state.phase).to_owned()
            }
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Returns the machines a session carries beside its peer.
fn session_guests(endpoint: &pb::Endpoint) -> Vec<String> {
    match &endpoint.detail {
        Some(pb::endpoint::Detail::NetworkSession(session)) => session.guests.clone(),
        _ => Vec::new(),
    }
}

/// Describes an endpoint a command made or changed, for its result under `--json`.
fn endpoint_result(endpoint: &pb::Endpoint) -> serde_json::Value {
    let port = match &endpoint.detail {
        Some(pb::endpoint::Detail::NetworkSession(session)) if session.control_port != 0 => {
            Some(session.control_port)
        }
        _ => None,
    };
    serde_json::json!({
        "id": endpoint.id,
        "name": endpoint.name,
        "kind": endpoint_kind(endpoint),
        "port": port,
    })
}

/// Returns when a retrying endpoint tries again, if it has an attempt scheduled.
fn retry_at(endpoint: &pb::Endpoint) -> Option<jiff::Timestamp> {
    let at = endpoint.state.as_ref()?.next_retry.as_ref()?;
    jiff::Timestamp::new(at.seconds, at.nanos).ok()
}

/// Says how long until a retrying endpoint tries again, measured from `now`.
///
/// Shown beside the phase, because when it tries again is what tells a user whether to wait or to
/// go and fix something (US5/AC3).
fn retry_in(endpoint: &pb::Endpoint, now: jiff::Timestamp) -> Option<String> {
    let at = retry_at(endpoint)?;
    let seconds = at.as_second().saturating_sub(now.as_second());
    Some(if seconds <= 0 {
        "trying again now".to_owned()
    } else if seconds < 60 {
        format!("next in {seconds}s")
    } else {
        // Rounded down, as a span shown in a coarser unit is.
        #[allow(clippy::integer_division)]
        let minutes = seconds / 60;
        format!("next in {minutes}m")
    })
}

/// Returns a readable name for a wire connection phase.
fn phase_name(value: i32) -> &'static str {
    match pb::ConnectionPhase::try_from(value) {
        Ok(pb::ConnectionPhase::Disabled) => "disabled",
        Ok(pb::ConnectionPhase::Disconnected) => "disconnected",
        Ok(pb::ConnectionPhase::Connecting) => "connecting",
        Ok(pb::ConnectionPhase::Connected) => "connected",
        Ok(pb::ConnectionPhase::Retrying) => "retrying",
        Ok(pb::ConnectionPhase::Unavailable) => "unavailable",
        _ => "unknown",
    }
}

/// Returns a readable name for an endpoint's kind.
///
/// A port macOS or another application provides, such as an IAC bus or one of Apple's network
/// sessions, arrives as a device but is not hardware, and the GUI lists it apart for that reason.
fn endpoint_kind(endpoint: &pb::Endpoint) -> &'static str {
    match &endpoint.detail {
        Some(pb::endpoint::Detail::PhysicalDevice(device)) if device.software => "provided",
        _ => kind_name(endpoint.kind),
    }
}

/// Returns a readable name for a wire endpoint kind.
fn kind_name(value: i32) -> &'static str {
    match pb::EndpointKind::try_from(value) {
        Ok(pb::EndpointKind::VirtualPort) => "virtual",
        Ok(pb::EndpointKind::PhysicalDevice) => "physical",
        Ok(pb::EndpointKind::NetworkSession) => "network",
        Ok(pb::EndpointKind::BluetoothDevice) => "bluetooth",
        _ => "unknown",
    }
}

/// Copies a port command, since dispatch needs to move it into an async body.
fn clone_port_command(command: &PortCommand) -> PortCommand {
    match command {
        PortCommand::List => PortCommand::List,
        PortCommand::Create {
            name,
            inputs,
            outputs,
            direction,
        } => PortCommand::Create {
            name: name.clone(),
            inputs: *inputs,
            outputs: *outputs,
            direction: *direction,
        },
        PortCommand::Connectors {
            target,
            inputs,
            outputs,
        } => PortCommand::Connectors {
            target: target.clone(),
            inputs: *inputs,
            outputs: *outputs,
        },
        PortCommand::Rename {
            target,
            new_name,
            yes,
        } => PortCommand::Rename {
            target: target.clone(),
            new_name: new_name.clone(),
            yes: *yes,
        },
        PortCommand::Delete { target, yes } => PortCommand::Delete {
            target: target.clone(),
            yes: *yes,
        },
        PortCommand::Enable { target } => PortCommand::Enable {
            target: target.clone(),
        },
        PortCommand::Disable { target } => PortCommand::Disable {
            target: target.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks how an event's time is written under `--json` and in the table.
    ///
    /// Scripts parse the JSON `at` field, so it is RFC 3339 in UTC and keeps the fraction of a
    /// second the daemon recorded. The table is for a person and reads to the second in their
    /// zone. An event with no time reads "-" rather than the Unix epoch. 1_790_092_867 seconds
    /// after the epoch is 2026-09-22 16:01:07 UTC.
    #[test]
    fn an_event_says_when_it_happened() {
        let mut dated = pb::Event {
            id: 1,
            at: Some(Default::default()),
            ..pb::Event::default()
        };
        if let Some(at) = dated.at.as_mut() {
            at.seconds = 1_790_092_867;
            at.nanos = 500_000_000;
        }
        let cases = [
            (
                "dated",
                dated,
                Some("2026-09-22T16:01:07.5Z"),
                "2026-09-22 16:01:07",
            ),
            ("undated", pb::Event::default(), None, "-"),
        ];
        for (name, event, json, table) in cases {
            assert_eq!(
                event_instant(&event).map(|at| at.to_string()).as_deref(),
                json,
                "the {name} event must write {json:?} under --json"
            );
            assert_eq!(
                event_time(&event, &jiff::tz::TimeZone::UTC),
                table,
                "the {name} event must read {table} in the table"
            );
        }
    }

    /// Locks the `kind` and `state` words `--json` writes for a device, which scripts switch on.
    ///
    /// Measured on a Mac with a USB MIDI interface and another program's network session: a
    /// switched-off interface read "attached", and Apple's session read "physical". A device
    /// another application holds is plugged in and still carries nothing, so it reads
    /// "unavailable" rather than "attached". A port macOS or another application provides is not
    /// hardware, and reads "provided".
    #[test]
    fn a_device_says_when_it_is_switched_off_held_or_not_hardware() {
        let device = |present, software, enabled, phase: pb::ConnectionPhase| pb::Endpoint {
            kind: pb::EndpointKind::PhysicalDevice as i32,
            enabled,
            detail: Some(pb::endpoint::Detail::PhysicalDevice(
                pb::PhysicalDeviceDetail {
                    present,
                    software,
                    ..pb::PhysicalDeviceDetail::default()
                },
            )),
            state: Some(pb::ConnectionState {
                phase: phase as i32,
                ..pb::ConnectionState::default()
            }),
            ..pb::Endpoint::default()
        };
        use pb::ConnectionPhase::{Connected, Unavailable};
        let cases = [
            (
                "plugged-in interface",
                device(true, false, true, Connected),
                "physical",
                "attached",
            ),
            (
                "switched-off interface",
                device(true, false, false, Connected),
                "physical",
                "disabled",
            ),
            (
                "unplugged interface",
                device(false, false, false, Connected),
                "physical",
                "absent",
            ),
            (
                "interface another application holds",
                device(true, false, true, Unavailable),
                "physical",
                "unavailable",
            ),
            (
                "provided port",
                device(true, true, true, Connected),
                "provided",
                "attached",
            ),
        ];
        for (name, endpoint, kind, state) in cases {
            assert_eq!(
                endpoint_kind(&endpoint),
                kind,
                "the {name} must be of kind {kind}"
            );
            assert_eq!(
                endpoint_state(&endpoint),
                state,
                "the {name} must read {state}"
            );
        }
    }

    /// Locks what importing Apple's setup creates, leaves, and refuses.
    ///
    /// A name this machine already has is left alone rather than made unique, because "Bus 1 (2)"
    /// is not what anyone moving over from the IAC Driver wants. A port and a network port may not
    /// share a name, since other applications see both as ports and the daemon refuses the
    /// second, so the plan says so up front rather than failing partway through. A name Apple
    /// lists twice is created once.
    #[test]
    fn importing_apples_setup_creates_only_what_can_be_created() {
        let names =
            |list: &[&str]| -> Vec<String> { list.iter().map(|name| (*name).to_owned()).collect() };
        let setup = |buses: &[&str], sessions: &[&str]| pb::ReadAppleSetupResponse {
            available: true,
            iac_online: true,
            buses: names(buses),
            sessions: names(sessions),
        };
        let plan =
            |ports: &[&str], sessions: &[&str], already_here: &[&str], name_taken: &[&str]| {
                ApplePlan {
                    ports: names(ports),
                    sessions: names(sessions),
                    already_here: names(already_here),
                    name_taken: names(name_taken),
                }
            };
        let cases = [
            (
                "a bus already here",
                setup(&["Bus 1", "Test2"], &["Session 1"]),
                names(&["Bus 1"]),
                names(&[]),
                plan(&["Test2"], &["Session 1"], &["Bus 1"], &[]),
            ),
            (
                "a session named like a port here",
                setup(&[], &["Studio"]),
                names(&["Studio"]),
                names(&[]),
                plan(&[], &[], &[], &["Studio"]),
            ),
            (
                "a session named like a bus being imported",
                setup(&["Bus 1"], &["Bus 1"]),
                names(&[]),
                names(&[]),
                plan(&["Bus 1"], &[], &[], &["Bus 1"]),
            ),
            (
                "a bus named like a network port here",
                setup(&["Studio"], &[]),
                names(&[]),
                names(&["Studio"]),
                plan(&[], &[], &[], &["Studio"]),
            ),
            (
                "a bus listed twice",
                setup(&["Bus 1", "Bus 1"], &[]),
                names(&[]),
                names(&[]),
                plan(&["Bus 1"], &[], &["Bus 1"], &[]),
            ),
        ];
        for (name, setup, ports_here, sessions_here, want) in cases {
            assert_eq!(
                plan_apple_import(&setup, &ports_here, &sessions_here),
                want,
                "the plan for {name} must create only what the daemon will accept"
            );
        }
    }
}
