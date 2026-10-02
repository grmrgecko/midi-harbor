//! The pages, and the panel that shows one endpoint in full.
//!
//! Only arrangement lives here. What a label says is decided in [`crate::format`].

use crate::app::{App, Dialog, Message, Page, TestNoteField, now_seconds};
use crate::format::{self, Section, Tone};
use crate::onboarding;
use crate::parts::{dot, empty, kind_icon, page_header, pill, row, toned};
use cosmic::iced::{Alignment, Length};
use cosmic::prelude::*;
use cosmic::widget;
use midi_harbor_ipc::pb::{
    ConnectionPhase, Endpoint, Invitation, Route, TrafficCounters, endpoint::Detail,
};

/// Builds the whole window for the current page.
pub fn window(app: &App) -> Element<'_, Message> {
    // A daemon that cannot be reached replaces the view entirely: showing a stale list beside the
    // error would suggest those endpoints are still being managed.
    if let Some(error) = &app.unreachable {
        return widget::container(unreachable(app, error))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center)
            .into();
    }

    let page = match app.page {
        Page::Endpoints => endpoints(app),
        Page::Routes => routes(app),
        Page::Bluetooth => bluetooth(app),
        Page::Activity => activity(app),
        Page::Monitor => monitor(app),
        Page::Settings => settings(app),
    };

    let mut column = widget::column::with_capacity(5).spacing(16);
    #[cfg(target_os = "macos")]
    if let Some(store) = &app.store
        && store.offers_login()
    {
        column = column.push(login_offer());
    }
    // Shown above whatever page is open until someone dismisses it, here or from the command
    // line: other applications may have stopped hearing MIDI, and nothing else on screen would
    // say so.
    if let Some(at) = app
        .snapshot
        .status
        .as_ref()
        .and_then(|status| status.midi_server_replaced_at.as_ref())
    {
        column = column.push(midi_server_banner(at));
    }
    // Shown above whatever page is open: the daemon is not running what this window came with,
    // and only the user can say when a restart suits.
    if let Some(notice) = &app.build.notice {
        column = column.push(build_banner(notice));
    }
    // Shown above whatever page is open, because a machine is waiting on an answer and will give
    // up. A page the user has to think to visit would be a prompt nobody sees.
    for invitation in &app.snapshot.invitations {
        column = column.push(invitation_banner(app, invitation));
    }
    if let Some(error) = &app.action_error {
        column = column.push(
            widget::row::with_capacity(2)
                .spacing(8)
                .align_y(Alignment::Center)
                .push(widget::container(toned(Tone::Bad, error.clone())).width(Length::Fill))
                .push(widget::button::text("Dismiss").on_press(Message::DismissError)),
        );
    }
    widget::scrollable(
        widget::container(column.push(page))
            .padding([16, 24])
            .width(Length::Fill),
    )
    .height(Length::Fill)
    .into()
}

/// Builds the one-line summary of what the daemon is carrying.
fn summary(app: &App) -> String {
    let Some(status) = &app.snapshot.status else {
        return "Connecting to Midi Harbor".to_owned();
    };
    // A listening network port is not connected and is not a problem either, so it is named
    // beside the connected count rather than left looking like a shortfall in it.
    let listening = app
        .snapshot
        .endpoints
        .iter()
        .filter(|endpoint| format::is_listening(endpoint))
        .count();
    let mut line = format!(
        "{} of {} connected",
        status.connected_count,
        format::plural(status.endpoint_count, "endpoint", "endpoints")
    );
    if listening > 0 {
        line.push_str(&format!(" · {listening} listening"));
    }
    line.push_str(&format!(
        " · {}",
        format::plural(status.route_count, "route", "routes")
    ));
    // Routes are reported only when some are broken, so a healthy system stays quiet.
    if status.broken_route_count > 0 {
        line.push_str(&format!(
            " · {} waiting for an endpoint",
            status.broken_route_count
        ));
    }
    line
}

/// Builds the endpoints page: one section per kind, each ending with its way to add one.
fn endpoints(app: &App) -> Element<'_, Message> {
    let mut column = widget::column::with_capacity(Section::ALL.len() + 1)
        .spacing(16)
        .push(page_header("Endpoints", summary(app), Vec::new()));
    for section in Section::ALL {
        let add = match section {
            Section::VirtualPorts => Some(("+ Add virtual port", Message::Open(Dialog::NewPort))),
            Section::NetworkPorts => {
                Some(("+ Add network port", Message::Open(Dialog::NewNetworkPort)))
            }
            Section::Bluetooth => {
                Some(("+ Connect a Bluetooth device", Message::Go(Page::Bluetooth)))
            }
            Section::Hardware | Section::Provided => None,
        };
        let members: Vec<&Endpoint> = app
            .snapshot
            .endpoints
            .iter()
            .filter(|endpoint| format::section(endpoint) == section)
            .collect();
        if members.is_empty() && add.is_none() {
            continue;
        }
        let mut list = widget::settings::section().title(section.title());
        for endpoint in members {
            list = list.add(endpoint_row(app, endpoint));
        }
        if let Some((label, message)) = add {
            list = list.add(widget::button::text(label).on_press(message));
        }
        column = column.push(list);
    }
    column.into()
}

/// Builds one endpoint's row: clicking it opens its panel.
fn endpoint_row<'a>(app: &'a App, endpoint: &'a Endpoint) -> Element<'a, Message> {
    let (word, tone) = format::endpoint_status(endpoint);
    let mut trailing: Vec<Element<'a, Message>> = Vec::with_capacity(2);
    if let Some(counters) = &endpoint.counters {
        trailing.push(widget::text::caption(traffic(counters)).into());
    }
    trailing.push(pill(tone, format::capitalised(word)));
    row(
        format::section(endpoint),
        endpoint.name.clone(),
        trailing,
        format::kind_and_detail(endpoint, &app.snapshot.peers),
        Some(Message::Select(endpoint.id.clone())),
    )
}

/// Returns an endpoint's traffic in the short form a row has room for.
fn traffic(counters: &TrafficCounters) -> String {
    format!(
        "↓ {}  ↑ {}",
        format::compact(counters.messages_received),
        format::compact(counters.messages_sent)
    )
}

/// Returns the section an endpoint, named by identifier, is listed under.
fn section_of(app: &App, id: Option<&str>) -> Section {
    id.and_then(|id| app.endpoint(id))
        .map_or(Section::Provided, format::section)
}

/// Builds one route's line: both ends, what it is doing, its Edit, and its switch.
fn route_line<'a>(app: &'a App, route: &'a Route) -> Element<'a, Message> {
    let id = route.id.clone();
    let status = format::route_problem(route)
        .or_else(|| format::route_traffic(route))
        .unwrap_or_default();
    let tone = format::route_tone(route);
    widget::row::with_capacity(7)
        .spacing(10)
        .align_y(Alignment::Center)
        .push(kind_icon(section_of(app, route.from_id.as_deref()), 28.0))
        .push(widget::text::body(format::route_end(
            &route.from_name,
            "MIDI In",
            route.from_connector,
        )))
        .push(widget::text::body(if route.both_ways {
            "↔"
        } else {
            "→"
        }))
        .push(kind_icon(section_of(app, route.to_id.as_deref()), 28.0))
        .push(
            widget::column::with_capacity(2)
                .push(widget::text::body(format::route_end(
                    &route.to_name,
                    "MIDI Out",
                    route.to_connector,
                )))
                .push(toned(tone, status))
                .width(Length::Fill),
        )
        .push(widget::button::text("Edit").on_press(Message::Open(Dialog::EditRoute(id.clone()))))
        .push(
            widget::toggler(route.enabled)
                .on_toggle(move |on| Message::ToggleRoute(id.clone(), on)),
        )
        .into()
}

/// Builds the routes page.
fn routes(app: &App) -> Element<'_, Message> {
    let mut list = widget::settings::section().title("All routes");
    for route in &app.snapshot.routes {
        list = list.add(route_line(app, route));
    }
    if app.snapshot.routes.is_empty() {
        list = list.add(empty(
            "No routes yet. A route sends the MIDI from one endpoint to another as it arrives.",
        ));
    }
    widget::column::with_capacity(2)
        .spacing(16)
        .push(page_header(
            "Routes",
            "MIDI from each source goes to its destination as it arrives",
            vec![
                widget::button::suggested("New route")
                    .on_press(Message::Open(Dialog::NewRoute))
                    .into(),
            ],
        ))
        .push(list)
        .into()
}

/// Builds the Bluetooth page: devices this computer connects to, devices in range, and this
/// computer offered as a device.
///
/// Each part says why it is unavailable instead of offering controls that would fail, because a
/// computer without a radio, or with it switched off, is an ordinary machine (FR-053).
fn bluetooth(app: &App) -> Element<'_, Message> {
    let capabilities = &app.snapshot.capabilities;
    let central_unavailable = format::unavailable_because(capabilities, "bluetooth_central");

    let mut yours = widget::settings::section().title("Your devices · reconnect by themselves");
    let mut any = false;
    for endpoint in app
        .snapshot
        .endpoints
        .iter()
        .filter(|e| format::section(e) == Section::Bluetooth)
    {
        any = true;
        yours = yours.add(endpoint_row(app, endpoint));
    }
    if !any {
        yours = yours.add(empty("No devices yet. Scan, then connect one below."));
    }

    let mut nearby = widget::settings::section().title("In range");
    if let Some(reason) = &central_unavailable {
        nearby = nearby.add(toned(Tone::Waiting, reason.clone()));
    } else {
        let mut heard = false;
        for device in app
            .snapshot
            .nearby
            .iter()
            .filter(|device| device.endpoint_id.is_none())
        {
            heard = true;
            let (name, detail) = format::nearby_label(device);
            nearby = nearby.add(row(
                Section::Bluetooth,
                name,
                vec![
                    widget::button::suggested("Connect")
                        .on_press(Message::ConnectBluetooth(device.address.clone()))
                        .into(),
                ],
                detail,
                None,
            ));
        }
        if !heard {
            nearby = nearby.add(empty(format!(
                "No new Bluetooth MIDI devices heard. Scan to listen for {} seconds.",
                crate::app::BLUETOOTH_SCAN_SECONDS
            )));
        }
    }

    let mut offered = widget::settings::section().title("This computer as a Bluetooth MIDI device");
    if let Some(reason) = format::unavailable_because(capabilities, "bluetooth_peripheral") {
        offered = offered.add(toned(Tone::Waiting, reason));
    } else {
        let advertising = app.snapshot.endpoints.iter().any(|endpoint| {
            matches!(
                &endpoint.detail,
                Some(Detail::BluetoothDevice(device)) if device.peripheral_role
            ) && endpoint
                .state
                .as_ref()
                .is_some_and(|state| state.phase() != ConnectionPhase::Disabled)
        });
        offered = offered.add(widget::settings::item(
            "Let phones, tablets and other computers connect",
            widget::toggler(advertising).on_toggle(Message::SetAdvertising),
        ));
    }

    let mut scan = widget::button::suggested("Scan");
    if central_unavailable.is_none() {
        scan = scan.on_press(Message::ScanBluetooth);
    }
    widget::column::with_capacity(4)
        .spacing(16)
        .push(page_header(
            "Bluetooth",
            "Bluetooth MIDI devices, and this computer offered as one",
            vec![scan.into()],
        ))
        .push(yours)
        .push(nearby)
        .push(offered)
        .into()
}

/// Builds the history, newest first, optionally for one endpoint and at most `limit` entries.
fn history<'a>(
    app: &'a App,
    only: Option<&str>,
    limit: usize,
) -> widget::settings::Section<'a, Message> {
    let problems =
        only.is_none() && app.activity_filter.position(app.activity_filter.active()) == Some(1);
    let now = now_seconds();
    let mut list =
        widget::settings::section().title(if only.is_some() { "Recent" } else { "History" });
    let mut shown = 0;
    // The daemon returns oldest first; a log is read newest first.
    for event in app.snapshot.events.iter().rev() {
        if only.is_some_and(|id| event.endpoint_id.as_deref() != Some(id)) {
            continue;
        }
        let tone = format::severity_tone(event.severity());
        if problems && tone == Tone::Good {
            continue;
        }
        if shown == limit {
            break;
        }
        shown += 1;
        let about = match (only, event.endpoint_id.as_deref()) {
            (None, Some(id)) => app.endpoint(id).map(|e| e.name.clone()).unwrap_or_default(),
            _ => String::new(),
        };
        list = list.add(widget::settings::item_row(vec![
            dot(tone),
            widget::column::with_capacity(2)
                .push(widget::text::body(event.detail.clone()))
                .push(widget::text::caption(
                    format::age(event.at.as_ref(), now).unwrap_or_default(),
                ))
                .width(Length::Fill)
                .into(),
            widget::text::caption(about).into(),
        ]));
    }
    if shown == 0 {
        list = list.add(empty(
            "Nothing yet. Connections, failures and recoveries are recorded here as they happen.",
        ));
    }
    list
}

/// Builds the activity page.
fn activity(app: &App) -> Element<'_, Message> {
    widget::column::with_capacity(3)
        .spacing(16)
        .push(page_header(
            "Activity",
            "What has happened to every connection, newest first",
            Vec::new(),
        ))
        .push(
            widget::segmented_control::horizontal(&app.activity_filter)
                .on_activate(Message::ActivityFilter)
                .width(Length::Fixed(320.0)),
        )
        .push(history(app, None, usize::MAX))
        .into()
}

/// Builds the monitor page: which endpoint to watch, and what it has carried, newest first.
fn monitor(app: &App) -> Element<'_, Message> {
    let watching = app.monitor.choices.position(app.monitor.watching.as_ref());
    let choose = widget::dropdown(
        app.monitor.choices.labels.as_slice(),
        watching,
        Message::PickMonitored,
    )
    .width(Length::Fixed(320.0));

    let mut list = widget::settings::section().title(if app.monitor.dropped > 0 {
        format!(
            "Messages · {} not shown because the window fell behind",
            app.monitor.dropped
        )
    } else {
        "Messages".to_owned()
    });
    if let Some(reason) = &app.monitor.ended {
        list = list.add(toned(Tone::Bad, format!("Stopped watching: {reason}")));
    }
    if watching.is_none() {
        list = list.add(empty(
            "Choose an endpoint to see the MIDI passing through it as it happens.",
        ));
    } else if app.monitor.seen.is_empty() {
        list = list.add(empty("Messages appear here as they arrive or leave."));
    }
    let now = now_seconds();
    for seen in &app.monitor.seen {
        // Which way it went decides whether it was something played into the endpoint or
        // something sent out of it, which is the first question when a note goes missing.
        let (tone, way) = if seen.outbound {
            (Tone::Waiting, "out ↑")
        } else {
            (Tone::Good, "in ↓")
        };
        list = list.add(widget::settings::item_row(vec![
            toned(tone, way),
            widget::text::monotext(seen.decoded.clone())
                .width(Length::Fill)
                .into(),
            widget::text::caption(format::age(seen.at.as_ref(), now).unwrap_or_default()).into(),
        ]));
    }

    // The picker sits at the left of the page, not in the header's right corner: its list opens
    // rightwards as wide as the longest name and is not held to the window, so from the corner it
    // ran off the edge and cut the names short.
    // The dropdown shows nothing until something is chosen, so the label asks for it.
    let label = if watching.is_some() {
        "Endpoint"
    } else {
        "Choose an endpoint to watch"
    };
    let watch = widget::column::with_capacity(2)
        .spacing(4)
        .push(widget::text::caption(label))
        .push(choose);
    widget::column::with_capacity(4)
        .spacing(16)
        .push(page_header(
            "Monitor",
            "The MIDI passing through one endpoint, as it happens",
            Vec::new(),
        ))
        .push(watch)
        .push(test_note(app))
        .push(list)
        .into()
}

/// Builds the row that sends a note out of the endpoint being watched, to test what listens
/// there. What is sent shows in the list below as it leaves.
fn test_note(app: &App) -> Element<'_, Message> {
    let sinks = app
        .monitor
        .watching
        .as_deref()
        .and_then(|id| app.endpoint(id))
        .is_some_and(|endpoint| format::can_sink(endpoint.direction()));
    let note = widget::column::with_capacity(2)
        .spacing(4)
        .push(widget::text::caption("Note"))
        .push(
            widget::dropdown(
                app.test_note.notes.as_slice(),
                Some(usize::from(app.test_note.note)),
                Message::PickTestNote,
            )
            .width(Length::Fixed(190.0)),
        );
    let field = |label: &'static str, value: &str, which: TestNoteField| {
        widget::column::with_capacity(2)
            .spacing(4)
            .push(widget::text::caption(label))
            .push(
                widget::text_input("", value.to_owned())
                    .on_input(move |typed| Message::EditTestNote(which, typed))
                    .width(Length::Fixed(72.0)),
            )
    };
    let mut send = widget::button::standard("Send note");
    if sinks {
        send = send.on_press(Message::SendTestNote);
    }
    let caption = if app.monitor.watching.is_none() {
        "Choose an endpoint to send it a note."
    } else if sinks {
        "Sends the note for half a second, then its note-off."
    } else {
        "This endpoint only sends MIDI, so nothing can be sent out of it."
    };
    widget::settings::section()
        .title("Send a test note")
        .add(
            widget::row::with_capacity(5)
                .spacing(12)
                .align_y(Alignment::End)
                .push(note)
                .push(field(
                    "Channel",
                    &app.test_note.channel,
                    TestNoteField::Channel,
                ))
                .push(field(
                    "Velocity",
                    &app.test_note.velocity,
                    TestNoteField::Velocity,
                ))
                .push(send)
                .push(
                    widget::text::caption(caption)
                        .width(Length::Fill)
                        .align_y(Alignment::Center),
                ),
        )
        .into()
}

/// Builds the settings page.
fn settings(app: &App) -> Element<'_, Message> {
    // The machines this one knows. Whether each is let in without asking belongs to the
    // machine rather than to one network port, which is why it lives here.
    let mut known = widget::settings::section().title("Machines you know");
    let mut any = false;
    for peer in app
        .snapshot
        .peers
        .iter()
        .filter(|peer| format::is_remembered(peer))
    {
        any = true;
        let detail = if peer.trusted {
            format!("{} · let in without asking", peer.addresses.join(", "))
        } else {
            format!("{} · asked about each time", peer.addresses.join(", "))
        };
        let id = peer.id.clone();
        known = known.add(row(
            Section::NetworkPorts,
            peer.advertised_name.clone(),
            vec![
                widget::toggler(peer.trusted)
                    .on_toggle(move |on| Message::TrustMachine(id.clone(), on))
                    .into(),
                widget::button::text("Forget")
                    .on_press(Message::ForgetMachine(peer.id.clone()))
                    .into(),
            ],
            detail,
            None,
        ));
    }
    if !any {
        known = known.add(empty(
            "None yet. Machines you let in, or add by address, are remembered here.",
        ));
    }
    known = known.add(
        widget::button::text("+ Add a machine by address")
            .on_press(Message::Open(Dialog::AddMachine)),
    );

    let mut capabilities = widget::settings::section().title("What this computer can do");
    for capability in &app.snapshot.capabilities {
        capabilities = capabilities.add(widget::settings::item(
            format::capitalised(&capability.name),
            if capability.available {
                toned(Tone::Good, "Available")
            } else {
                toned(Tone::Waiting, format::capitalised(&capability.reason))
            },
        ));
    }

    let mut diagnostics =
        widget::settings::section()
            .title("Diagnostics")
            .add(widget::settings::item(
                "A report of the running setup and recent history, for a bug report",
                widget::button::standard("Export report").on_press(Message::ExportDiagnostics),
            ));
    if let Some(path) = &app.saved_report {
        diagnostics = diagnostics.add(toned(Tone::Good, format!("Saved to {path}")));
    }

    let page = widget::column::with_capacity(5)
        .spacing(16)
        .push(page_header("Settings", "Midi Harbor", Vec::new()));
    #[cfg(target_os = "macos")]
    let page = match &app.store {
        Some(store) => page.push(startup(store)),
        None => page,
    };
    page.push(known).push(capabilities).push(diagnostics).into()
}

/// Builds the one-time offer to start at login, in App Store mode.
#[cfg(target_os = "macos")]
fn login_offer<'a>() -> Element<'a, Message> {
    widget::container(
        widget::row::with_capacity(3)
            .spacing(10)
            .align_y(Alignment::Center)
            .push(
                widget::column::with_capacity(2)
                    .push(widget::text::heading("Start Midi Harbor when you log in?"))
                    .push(widget::text::caption(
                        "It starts in the menu bar, with your ports and connections running.",
                    ))
                    .width(Length::Fill),
            )
            .push(
                widget::button::suggested("Start at login")
                    .on_press(Message::AnswerLoginOffer(true)),
            )
            .push(widget::button::text("Not now").on_press(Message::AnswerLoginOffer(false))),
    )
    .padding(12)
    .class(cosmic::theme::Container::Card)
    .into()
}

/// Builds the Start at login setting, in App Store mode.
#[cfg(target_os = "macos")]
fn startup(store: &crate::app_store::StoreState) -> Element<'_, Message> {
    use midi_harbor_platform::appkit::login_item::LoginItem;

    let detail = match store.login {
        LoginItem::On => {
            "Midi Harbor starts in the menu bar when you log in, and opens this window if it \
             was open when it quit"
        }
        LoginItem::Waiting => {
            "Waiting for you to allow it in System Settings, under General, then Login Items"
        }
        LoginItem::Off => "Midi Harbor starts only when you open it",
    };
    let mut section = widget::settings::section()
        .title("Starting up")
        .add(widget::settings::item(
            "Start at login",
            widget::toggler(store.login != LoginItem::Off).on_toggle(Message::SetStartAtLogin),
        ));
    section = section.add(widget::text::caption(detail));
    if let Some(error) = &store.login_error {
        section = section.add(toned(Tone::Bad, error.clone()));
    }
    section.into()
}

/// Builds the warning that the MIDI service stopped and was restarted.
fn midi_server_banner<'a>(at: &prost_types::Timestamp) -> Element<'a, Message> {
    let when = format::clock(at, &jiff::tz::TimeZone::system(), jiff::Timestamp::now());
    widget::container(
        widget::row::with_capacity(3)
            .spacing(10)
            .align_y(Alignment::Center)
            .push(dot(Tone::Bad))
            .push(
                widget::column::with_capacity(2)
                    .push(widget::text::heading(format!(
                        "The MIDI service stopped at {when} and was restarted"
                    )))
                    .push(widget::text::caption(
                        "Midi Harbor recovered, but other apps may have lost their MIDI \
                         connection. Relaunch any app that stops sending or receiving MIDI.",
                    ))
                    .width(Length::Fill),
            )
            .push(widget::button::text("Dismiss").on_press(Message::DismissMidiServerWarning)),
    )
    .padding(12)
    .class(cosmic::theme::Container::Card)
    .into()
}

/// Builds the notice that the daemon is another build of Midi Harbor than this window, with the
/// button that updates it where the window can.
fn build_banner<'a>(notice: &crate::update::Notice) -> Element<'a, Message> {
    let mut row = widget::row::with_capacity(4)
        .spacing(10)
        .align_y(Alignment::Center)
        .push(dot(Tone::Waiting))
        .push(
            widget::column::with_capacity(2)
                .push(widget::text::heading(notice.heading()))
                .push(widget::text::caption(
                    notice.explanation(midi_harbor_core::VERSION),
                ))
                .width(Length::Fill),
        );
    if let Some(label) = notice.action() {
        row = row.push(widget::button::suggested(label).on_press(Message::ReplaceDaemon));
    }
    widget::container(
        row.push(widget::button::text("Not now").on_press(Message::DismissBuildNotice)),
    )
    .padding(12)
    .class(cosmic::theme::Container::Card)
    .into()
}

/// Builds the notice for a machine asking to join a network port.
fn invitation_banner<'a>(app: &'a App, invitation: &'a Invitation) -> Element<'a, Message> {
    let id = invitation.invitation_id.clone();
    let port = app
        .endpoint(&invitation.session_endpoint_id)
        .map_or_else(|| "a network port".to_owned(), |e| e.name.clone());
    let answer = |accept, always| Message::AnswerInvitation {
        id: id.clone(),
        accept,
        always,
    };
    widget::container(
        widget::row::with_capacity(5)
            .spacing(10)
            .align_y(Alignment::Center)
            .push(kind_icon(Section::NetworkPorts, 34.0))
            .push(
                widget::column::with_capacity(2)
                    .push(widget::text::heading(format!(
                        "{} wants to join {port}",
                        invitation.peer_name
                    )))
                    .push(widget::text::caption(invitation.peer_address.clone()))
                    .width(Length::Fill),
            )
            .push(widget::button::suggested("Always allow").on_press(answer(true, true)))
            .push(widget::button::standard("Allow once").on_press(answer(true, false)))
            .push(widget::button::text("Refuse").on_press(answer(false, false))),
    )
    .padding(12)
    .class(cosmic::theme::Container::Card)
    .into()
}

/// Builds what replaces the window while the daemon cannot be reached: why, and what can be done
/// about it without a terminal.
fn unreachable<'a>(app: &'a App, error: &'a str) -> Element<'a, Message> {
    let mut column = widget::column::with_capacity(5)
        .spacing(12)
        .align_x(Alignment::Center)
        .max_width(560);

    // In App Store mode the app starts its own daemon, so there is no service to offer: it is
    // starting, or it could not.
    #[cfg(target_os = "macos")]
    if let Some(store) = &app.store {
        return match &store.failed {
            Some(failed) => column
                .push(widget::text::title3("Midi Harbor could not start"))
                .push(toned(Tone::Bad, failed.clone()))
                .into(),
            None => column
                .push(widget::text::title3("Starting Midi Harbor…"))
                .push(widget::text::body(
                    "Your ports and connections come back as soon as it is running.",
                ))
                .into(),
        };
    }
    if app.build.replacing {
        return column
            .push(widget::text::title3("Updating Midi Harbor…"))
            .push(widget::text::body(error))
            .into();
    }
    let Some(checked) = &app.service.checked else {
        column = column
            .push(widget::text::title3("Midi Harbor cannot be reached"))
            .push(widget::text::body(error));
        if let Some(failed) = &app.service.failed {
            column = column.push(toned(Tone::Bad, failed.clone()));
        }
        return column.into();
    };

    column = column
        .push(widget::text::title3("Midi Harbor is not running"))
        .push(widget::text::body(onboarding::explanation(
            &checked.offer,
            checked.manager,
        )));
    if let Some(label) = onboarding::action_label(&checked.offer) {
        let mut button = widget::button::suggested(if app.service.working {
            "Setting up…"
        } else {
            label
        });
        if !app.service.working {
            button = button.on_press(Message::SetUpService);
        }
        column = column.push(button);
    }
    if let Some(failed) = &app.service.failed {
        column = column.push(toned(Tone::Bad, failed.clone()));
    }
    column.into()
}

/// Returns what an endpoint's Disconnect asks for.
///
/// Each kind closes through its own call: a Bluetooth link sent to the network port's disconnect
/// was not found, and the button failed every time.
fn disconnect_message(endpoint: &Endpoint) -> Message {
    if matches!(endpoint.detail, Some(Detail::BluetoothDevice(_))) {
        Message::DisconnectBluetooth(endpoint.id.clone())
    } else {
        Message::DisconnectPeer(endpoint.id.clone())
    }
}

/// Builds everything about one endpoint: what it is, what can be done to it, its routes,
/// traffic and history.
pub fn panel<'a>(app: &'a App, endpoint: &'a Endpoint) -> Element<'a, Message> {
    let id = endpoint.id.clone();
    let section = format::section(endpoint);
    let (word, tone) = format::endpoint_status(endpoint);
    let mut column = widget::column::with_capacity(9).spacing(16);

    // What it is, its state, and its switch.
    let toggle_id = id.clone();
    column = column.push(
        widget::row::with_capacity(4)
            .spacing(12)
            .align_y(Alignment::Center)
            .push(kind_icon(section, 44.0))
            .push(
                widget::column::with_capacity(2)
                    .push(widget::text::heading(format::kind_name(endpoint)))
                    .push(widget::text::caption(format::detail_line(
                        endpoint,
                        &app.snapshot.peers,
                    )))
                    .width(Length::Fill),
            )
            .push(pill(tone, format::capitalised(word)))
            .push(
                widget::toggler(endpoint.enabled)
                    .on_toggle(move |on| Message::ToggleEndpoint(toggle_id.clone(), on)),
            ),
    );
    if let Some(state) = &endpoint.state {
        if let Some(detail) = format::state_detail(state, now_seconds()) {
            column = column.push(toned(tone, format::capitalised(&detail)));
        }
        if let Some(guidance) = state.last_error.as_ref().and_then(format::guidance) {
            column = column.push(toned(Tone::Bad, format::capitalised(guidance)));
        }
    }

    column = column.push(actions(endpoint));
    column = column.push(details(endpoint));
    if let Some(Detail::NetworkSession(session)) = &endpoint.detail {
        column = column.push(machines(app, endpoint, session));
    }

    // Its routes.
    let mut routes = widget::settings::section().title("Routes");
    for route in app
        .snapshot
        .routes
        .iter()
        .filter(|r| r.from_id.as_deref() == Some(&id) || r.to_id.as_deref() == Some(&id))
    {
        routes = routes.add(route_line(app, route));
    }
    routes = routes
        .add(widget::button::text("+ Add a route").on_press(Message::OpenRouteFrom(id.clone())));
    column = column.push(routes);

    if let Some(counters) = &endpoint.counters {
        let zone = jiff::tz::TimeZone::system();
        let now = jiff::Timestamp::now();
        let mut traffic = widget::column::with_capacity(4)
            .spacing(8)
            .push(widget::text::heading("Traffic"))
            .push(counter_cells(counters));
        if let Some(last) = format::last_traffic(counters, &zone, now) {
            traffic = traffic.push(widget::text::caption(last));
        }
        // What the network carried is above; whether it reached the apps here is this.
        if let Some(Detail::NetworkSession(detail)) = &endpoint.detail
            && let Some(automatic) = &detail.automatic_port_counters
        {
            traffic = traffic.push(widget::text::caption(format::automatic_port_traffic(
                automatic, &zone, now,
            )));
        }
        column = column.push(traffic);
    }
    column.push(history(app, Some(&endpoint.id), 5)).into()
}

/// Builds the buttons for what can be done to an endpoint, which depend on what it is.
fn actions(endpoint: &Endpoint) -> Element<'_, Message> {
    let id = endpoint.id.clone();
    let mut row = widget::row::with_capacity(4).spacing(8);
    match format::section(endpoint) {
        Section::VirtualPorts => {
            row = row
                .push(
                    widget::button::standard("Edit")
                        .on_press(Message::Open(Dialog::EditPort(id.clone()))),
                )
                .push(
                    widget::button::destructive("Delete")
                        .on_press(Message::Open(Dialog::DeletePort(id.clone()))),
                );
        }
        Section::NetworkPorts => {
            row = row
                .push(
                    widget::button::standard("Edit")
                        .on_press(Message::Open(Dialog::EditNetworkPort(id.clone()))),
                )
                .push(
                    widget::button::destructive("Delete")
                        .on_press(Message::Open(Dialog::DeleteNetworkPort(id.clone()))),
                );
        }
        Section::Bluetooth => {
            let peripheral = matches!(
                &endpoint.detail,
                Some(Detail::BluetoothDevice(device)) if device.peripheral_role
            );
            if !peripheral {
                row = row
                    .push(
                        widget::button::standard("Disconnect")
                            .on_press(disconnect_message(endpoint)),
                    )
                    .push(
                        widget::button::destructive("Forget")
                            .on_press(Message::ForgetBluetooth(id.clone())),
                    );
            }
        }
        Section::Hardware | Section::Provided => {
            let absent = matches!(
                &endpoint.detail,
                Some(Detail::PhysicalDevice(device)) if !device.present
            );
            if absent {
                row = row.push(
                    widget::button::destructive("Forget")
                        .on_press(Message::ForgetDevice(id.clone())),
                );
            }
        }
    }
    row.push(widget::button::text("Watch MIDI").on_press(Message::Watch(id)))
        .into()
}

/// Builds the facts about an endpoint, which depend on what it is.
fn details(endpoint: &Endpoint) -> Element<'_, Message> {
    let mut facts = widget::settings::section().title("Details");
    let count = |present: bool| if present { "1" } else { "None" };
    match &endpoint.detail {
        Some(Detail::NetworkSession(session)) => {
            let policy = format::POLICIES
                .get(format::policy_position(session.invitation_policy()))
                .map_or("Ask each time", |(label, _)| *label);
            facts = facts
                .add(widget::settings::item(
                    "Name other machines see",
                    widget::text::body(if session.local_name.is_empty() {
                        endpoint.name.clone()
                    } else {
                        session.local_name.clone()
                    }),
                ))
                .add(widget::settings::item(
                    "UDP port",
                    widget::text::body(format!(
                        "{} (data {})",
                        session.control_port,
                        session.control_port.saturating_add(1)
                    )),
                ))
                .add(widget::settings::item(
                    "Who may join",
                    widget::text::body(policy),
                ))
                .add(widget::settings::item(
                    "Automatic virtual port",
                    widget::text::body(if session.automatic_port {
                        format!("On, as {}", endpoint.name)
                    } else {
                        "Off".to_owned()
                    }),
                ));
        }
        Some(Detail::BluetoothDevice(device)) => {
            if let Some(rssi) = device.rssi {
                facts = facts.add(widget::settings::item(
                    "Signal",
                    widget::text::body(format!("{rssi} dBm")),
                ));
            }
            facts = facts.add(widget::settings::item(
                "Reconnects",
                widget::text::body("By itself, whenever it is heard"),
            ));
        }
        Some(Detail::PhysicalDevice(device)) => {
            let fingerprint = device.fingerprint.clone().unwrap_or_default();
            if let Some(maker) = fingerprint.manufacturer {
                facts = facts.add(widget::settings::item("Maker", widget::text::body(maker)));
            }
            if let Some(model) = fingerprint.model {
                facts = facts.add(widget::settings::item("Model", widget::text::body(model)));
            }
            // Counted from the device's side, as Audio MIDI Setup does: its MIDI In takes what
            // is sent to it, its MIDI Out sends.
            facts = facts
                .add(widget::settings::item(
                    "MIDI In",
                    widget::text::body(count(format::can_sink(endpoint.direction()))),
                ))
                .add(widget::settings::item(
                    "MIDI Out",
                    widget::text::body(count(format::can_source(endpoint.direction()))),
                ));
            if let Some(holder) = &device.claimed_by {
                facts = facts.add(widget::settings::item(
                    "In use by",
                    widget::text::body(holder.clone()),
                ));
            }
        }
        _ => {
            let (inputs, outputs) = format::connectors(endpoint);
            facts = facts
                .add(widget::settings::item(
                    "MIDI In connectors",
                    widget::text::body(inputs.to_string()),
                ))
                .add(widget::settings::item(
                    "MIDI Out connectors",
                    widget::text::body(outputs.to_string()),
                ));
        }
    }
    facts.into()
}

/// Builds a network port's machines: those taking part, each with its own Disconnect, then the
/// ones it could connect to, then a way to connect by address.
fn machines<'a>(
    app: &'a App,
    endpoint: &'a Endpoint,
    session: &'a midi_harbor_ipc::pb::NetworkSessionDetail,
) -> Element<'a, Message> {
    let mut list = widget::settings::section().title("Machines");
    let peers = &app.snapshot.peers;
    let known = |address: &str| {
        peers
            .iter()
            .find(|peer| peer.addresses.iter().any(|known| known == address))
    };

    // In it now.
    for machine in &session.machines {
        let name = machine
            .name
            .clone()
            .or_else(|| known(&machine.address).map(|peer| peer.advertised_name.clone()))
            .unwrap_or_else(|| machine.address.clone());
        list = list.add(row(
            Section::NetworkPorts,
            name,
            vec![
                widget::button::standard("Disconnect")
                    .on_press(Message::DisconnectMachine(
                        endpoint.id.clone(),
                        machine.address.clone(),
                    ))
                    .into(),
            ],
            format::machine_detail(machine),
            None,
        ));
    }

    // Could connect. A machine connected while others are carries MIDI beside them.
    let alongside = !session.machines.is_empty();
    let taking_part = |peer: &midi_harbor_ipc::pb::Peer| {
        session.machines.iter().any(|machine| {
            peer.addresses.contains(&machine.address)
                || machine.name.as_deref() == Some(peer.advertised_name.as_str())
        })
    };
    let candidates = peers
        .iter()
        .filter(|peer| (peer.discovered || format::is_remembered(peer)) && !taking_part(peer));
    for peer in candidates {
        let detail = if peer.discovered {
            format!("On this network · {}", peer.addresses.join(", "))
        } else {
            format!("Remembered · {}", peer.addresses.join(", "))
        };
        list = list.add(row(
            Section::NetworkPorts,
            peer.advertised_name.clone(),
            vec![
                widget::button::suggested("Connect")
                    .on_press(Message::ConnectPeer(
                        endpoint.id.clone(),
                        peer.id.clone(),
                        alongside,
                    ))
                    .into(),
            ],
            detail,
            None,
        ));
    }
    list = list.add(
        widget::button::text("+ Connect by address…")
            .on_press(Message::Open(Dialog::ConnectByAddress(endpoint.id.clone()))),
    );
    list.into()
}

/// Builds a row of traffic counters.
fn counter_cells(counters: &TrafficCounters) -> Element<'static, Message> {
    let cells: Vec<Element<'static, Message>> = [
        (counters.messages_received, "received"),
        (counters.messages_sent, "sent"),
        (counters.messages_lost, "lost"),
        (counters.messages_recovered, "recovered"),
        (counters.messages_dropped, "dropped"),
        (counters.packets_malformed, "malformed"),
    ]
    .into_iter()
    .map(|(value, label)| {
        widget::container(
            widget::column::with_capacity(2)
                .push(widget::text::title4(format::compact(value)))
                .push(widget::text::caption(label)),
        )
        .padding(10)
        .width(Length::Fixed(104.0))
        .class(cosmic::theme::Container::Card)
        .into()
    })
    .collect();
    widget::flex_row(cells)
        .row_spacing(8)
        .column_spacing(8)
        .into()
}
