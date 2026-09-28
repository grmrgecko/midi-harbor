//! Small pieces the pages are built from: kind icons, status pills, rows and headers.

use crate::app::Message;
use crate::format::{Section, Tone};
use cosmic::iced::{Alignment, Background, Border, Color, Length};
use cosmic::prelude::*;
use cosmic::widget::{self, icon, svg};
use cosmic::{Theme, theme};

/// Returns an icon's SVG paths, drawn in a 24-unit box with `C` standing for its colour.
fn paths(name: &str) -> &'static str {
    match name {
        "virtual" => {
            r#"<circle cx="12" cy="12" r="8"/><circle cx="8.5" cy="11" r="1" fill="C"/><circle cx="15.5" cy="11" r="1" fill="C"/><circle cx="12" cy="8" r="1" fill="C"/><circle cx="12" cy="15.5" r="1" fill="C"/>"#
        }
        "network" => {
            r#"<circle cx="12" cy="12" r="8.5"/><path d="M3.5 12h17M12 3.5c2.5 2.5 3.5 5.3 3.5 8.5s-1 6-3.5 8.5M12 3.5C9.5 6 8.5 8.8 8.5 12s1 6 3.5 8.5"/>"#
        }
        "bluetooth" => r#"<path d="M7 7l10 10-5 4V3l5 4L7 17"/>"#,
        "hardware" => {
            r#"<rect x="2.5" y="6" width="19" height="12" rx="2"/><path d="M7 6v7M11 6v7M15 6v7M19 6v7"/>"#
        }
        "provided" => {
            r#"<rect x="3" y="4" width="18" height="12" rx="2"/><path d="M8 20h8M12 16v4"/>"#
        }
        "routes" => {
            r#"<circle cx="5" cy="6" r="2.2"/><circle cx="5" cy="18" r="2.2"/><circle cx="19" cy="12" r="2.2"/><path d="M7.2 6c6 0 5 6 9.6 6M7.2 18c6 0 5-6 9.6-6"/>"#
        }
        "activity" => r#"<path d="M3 12h4l3-7 4 14 3-7h4"/>"#,
        "monitor" => {
            r#"<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M7 9h10M7 13h6M7 17h8"/>"#
        }
        "settings" => {
            r#"<circle cx="12" cy="12" r="3"/><path d="M12 2.5v3M12 18.5v3M2.5 12h3M18.5 12h3M5.3 5.3l2.1 2.1M16.6 16.6l2.1 2.1M5.3 18.7l2.1-2.1M16.6 7.4l2.1-2.1"/>"#
        }
        "list" => {
            r#"<path d="M8 6h13M8 12h13M8 18h13"/><circle cx="4" cy="6" r="1" fill="C"/><circle cx="4" cy="12" r="1" fill="C"/><circle cx="4" cy="18" r="1" fill="C"/>"#
        }
        _ => r#"<circle cx="12" cy="12" r="8"/>"#,
    }
}

/// Returns the icon name for a section.
fn section_icon(section: Section) -> &'static str {
    match section {
        Section::VirtualPorts => "virtual",
        Section::NetworkPorts => "network",
        Section::Bluetooth => "bluetooth",
        Section::Hardware => "hardware",
        Section::Provided => "provided",
    }
}

/// Returns the colour that marks a section's endpoints wherever they appear.
///
/// Fixed rather than taken from the theme: the kinds need five hues that stay apart from each
/// other, and the theme offers an accent and three states.
fn section_colour(section: Section) -> Color {
    match section {
        Section::VirtualPorts => Color::from_rgb8(0x63, 0xd0, 0xdf),
        Section::NetworkPorts => Color::from_rgb8(0xb3, 0x9d, 0xdb),
        Section::Bluetooth => Color::from_rgb8(0x6f, 0xa8, 0xff),
        Section::Hardware => Color::from_rgb8(0xf2, 0xb6, 0x6d),
        Section::Provided => Color::from_rgb8(0xa3, 0xa8, 0xae),
    }
}

/// Builds an SVG document for an icon in one colour.
fn document(name: &str, colour: &str) -> Vec<u8> {
    let body = paths(name).replace("fill=\"C\"", &format!("fill=\"{colour}\""));
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="{colour}" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">{body}</svg>"#
    )
    .into_bytes()
}

/// Returns a symbolic icon for the nav bar, which libcosmic tints to match the theme.
///
/// Drawn here rather than named, because a named icon comes from the freedesktop theme, which
/// macOS does not have.
pub fn nav_icon(name: &str) -> icon::Icon {
    icon::Handle {
        symbolic: true,
        ..icon::from_svg_bytes(document(name, "#000000"))
    }
    .icon()
}

/// Builds a section's icon on a tinted square.
pub fn kind_icon(section: Section, size: f32) -> Element<'static, Message> {
    let colour = section_colour(section);
    let image = svg(svg::Handle::from_memory(document(
        section_icon(section),
        &hex(colour),
    )))
    .width(Length::Fixed(size * 0.56))
    .height(Length::Fixed(size * 0.56));
    widget::container(image)
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .class(tinted(move |_| colour, 0.15, size * 0.27))
        .into()
}

/// Returns a container style filled with a translucent colour, chosen against the theme.
fn tinted(
    colour: impl Fn(&Theme) -> Color + 'static,
    alpha: f32,
    radius: f32,
) -> theme::Container<'static> {
    theme::Container::custom(move |theme| widget::container::Style {
        background: Some(Background::Color(Color {
            a: alpha,
            ..colour(theme)
        })),
        border: Border {
            radius: radius.into(),
            ..Border::default()
        },
        ..widget::container::Style::default()
    })
}

/// Returns a tone's colour in the active theme, so states stay legible in light and dark.
pub fn tone_colour(theme: &Theme, tone: Tone) -> Color {
    let cosmic = theme.cosmic();
    match tone {
        Tone::Good => cosmic.success_color().into(),
        Tone::Busy => cosmic.accent_color().into(),
        Tone::Waiting => cosmic.warning_color().into(),
        Tone::Bad => cosmic.destructive_color().into(),
        Tone::Off => cosmic.palette.neutral_6.into(),
    }
}

/// Builds a coloured status pill.
pub fn pill(tone: Tone, state: impl Into<String>) -> Element<'static, Message> {
    widget::container(
        widget::row::with_capacity(2)
            .spacing(6)
            .align_y(Alignment::Center)
            .push(dot(tone))
            .push(widget::text::caption(state.into()).class(tone_class(tone))),
    )
    .padding([2, 10])
    .class(tinted(move |theme| tone_colour(theme, tone), 0.14, 10.0))
    .into()
}

/// Builds a small dot in a tone's colour.
pub fn dot(tone: Tone) -> Element<'static, Message> {
    widget::container(widget::Space::new().width(8).height(8))
        .class(tinted(move |theme| tone_colour(theme, tone), 1.0, 4.0))
        .into()
}

/// Builds a caption in a tone's colour.
pub fn toned(tone: Tone, text: impl Into<String>) -> Element<'static, Message> {
    widget::text::caption(text.into())
        .class(tone_class(tone))
        .into()
}

/// Maps a tone onto a text colour from the active theme.
///
/// `Text::Custom` takes a plain function pointer, so each tone needs its own function rather
/// than a closure over the tone.
pub fn tone_class(tone: Tone) -> theme::Text {
    match tone {
        Tone::Good => theme::Text::Custom(|theme| coloured(theme.cosmic().success_color())),
        Tone::Busy => theme::Text::Accent,
        Tone::Waiting => theme::Text::Custom(|theme| coloured(theme.cosmic().warning_color())),
        Tone::Bad => theme::Text::Custom(|theme| coloured(theme.cosmic().destructive_color())),
        Tone::Off => theme::Text::Default,
    }
}

/// Builds a text style that overrides only the colour, leaving selection rendering alone.
fn coloured(colour: cosmic::cosmic_theme::palette::Srgba) -> cosmic::iced::widget::text::Style {
    cosmic::iced::widget::text::Style {
        color: Some(colour.into()),
        ..cosmic::iced::widget::text::Style::default()
    }
}

/// Builds a page's title with its one-line summary and buttons on the right.
pub fn page_header<'a>(
    title: &'a str,
    summary: impl Into<String>,
    buttons: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut row = widget::row::with_capacity(buttons.len() + 1)
        .spacing(8)
        .align_y(Alignment::Center)
        .push(
            widget::column::with_capacity(2)
                .push(widget::text::title3(title))
                .push(widget::text::caption(summary.into()))
                .width(Length::Fill),
        );
    for button in buttons {
        row = row.push(button);
    }
    row.into()
}

/// Builds a clickable row: an icon, a first line with trailing parts, and a detail line.
///
/// Two lines, so a narrow window, or one with the side panel open, wraps the detail rather than
/// squeezing it into a column beside fixed-width parts.
pub fn row<'a>(
    section: Section,
    name: String,
    trailing: Vec<Element<'a, Message>>,
    detail: String,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    let mut first = widget::row::with_capacity(trailing.len() + 1)
        .spacing(10)
        .align_y(Alignment::Center)
        .push(widget::text::body(name).width(Length::Fill));
    for part in trailing {
        first = first.push(part);
    }
    let body = widget::row::with_capacity(2)
        .spacing(14)
        .align_y(Alignment::Center)
        .push(kind_icon(section, 34.0))
        .push(
            widget::column::with_capacity(2)
                .spacing(2)
                .push(first)
                .push(widget::text::caption(detail))
                .width(Length::Fill),
        );
    // A row with nothing to open is drawn plainly: a button with nothing to press reads as
    // disabled, greying out a row whose own buttons still work.
    match on_press {
        Some(message) => widget::button::custom(body)
            .class(theme::Button::MenuItem)
            .padding([4, 4])
            .width(Length::Fill)
            .on_press(message)
            .into(),
        None => widget::container(body)
            .padding([4, 4])
            .width(Length::Fill)
            .into(),
    }
}

/// Builds a field with its label above it.
pub fn labelled<'a>(
    label: &'a str,
    control: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    widget::column::with_capacity(2)
        .spacing(6)
        .push(widget::text::caption(label))
        .push(control)
        .into()
}

/// Builds a labelled count with minus and plus buttons, as Audio MIDI Setup's connector counts,
/// held between one and sixteen.
pub fn stepper<'a>(
    label: &'a str,
    detail: &'a str,
    value: u8,
    on_change: fn(u8) -> Message,
) -> Element<'a, Message> {
    let mut less = widget::button::standard("−");
    if value > 1 {
        less = less.on_press(on_change(value - 1));
    }
    let mut more = widget::button::standard("+");
    if value < 16 {
        more = more.on_press(on_change(value + 1));
    }
    widget::row::with_capacity(4)
        .spacing(8)
        .align_y(Alignment::Center)
        .push(
            widget::column::with_capacity(2)
                .push(widget::text::body(label))
                .push(widget::text::caption(detail))
                .width(Length::Fill),
        )
        .push(less)
        .push(
            widget::text::title4(value.to_string())
                .width(Length::Fixed(32.0))
                .align_x(Alignment::Center),
        )
        .push(more)
        .into()
}

/// Builds the placeholder shown where a list has nothing in it, saying what would appear.
pub fn empty(text: impl Into<String>) -> Element<'static, Message> {
    widget::container(widget::text::caption(text.into()))
        .padding(12)
        .width(Length::Fill)
        .into()
}

/// Formats a colour as a CSS hex string for the SVGs.
fn hex(colour: Color) -> String {
    let [r, g, b, _] = colour.into_rgba8();
    format!("#{r:02x}{g:02x}{b:02x}")
}
