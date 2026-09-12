//! The applet: a status icon, and a popup with the daemon's state, a Start/Stop button,
//! a pause toggle, and the tuning knobs behind an "Advanced" fold.
//!
//! Status comes from polling the daemon (`daemon.rs`) on a timer; tuning is read from
//! cosmic-config on start and watched for changes from elsewhere, and every slider
//! writes its key straight back, which is what the daemon watches.

use std::time::{Duration, Instant};

use cosmic::cosmic_config::{self, ConfigSet};
use cosmic::iced::platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup};
use cosmic::iced::widget::container;
use cosmic::iced::{self, Alignment, Border, Color, Length, Subscription, window};
use cosmic::widget::{self, column, icon, row, slider, text, toggler};
use cosmic::{Element, Task, app, applet::padded_control, cosmic_theme::Spacing, theme};
use gaze_config::{CONFIG_ID, CONFIG_VERSION, KNOBS, Knob, Mode, Status, Tuning, TuningStore};
use tracing::warn;

use crate::daemon;
use crate::knobs::format_value;

/// The applet's id, in the desktop entry and the panel config.
const APP_ID: &str = "dev.techgeek1.CosmicGazeApplet";

/// The panel icon: the eye shipped in `data/`, installed under hicolor by `just
/// install`. Symbolic, so it follows the panel's colour.
const ICON_NAME: &str = "dev.techgeek1.CosmicGazeApplet-symbolic";

/// How often the daemon is asked for its state while the popup is closed.
const POLL_CLOSED: Duration = Duration::from_secs(1);

/// How often while it is open, so a mode change shows as it happens.
const POLL_OPEN: Duration = Duration::from_millis(500);

/// The popup's width, logical pixels.
const POPUP_WIDTH: f32 = 340.0;

/// How long after Start the button reads "Starting" while the daemon is not yet on the
/// bus. The daemon claims its name within a second; past this it did not come up, and
/// the button offers Start again (the log says why).
const STARTING_GRACE: Duration = Duration::from_secs(10);

/// The state dot's colours: running, paused, and no daemon or no tracker.
const DOT_RUNNING: Color = Color::from_rgb(0.2, 0.8, 0.2);
const DOT_PAUSED : Color = Color::from_rgb(0.9, 0.7, 0.1);
const DOT_OFF    : Color = Color::from_rgba(0.5, 0.5, 0.5, 0.6);

/// The applet.
pub struct App {
    core     : app::Core,
    popup    : Option<window::Id>,
    /// The daemon's last answer; `None` when it is not on the bus.
    status   : Option<Status>,
    /// Whether the last poll has come back, so a slow bus does not queue polls.
    polling  : bool,
    /// When Start was pressed, until the daemon answers or [`STARTING_GRACE`] passes.
    starting : Option<Instant>,
    /// Whether the knobs are shown.
    advanced : bool,
    tuning   : Tuning,
    /// Where the tuning is written. `None` when cosmic-config is unavailable, in which
    /// case the sliders move the in-memory copy and nothing else.
    store    : Option<TuningStore>,
}

/// Everything that can happen.
#[derive(Clone, Debug)]
pub enum Message {
    TogglePopup,
    PopupClosed(window::Id),
    Poll,
    Status(Option<Status>),
    /// Run the daemon.
    Start,
    /// The daemon was run, or could not be.
    Started(Result<(), String>),
    /// Ask the daemon to exit.
    Stop,
    /// The switch at the top: on runs the daemon, off asks it to exit.
    SetRunning(bool),
    SetPaused(bool),
    /// Ask the daemon for the quick calibration.
    Calibrate,
    /// A fire-and-forget daemon call finished; poll again so the popup catches up.
    Called,
    /// The daemon answered its first poll after a start.
    Ready,
    ToggleAdvanced,
    /// A slider moved: the knob's key and its new value.
    SetKnob(&'static str, f64),
    SetHighlightText(bool),
    ResetTuning,
    /// The tuning changed on disk, from the daemon, another applet or an editor.
    TuningChanged(Tuning),
}

// --- App ---

impl App {
    /// Polls the daemon unless a poll is already out.
    fn poll(&mut self) -> app::Task<Message> {
        if self.polling {
            return Task::none();
        }

        self.polling = true;

        cosmic::task::future(async { Message::Status(daemon::poll().await) })
    }

    /// Writes one tuning key, keeping the in-memory copy either way.
    fn write_key<T: serde::Serialize>(&self, key: &str, value: T) {
        if let Some(store) = &self.store
            && let Err(e) = store.config().set(key, value)
        {
            warn!(key, "writing the tuning key failed: {e}");
        }
    }

    /// The colour the state dot shows.
    fn dot_color(&self) -> Color {
        match &self.status {
            Some(s) if s.paused  => DOT_PAUSED,
            Some(s) if s.tracker => DOT_RUNNING,
            _                    => DOT_OFF,
        }
    }

    /// The Status group: what the daemon is doing beside the switch that runs it, and
    /// whether it is calibrated beside the button that calibrates it. Every row is
    /// always there, so the popup never reflows as the daemon comes and goes.
    fn status_group(&self, spacing: &Spacing) -> Vec<Element<'_, Message>> {
        let running = self.status.is_some();

        let state = match (&self.status, self.starting) {
            (Some(s), _)       => (if s.paused { Mode::Paused } else { s.mode }).label(),
            (None, Some(_))    => "Starting",
            (None, None)       => "Not running",
        };

        // The switch reads as on from the press until the daemon is on the bus or the
        // start is given up on, so it does not flick back while the daemon comes up.
        let on = running || self.starting.is_some();

        let calibrated  = self.status.as_ref().is_some_and(|s| s.calibrated);
        let calibrating = self.status.as_ref().is_some_and(|s| s.mode == Mode::Calibrating);

        let calibrate = match (running, calibrating) {
            (true, false) => widget::button::standard("Calibrate").on_press(Message::Calibrate),
            (true, true)  => widget::button::standard("Calibrating"),
            (false, _)    => widget::button::standard("Calibrate"),
        };

        vec![
            padded_control(text::heading("Status")).into(),
            padded_control(
                row![
                    text::body(state),
                    widget::Space::new().width(Length::Fill),
                    toggler(on).on_toggle(Message::SetRunning),
                ]
                .spacing(spacing.space_xs)
                .align_y(Alignment::Center),
            )
            .into(),
            padded_control(
                row![
                    dot(if calibrated { DOT_RUNNING } else { DOT_OFF }),
                    text::body("Calibrated"),
                    widget::Space::new().width(Length::Fill),
                    calibrate,
                ]
                .spacing(spacing.space_xs)
                .align_y(Alignment::Center),
            )
            .into(),
        ]
    }

    /// The Gaze group: the tracker and the controller, lit when connected.
    fn gaze_group(&self, spacing: &Spacing) -> Vec<Element<'_, Message>> {
        let flags = [
            ("Tracker"    , self.status.as_ref().is_some_and(|s| s.tracker)),
            ("Controller" , self.status.as_ref().is_some_and(|s| s.controller)),
        ];

        let mut rows: Vec<Element<'_, Message>> = vec![padded_control(text::heading("Gaze")).into()];

        rows.extend(flags.into_iter().map(|(label, on)| {
            padded_control(
                row![dot(if on { DOT_RUNNING } else { DOT_OFF }), text::body(label)]
                    .spacing(spacing.space_xs)
                    .align_y(Alignment::Center),
            )
            .into()
        }));

        rows
    }

    /// One knob: its label and value on one line, the slider under them.
    fn knob_row(&self, knob: &'static Knob, spacing: &Spacing) -> Element<'_, Message> {
        let value = self.tuning.get(knob.key).unwrap_or(knob.min);

        let control = slider(knob.min..=knob.max, value, move |v| Message::SetKnob(knob.key, v))
            .step(knob.step)
            .width(Length::Fill);

        padded_control(
            column![
                row![
                    text::body(knob.label),
                    widget::Space::new().width(Length::Fill),
                    text::caption(format_value(knob, value)),
                ]
                .align_y(Alignment::Center),
                control,
            ]
            .spacing(spacing.space_xxxs),
        )
        .into()
    }
}

impl cosmic::Application for App {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags    = ();
    type Message  = Message;

    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut app::Core {
        &mut self.core
    }

    fn style(&self) -> Option<iced::theme::Style> {
        Some(cosmic::applet::style())
    }

    fn init(core: app::Core, _flags: ()) -> (Self, app::Task<Message>) {
        let (store, tuning) = match TuningStore::open() {
            Ok(store) => {
                let tuning = store.load();

                (Some(store), tuning)
            }

            Err(e) => {
                warn!("tuning config unavailable, sliders will not persist: {e}");

                (None, Tuning::default())
            }
        };

        let mut app = App {
            core     : core,
            popup    : None,
            status   : None,
            polling  : false,
            starting : None,
            advanced : false,
            tuning   : tuning,
            store    : store,
        };

        let poll = app.poll();

        (app, poll)
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn update(&mut self, message: Message) -> app::Task<Message> {
        match message {
            Message::TogglePopup => {
                if let Some(id) = self.popup.take() {
                    return Task::batch(vec![destroy_popup(id), popup_open(false)]);
                }

                let id = window::Id::unique();

                self.popup = Some(id);

                let settings = self.core.applet.get_popup_settings(
                    self.core.main_window_id().unwrap(),
                    id,
                    None,
                    None,
                    None,
                );

                return Task::batch(vec![get_popup(settings), popup_open(true), self.poll()]);
            }

            Message::PopupClosed(id) => {
                if self.popup == Some(id) {
                    self.popup = None;

                    return popup_open(false);
                }
            }

            Message::Poll => return self.poll(),

            Message::Status(status) => {
                self.polling = false;

                let came_up = status.is_some() && self.status.is_none();

                self.status = status;

                // Up, or given up on.
                if self.status.is_some()
                    || self.starting.is_some_and(|since| since.elapsed() > STARTING_GRACE)
                {
                    self.starting = None;
                }

                if came_up {
                    return self.update(Message::Ready);
                }
            }

            Message::Start => {
                self.starting = Some(Instant::now());

                return cosmic::task::future(async {
                    Message::Started(daemon::start().map_err(|e| format!("{e:#}")))
                });
            }

            Message::Started(result) => {
                if let Err(e) = result {
                    warn!("starting the daemon failed: {e}");
                    self.starting = None;
                }

                return self.poll();
            }

            Message::Ready => {
                // A daemon that came up while the popup was open has not been told.
                return popup_open(self.popup.is_some());
            }

            Message::SetRunning(on) => {
                let next = if on { Message::Start } else { Message::Stop };

                return self.update(next);
            }

            Message::Stop => {
                return cosmic::task::future(async {
                    daemon::quit().await;

                    Message::Called
                });
            }

            Message::SetPaused(paused) => {
                // Shown at once; the next poll says whether it took.
                if let Some(status) = self.status.as_mut() {
                    status.paused = paused;
                }

                return cosmic::task::future(async move {
                    daemon::set_paused(paused).await;

                    Message::Called
                });
            }

            Message::Calibrate => {
                // Shown at once; the next poll says whether it took.
                if let Some(status) = self.status.as_mut() {
                    status.mode = Mode::Calibrating;
                }

                return cosmic::task::future(async {
                    daemon::calibrate().await;

                    Message::Called
                });
            }

            Message::Called => return self.poll(),

            Message::ToggleAdvanced => self.advanced = !self.advanced,

            Message::SetKnob(key, value) => {
                if self.tuning.set(key, value) {
                    self.write_key(key, value);
                }
            }

            Message::SetHighlightText(on) => {
                self.tuning.highlight_text = on;
                self.write_key("highlight_text", on);
            }

            Message::ResetTuning => {
                self.tuning = Tuning::default();

                if let Some(store) = &self.store
                    && let Err(e) = store.save(&self.tuning)
                {
                    warn!("writing the default tuning failed: {e}");
                }
            }

            Message::TuningChanged(tuning) => self.tuning = tuning,
        }

        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let suggested = self.core.applet.suggested_size(true);
        let icon_size = f32::from(suggested.0.min(suggested.1));
        let dot_size  = (icon_size * 0.3).max(6.0);

        let icon = icon::from_name(ICON_NAME)
            .symbolic(true)
            .size(suggested.0)
            .icon()
            .width(Length::Fixed(icon_size))
            .height(Length::Fixed(icon_size));

        // The state dot sits in the icon's bottom right corner, as claude-status does.
        let content = iced::widget::Stack::new()
            .push(icon)
            .push(
                container(dot_sized(self.dot_color(), dot_size))
                    .width(Length::Fixed(icon_size))
                    .height(Length::Fixed(icon_size))
                    .align_x(Alignment::End)
                    .align_y(Alignment::End),
            )
            .width(Length::Fixed(icon_size))
            .height(Length::Fixed(icon_size));

        self.core
            .applet
            .button_from_element(Element::from(content), false)
            .on_press_down(Message::TogglePopup)
            .into()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Message> {
        let spacing = theme::active().cosmic().spacing;
        let Spacing { space_xxs, space_xs, space_s, .. } = spacing;

        let mut content = column![].spacing(space_xxs).width(Length::Fixed(POPUP_WIDTH));

        for row in self.status_group(&spacing) {
            content = content.push(row);
        }

        content = content.push(
            padded_control(widget::divider::horizontal::default()).padding([space_xxs, space_s]),
        );

        for row in self.gaze_group(&spacing) {
            content = content.push(row);
        }

        content = content.push(
            padded_control(widget::divider::horizontal::default()).padding([space_xxs, space_s]),
        );

        // --- advanced ---

        let chevron = if self.advanced { "go-down-symbolic" } else { "go-next-symbolic" };

        content = content.push(padded_control(
            widget::button::custom(
                row![
                    text::heading("Advanced"),
                    widget::Space::new().width(Length::Fill),
                    icon::from_name(chevron).symbolic(true).size(16).icon(),
                ]
                .align_y(Alignment::Center)
                .width(Length::Fill),
            )
            .class(theme::Button::Text)
            .on_press(Message::ToggleAdvanced),
        ));

        if self.advanced {
            let paused = self.status.as_ref().is_some_and(|s| s.paused);

            content = content.push(padded_control(
                row![
                    text::body("Paused"),
                    widget::Space::new().width(Length::Fill),
                    toggler(paused).on_toggle(Message::SetPaused),
                ]
                .spacing(space_xs)
                .align_y(Alignment::Center),
            ));

            content = content.push(padded_control(
                row![
                    text::body("Highlight text"),
                    widget::Space::new().width(Length::Fill),
                    toggler(self.tuning.highlight_text).on_toggle(Message::SetHighlightText),
                ]
                .spacing(space_xs)
                .align_y(Alignment::Center),
            ));

            for knob in KNOBS {
                content = content.push(self.knob_row(knob, &spacing));
            }

            content = content.push(padded_control(
                widget::button::standard("Reset to defaults").on_press(Message::ResetTuning),
            ));
        }

        content = content.padding([8, 0]);

        self.core
            .applet
            .popup_container(container(content))
            .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        let period = if self.popup.is_some() { POLL_OPEN } else { POLL_CLOSED };

        let tuning = cosmic_config::config_subscription::<_, Tuning>(
            "tuning",
            CONFIG_ID.into(),
            CONFIG_VERSION,
        )
        .map(|update| {
            for e in &update.errors {
                warn!("tuning key unreadable, keeping the default: {e}");
            }

            Message::TuningChanged(update.config)
        });

        Subscription::batch(vec![
            iced::time::every(period).map(|_| Message::Poll),
            tuning,
        ])
    }
}

// --- Widgets ---

/// An 8 px dot in `color`, for the status rows.
/// Tells the daemon the popup's state, then polls.
fn popup_open(open: bool) -> app::Task<Message> {
    cosmic::task::future(async move {
        daemon::set_popup_open(open).await;

        Message::Called
    })
}

fn dot(color: Color) -> Element<'static, Message> {
    dot_sized(color, 8.0)
}

/// A round dot of `size` logical pixels in `color`.
fn dot_sized(color: Color, size: f32) -> Element<'static, Message> {
    container(widget::Space::new().width(size).height(size))
        .class(theme::Container::custom(move |_| container::Style {
            background : Some(color.into()),
            border     : Border::default().rounded(size / 2.0),
            ..Default::default()
        }))
        .into()
}
