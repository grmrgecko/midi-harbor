//! The modal dialogs. Adding and editing the same thing share one dialog, filled in when editing.

use crate::app::{App, DEFAULT_RTP_PORT, Dialog, Message};
use crate::format;
use crate::parts::{kind_icon, labelled, stepper};
use cosmic::iced::{Alignment, Length};
use cosmic::prelude::*;
use cosmic::widget;

/// Builds the open dialog.
pub fn view<'a>(app: &'a App, dialog: &'a Dialog) -> Element<'a, Message> {
    let draft = &app.draft;
    let cancel = widget::button::text("Cancel").on_press(Message::CloseDialog);
    let name = |placeholder: &'static str| {
        labelled(
            "Name",
            widget::text_input(placeholder, draft.name.as_str())
                .on_input(Message::DraftName)
                .on_submit(|_| Message::Confirm),
        )
    };
    let named = !draft.name.trim().is_empty();

    match dialog {
        Dialog::NewPort | Dialog::EditPort(_) => {
            let editing = matches!(dialog, Dialog::EditPort(_));
            let mut save = widget::button::suggested(if editing { "Save" } else { "Create port" });
            if named {
                save = save.on_press(Message::Confirm);
            }
            let several = draft.inputs > 1 || draft.outputs > 1;
            let control = widget::column::with_capacity(4)
                .spacing(12)
                .push(name("Stage Keys"))
                .push(stepper(
                    "MIDI In connectors",
                    "What other applications send MIDI to",
                    draft.inputs,
                    Message::DraftInputs,
                ))
                .push(stepper(
                    "MIDI Out connectors",
                    "What other applications receive MIDI from",
                    draft.outputs,
                    Message::DraftOutputs,
                ))
                .push(widget::text::caption(if several {
                    "Several of one kind show to other applications as numbered ports, such as Keys 1 and Keys 2. Every connector carries all 16 channels."
                } else {
                    "Every connector carries all 16 MIDI channels."
                }));
            let mut built = widget::dialog()
                .title(if editing { "Edit virtual port" } else { "New virtual port" })
                .body(if editing {
                    "Renaming it or changing its connectors may mean choosing it again in other applications. Routes follow a new name; routes on a connector it no longer has are removed."
                } else {
                    "Other applications on this computer will see it as a MIDI port."
                })
                .control(control)
                .primary_action(save)
                .secondary_action(cancel);
            if let Dialog::EditPort(id) = dialog {
                built = built.tertiary_action(
                    widget::button::destructive("Delete")
                        .on_press(Message::Open(Dialog::DeletePort(id.clone()))),
                );
            }
            built.into()
        }
        Dialog::DeletePort(id) => {
            let name = app.endpoint(id).map_or("this port", |e| e.name.as_str());
            widget::dialog()
                .title(format!("Delete {name}?"))
                .body(
                    "Anything sounding through it is silenced first, and applications using it lose it. Its routes stay, waiting, and carry again if a port of its name is made.",
                )
                .primary_action(widget::button::destructive("Delete").on_press(Message::Confirm))
                .secondary_action(cancel)
                .into()
        }
        Dialog::DeleteNetworkPort(id) => {
            let name = app
                .endpoint(id)
                .map_or("this network port", |e| e.name.as_str());
            widget::dialog()
                .title(format!("Delete {name}?"))
                .body(
                    "Anything it was carrying is silenced first. Every machine connected to it is disconnected, and applications on this computer lose its port. Its routes stay, waiting, and carry again if a network port of its name is made.",
                )
                .primary_action(widget::button::destructive("Delete").on_press(Message::Confirm))
                .secondary_action(cancel)
                .into()
        }
        Dialog::NewNetworkPort | Dialog::EditNetworkPort(_) => {
            let editing = matches!(dialog, Dialog::EditNetworkPort(_));
            let mut save = widget::button::suggested(if editing {
                "Save"
            } else {
                "Create network port"
            });
            // Empty lets the system choose; otherwise an even number, the data port being the
            // one above it.
            let typed_port = draft.udp_port.trim();
            let port_ok = typed_port.is_empty()
                || typed_port
                    .parse::<u16>()
                    .is_ok_and(|port| port.is_multiple_of(2));
            if named && port_ok {
                save = save.on_press(Message::Confirm);
            }
            let udp = labelled(
                "UDP port",
                widget::text_input("Automatic", draft.udp_port.as_str())
                    .on_input(Message::DraftUdpPort)
                    .on_submit(|_| Message::Confirm),
            );
            let bonjour = labelled(
                "Name other machines see",
                widget::text_input(
                    if draft.name.trim().is_empty() {
                        "Same as its name"
                    } else {
                        draft.name.as_str()
                    },
                    draft.local_name.as_str(),
                )
                .on_input(Message::DraftLocalName)
                .on_submit(|_| Message::Confirm),
            );
            let policies = format::policy_labels();
            let automatic = widget::row::with_capacity(2)
                .spacing(8)
                .align_y(Alignment::Center)
                .push(
                    widget::column::with_capacity(2)
                        .push(widget::text::body("Automatic virtual port"))
                        .push(widget::text::caption(
                            "Applications on this computer see it as a MIDI port of the same name. Off, it carries only what is routed to it.",
                        ))
                        .width(Length::Fill),
                )
                .push(
                    widget::toggler(draft.automatic_port).on_toggle(Message::DraftAutomaticPort),
                );
            let mut control = widget::column::with_capacity(5)
                .spacing(12)
                .push(name("Rehearsal Room"))
                .push(bonjour)
                .push(
                    widget::row::with_capacity(2)
                        .spacing(12)
                        .push(widget::container(udp).width(Length::FillPortion(2)))
                        .push(
                            widget::container(labelled(
                                "Who may join",
                                widget::dropdown(
                                    policies,
                                    Some(draft.policy),
                                    Message::DraftPolicy,
                                )
                                .width(Length::Fill),
                            ))
                            .width(Length::FillPortion(3)),
                        ),
                )
                .push(automatic);
            if !port_ok {
                control = control.push(widget::text::caption(
                    "The UDP port has to be an even number up to 65534, the data port being the one above it. Leave it empty to let the system choose.",
                ));
            }
            let mut built = widget::dialog()
                .title(if editing { "Edit network port" } else { "New network port" })
                .body(if editing {
                    "Changes apply straight away, and connected machines stay connected. A new UDP port is the exception: the network port restarts on it, reconnecting to the machines it connected to, while machines that connected to it need the new port."
                } else {
                    "MIDI over the network, which Apple calls a network session. Other machines can find it and join."
                })
                .control(control)
                .primary_action(save)
                .secondary_action(cancel);
            if let Dialog::EditNetworkPort(id) = dialog {
                built = built.tertiary_action(
                    widget::button::destructive("Delete")
                        .on_press(Message::Open(Dialog::DeleteNetworkPort(id.clone()))),
                );
            }
            built.into()
        }
        Dialog::NewRoute | Dialog::EditRoute(_) => {
            let editing = matches!(dialog, Dialog::EditRoute(_));
            let from = draft.sources.position_of(draft.from.as_ref());
            let to = draft.destinations.position_of(draft.to.as_ref());
            let mut connect = widget::button::suggested(if editing { "Save" } else { "Connect" });
            let same_endpoint =
                matches!((&draft.from, &draft.to), (Some(a), Some(b)) if a.0 == b.0);
            if from.is_some() && to.is_some() && !same_endpoint {
                connect = connect.on_press(Message::Confirm);
            }
            let possible = app.two_way_possible();
            let both_ways = draft.both_ways && possible;
            let mut toggle = widget::toggler(both_ways);
            if possible {
                toggle = toggle.on_toggle(Message::DraftBothWays);
            }
            let two_way = widget::row::with_capacity(2)
                .spacing(8)
                .align_y(Alignment::Center)
                .push(
                    widget::column::with_capacity(2)
                        .push(widget::text::body("Both ways"))
                        .push(widget::text::caption(if possible || draft.to.is_none() {
                            "MIDI from the destination comes back to the source on the same route."
                        } else {
                            "The destination sends nothing back, or the source cannot receive."
                        }))
                        .width(Length::Fill),
                )
                .push(toggle);
            let mut built = widget::dialog()
                .title(if editing { "Edit route" } else { "New route" })
                .body(if editing {
                    "Anything sounding through it is silenced before it changes. Switch it on and off from its row."
                } else {
                    "MIDI from the source goes to the destination as it arrives."
                })
                .control(
                    widget::column::with_capacity(4)
                        .spacing(12)
                        .push(preview(app, draft.from.as_ref(), draft.to.as_ref(), both_ways))
                        .push(labelled(
                            "From",
                            widget::dropdown(draft.sources.labels.as_slice(), from, Message::DraftFrom)
                                .width(Length::Fill),
                        ))
                        .push(labelled(
                            "To",
                            widget::dropdown(
                                draft.destinations.labels.as_slice(),
                                to,
                                Message::DraftTo,
                            )
                            .width(Length::Fill),
                        ))
                        .push(two_way),
                )
                .primary_action(connect)
                .secondary_action(cancel);
            if let Dialog::EditRoute(id) = dialog {
                built = built.tertiary_action(
                    widget::button::destructive("Delete route")
                        .on_press(Message::DeleteRoute(id.clone())),
                );
            }
            built.into()
        }
        Dialog::ConnectByAddress(_) | Dialog::AddMachine => {
            let connecting = match dialog {
                Dialog::ConnectByAddress(id) => app.endpoint(id).map(|e| e.name.as_str()),
                _ => None,
            };
            let port_hint = DEFAULT_RTP_PORT.to_string();
            let mut go = widget::button::suggested(if connecting.is_some() {
                "Connect"
            } else {
                "Add"
            });
            if !draft.address.trim().is_empty() {
                go = go.on_press(Message::Confirm);
            }
            widget::dialog()
                .title(match connecting {
                    Some(port) => format!("Connect {port} to a machine"),
                    None => "Add a machine".to_owned(),
                })
                .body("For a machine that does not advertise itself on this network. Without a port, 5004 is used. The name defaults to the address.")
                .control(
                    widget::column::with_capacity(3)
                        .spacing(12)
                        .push(name("Studio PC"))
                        .push(
                            widget::row::with_capacity(2)
                                .spacing(12)
                                .push(
                                    widget::container(labelled(
                                        "Address",
                                        widget::text_input("192.168.1.20", draft.address.as_str())
                                            .on_input(Message::DraftAddress)
                                            .on_submit(|_| Message::Confirm),
                                    ))
                                    .width(Length::FillPortion(3)),
                                )
                                .push(
                                    widget::container(labelled(
                                        "Port",
                                        widget::text_input(port_hint, draft.machine_port.as_str())
                                            .on_input(Message::DraftMachinePort)
                                            .on_submit(|_| Message::Confirm),
                                    ))
                                    .width(Length::FillPortion(1)),
                                ),
                        )
                        .push_maybe(connecting.is_none().then(|| {
                            widget::row::with_capacity(2)
                                .spacing(8)
                                .align_y(Alignment::Center)
                                .push(
                                    widget::column::with_capacity(2)
                                        .push(widget::text::body("Let in without asking"))
                                        .push(widget::text::caption(
                                            "Its invitations to any network port are accepted. Off, each is asked about.",
                                        ))
                                        .width(Length::Fill),
                                )
                                .push(
                                    widget::toggler(draft.trusted)
                                        .on_toggle(Message::DraftTrusted),
                                )
                        })),
                )
                .primary_action(go)
                .secondary_action(cancel)
                .into()
        }
    }
}

/// Builds the "from → to" line at the top of the route dialogs, "from ↔ to" for a two-way
/// route, naming each connector as other applications see it.
fn preview<'a>(
    app: &'a App,
    from: Option<&(String, u8)>,
    to: Option<&(String, u8)>,
    both_ways: bool,
) -> Element<'a, Message> {
    let end = |pick: Option<&(String, u8)>, inputs: bool| {
        let (id, connector) = pick?;
        let endpoint = app.endpoint(id)?;
        let (ins, outs) = format::connectors(endpoint);
        let count = if inputs { ins } else { outs };
        Some((
            format::section(endpoint),
            format::connector_label(&endpoint.name, count, *connector),
        ))
    };
    let (Some((from_section, from_name)), Some((to_section, to_name))) =
        (end(from, true), end(to, false))
    else {
        return widget::text::caption("Choose where MIDI comes from and where it goes.").into();
    };
    widget::container(
        widget::row::with_capacity(5)
            .spacing(10)
            .align_y(Alignment::Center)
            .push(kind_icon(from_section, 30.0))
            .push(widget::text::heading(from_name))
            .push(widget::text::body(if both_ways { "↔" } else { "→" }))
            .push(kind_icon(to_section, 30.0))
            .push(widget::text::heading(to_name)),
    )
    .padding(12)
    .width(Length::Fill)
    .class(cosmic::theme::Container::Card)
    .into()
}
