//! The application: a libcosmic window the user simply navigates.
//!
//! There is no task and no target. The window is one of six generated archetypes
//! ([`AppKind`]) and the user clicks around it the way they would any application; the
//! switcher in the header bar and the shuffle button beside it generate a fresh
//! window whenever the current one stops being interesting. Every press inside the
//! window goes to the collector through [`Link`], with the widget's box when a
//! [`Probe`] saw it and without one otherwise. [`Coverage`] steers where the furniture
//! goes rather than what to press: the emptier half of the screen gets the sidebar and
//! the toolbar more often. Every so many labelled presses the window turns into a
//! posture prompt, and the posture stays on the wire until the next prompt.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cosmic::iced::alignment::{Horizontal, Vertical};
use cosmic::iced::{self, Length, Point, Subscription, mouse, window};
use cosmic::widget::{self, popover};
use cosmic::{Core, Element, theme};
use gaze_core::{GlobalPx, TrainerElement, TrainerMessage, TrainerTag};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tracing::{info, warn};

use crate::coverage::Coverage;
use crate::link::Link;
use crate::probe::{Label, Press, Probe, Registry, Shared, now_unix_s};
use crate::scene::{AppKind, Bias, Dialog, Edge, FieldKind, Page, Scene, Side, ThemeKind, WidgetId};

/// How often the clock ticks. Full screen, the theme and the posture prompt are all
/// decided here, so this bounds how late any of them is.
const TICK: Duration = Duration::from_millis(60);

/// Two probe messages closer than this are the same press (a popup item and the
/// widget beneath it). The first wins.
const SAME_PRESS_S: f64 = 0.020;

/// A raw press this long after the last probe message was on nothing labelled.
const UNLABELLED_AFTER_S: f64 = 0.050;

/// Width of a card in the store grid, logical pixels.
const CARD_W: f32 = 220.0;

/// Horizontal pitch of the store grid: a card plus the gap beside it.
const CARD_PITCH: f32 = 240.0;

/// Padding the grid sits inside, both sides together.
const GRID_PAD: f32 = 64.0;

/// The posture prompts, in the order they cycle.
const POSTURES: &[(&str, &str)] = &[
    ("normal", "Sit the way you normally do."),
    ("back",   "Sit back in your chair, a hand's width further from the screen."),
    ("in",     "Lean in toward the screen."),
    ("left",   "Lean to the left."),
    ("normal", "Back to how you normally sit."),
    ("right",  "Lean to the right."),
    ("tall",   "Sit up tall."),
    ("slouch", "Slouch down a little."),
];

/// Command-line configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// Desk geometry, for the output's origin.
    pub desk          : PathBuf,
    /// The output the window is full screen on.
    pub output        : String,
    /// Session files, for the coverage histogram.
    pub sessions      : PathBuf,
    /// Labelled presses between posture prompts.
    pub posture_every : u32,
    /// Do not warn about a missing collector.
    pub offline       : bool,
}

/// Application messages.
#[derive(Clone, Debug)]
pub enum Message {
    Tick,
    Cursor(Point),
    RawPress(mouse::Button),
    Probe(Press),
    Clicked(WidgetId),
    Input(WidgetId, String),
    MenuClosed,
    DropdownClosed,
}

/// What the window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    /// The generated application.
    App,
    /// Index into [`POSTURES`].
    Posture(usize),
}

/// The application state.
pub struct App {
    core         : Core,
    config       : Config,
    registry     : Shared,
    link         : Link,
    rng          : StdRng,
    scene        : Scene,
    screen       : Screen,
    coverage     : Coverage,
    /// Window size, logical pixels.
    window       : (f32, f32),
    cursor       : Point,
    /// Windows generated so far; the wire's `task`.
    generation   : u64,
    /// Labelled presses since the last posture prompt.
    since_prompt : u32,
    /// Labelled presses this session, for the status line.
    labelled     : u64,
    /// Index of the posture last asked for.
    posture      : usize,
    /// The theme last pushed to the toolkit.
    theme_shown  : Option<ThemeKind>,
    /// Wall-clock time of the last probe message.
    last_probe_s : f64,
    /// Whether the window has been sent full screen.
    fullscreen   : bool,
    /// Where the dialog card of the posture screen sits, as fractions.
    prompt_anchor: (f32, f32),
}

// --- Application ---

impl cosmic::Application for App {
    type Executor = cosmic::executor::Default;
    type Flags    = Config;
    type Message  = Message;

    const APP_ID: &'static str = "dev.techgeek1.GazeTrainer";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, config: Config) -> (Self, cosmic::app::Task<Message>) {
        let mut rng      = StdRng::from_os_rng();
        let mut coverage = Coverage::new();

        match load_output(&config) {
            Ok((origin, size)) => {
                match coverage.load_sessions(&config.sessions, &config.output, origin, size) {
                    Ok(n)  => info!(clicks = n, "coverage seeded from the session files"),
                    Err(e) => warn!(error = %e, "could not read the session files"),
                }
            }
            Err(e) => warn!(error = %e, "no desk geometry; coverage starts empty"),
        }

        let kind  = AppKind::random(&mut rng);
        let scene = Scene::generate(kind, bias_from(&coverage), &mut rng);

        info!(generation = 1, kind = ?kind, "first window generated");

        let mut app = App {
            core         : core,
            config       : config,
            registry     : Arc::new(Mutex::new(Registry::default())),
            link         : Link::spawn(now_unix_s()),
            rng          : rng,
            scene        : scene,
            screen       : Screen::Posture(0),
            coverage     : coverage,
            window       : (1920.0, 1080.0),
            cursor       : Point::ORIGIN,
            generation   : 1,
            since_prompt : 0,
            labelled     : 0,
            posture      : 0,
            theme_shown  : None,
            last_probe_s : 0.0,
            fullscreen   : false,
            prompt_anchor: (0.5, 0.5),
        };

        app.core.set_header_title(app.scene.kind.name().to_string());

        (app, cosmic::app::Task::none())
    }

    fn header_start(&self) -> Vec<Element<'_, Message>> {
        if self.screen != Screen::App {
            return Vec::new();
        }

        let frame = self.frame();

        self.scene.menus.iter().enumerate().map(|(m, menu)| {
            let m    = m as u8;
            let root = widget::button::text(menu.title.clone()).on_press(Message::Clicked(WidgetId::Menu(m)));
            let root = self.probe(frame, WidgetId::Menu(m), root);

            let mut pop = popover(root).position(popover::Position::Bottom);

            if self.scene.open_menu == Some(m) {
                let items = menu.items.iter().enumerate().map(|(i, item)| {
                    let button = widget::button::text(item.clone())
                        .width(Length::Fill)
                        .on_press(Message::Clicked(WidgetId::MenuItem(m, i as u8)));

                    self.probe(frame, WidgetId::MenuItem(m, i as u8), button).width(Length::Fill).into()
                }).collect::<Vec<Element<'_, Message>>>();

                let popup = widget::container(widget::column::with_children(items).spacing(2.0))
                    .padding(6.0)
                    .width(Length::Fixed(260.0))
                    .class(theme::Container::Dropdown);

                pop = pop.popup(popup).modal(true).on_close(Message::MenuClosed);
            }

            pop.into()
        }).collect()
    }

    fn header_end(&self) -> Vec<Element<'_, Message>> {
        if self.screen != Screen::App {
            return Vec::new();
        }

        let frame = self.frame();

        // One icon per archetype, the current one lit, then the shuffle. These are the
        // only way to leave a window, so they are always drawn.
        let mut items: Vec<Element<'_, Message>> = AppKind::ALL.iter().enumerate()
            .map(|(k, kind)| {
                let k      = k as u8;
                let active = *kind == self.scene.kind;

                let button = widget::button::icon(widget::icon::from_name(kind.icon()))
                    .class(if active { theme::Button::Suggested } else { theme::Button::Icon })
                    .on_press(Message::Clicked(WidgetId::Switch(k)));

                self.probe(frame, WidgetId::Switch(k), button).into()
            })
            .collect();

        let shuffle = widget::button::icon(widget::icon::from_name("media-playlist-shuffle-symbolic"))
            .on_press(Message::Clicked(WidgetId::Shuffle));

        items.push(self.probe(frame, WidgetId::Shuffle, shuffle).into());

        if self.scene.show_search {
            // A browser's search field is its address bar, and reads as one.
            let width = match self.scene.kind {
                AppKind::Browser => 420.0,
                _                => 280.0,
            };

            let input = widget::text_input::search_input("Search", &self.scene.search)
                .on_input(|v| Message::Input(WidgetId::SearchInput, v))
                .width(Length::Fixed(width));

            let button = widget::button::standard("Search")
                .on_press(Message::Clicked(WidgetId::SearchButton));

            items.push(self.probe(frame, WidgetId::SearchInput, input).into());
            items.push(self.probe(frame, WidgetId::SearchButton, button).into());
        }

        items
    }

    fn update(&mut self, message: Message) -> cosmic::app::Task<Message> {
        match message {
            Message::Tick             => return self.tick(),
            Message::Cursor(p)        => self.cursor = p,
            Message::RawPress(button) => self.raw_press(button),
            Message::Probe(press)     => self.probe_press(press),
            Message::Clicked(id)      => self.clicked(id),
            Message::Input(id, value) => self.input(id, value),
            Message::MenuClosed       => self.scene.open_menu = None,
            Message::DropdownClosed   => self.scene.open_dropdown = None,
        }

        cosmic::app::Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(TICK).map(|_| Message::Tick),
            iced::event::listen_with(|event, _status, _id| match event {
                iced::Event::Mouse(mouse::Event::CursorMoved { position }) => Some(Message::Cursor(position)),
                iced::Event::Mouse(mouse::Event::ButtonPressed(button))    => Some(Message::RawPress(button)),
                _                                                          => None,
            }),
        ])
    }

    fn on_window_resize(&mut self, _id: window::Id, width: f32, height: f32) {
        self.window = (width, height);
    }

    fn view(&self) -> Element<'_, Message> {
        let frame = self.registry.lock().map(|mut r| r.next_frame()).unwrap_or(0);

        match self.screen {
            Screen::Posture(i) => self.view_prompt(frame, i),
            Screen::App        => self.view_app(frame),
        }
    }
}

// --- Updates ---

impl App {
    /// The layout generation `view` is building, for probes made outside `view`.
    fn frame(&self) -> u64 {
        // `header_start` and `header_end` run after `view` in the same frame, so the
        // registry already carries this frame's generation. Peeking does not bump it.
        self.registry.lock().map(|r| r.current()).unwrap_or(0)
    }

    /// Wraps `content` as the labelled control `id`.
    fn probe<'a>(&self, frame: u64, id: WidgetId, content: impl Into<Element<'a, Message>>)
        -> Probe<'a, Message>
    {
        let label = self.scene.label(id).unwrap_or(Label { id: id, kind: "unknown", text: None });

        Probe::new(&self.registry, frame, label, content, Message::Probe)
    }

    /// The periodic tick: full screen once, the theme when it changes, and the posture
    /// prompt once enough labelled presses have gone by.
    fn tick(&mut self) -> cosmic::app::Task<Message> {
        let mut tasks = Vec::new();

        if !self.fullscreen
            && let Some(id) = self.core.main_window_id()
        {
            self.fullscreen = true;
            tasks.push(window::set_mode(id, window::Mode::Fullscreen));
        }

        if self.theme_shown != Some(self.scene.theme) {
            self.theme_shown = Some(self.scene.theme);

            let theme = match self.scene.theme {
                ThemeKind::Dark  => cosmic::Theme::dark(),
                ThemeKind::Light => cosmic::Theme::light(),
            };

            tasks.push(cosmic::command::set_theme(theme));
        }

        // The prompt is decided here rather than in the press handler so it never
        // lands between a press and the click it turns into.
        if self.prompt_due() {
            let next = (self.posture + 1) % POSTURES.len();

            self.start_prompt(next);
        }

        cosmic::app::Task::batch(tasks)
    }

    /// Whether a posture prompt is due and nothing transient is open over the window.
    fn prompt_due(&self) -> bool {
        self.screen == Screen::App
            && self.since_prompt >= self.config.posture_every
            && self.scene.dialog.is_none()
            && self.scene.open_menu.is_none()
            && self.scene.open_dropdown.is_none()
    }

    /// A press a probe saw. Every one of these is a labelled control.
    fn probe_press(&mut self, press: Press) {
        if press.t_unix_s - self.last_probe_s < SAME_PRESS_S {
            return;
        }

        self.last_probe_s = press.t_unix_s;
        self.labelled    += 1;

        if self.screen == Screen::App {
            self.since_prompt += 1;
        }

        self.coverage.add(
            f64::from(press.px.x / self.window.0.max(1.0)),
            f64::from(press.px.y / self.window.1.max(1.0)),
        );

        let element = TrainerElement {
            kind : press.label.kind.to_string(),
            bbox : gaze_core::Rect {
                x : f64::from(press.bounds.x),
                y : f64::from(press.bounds.y),
                w : f64::from(press.bounds.width),
                h : f64::from(press.bounds.height),
            },
            text : press.label.text.clone(),
        };

        self.send_press(press.t_unix_s, press.button, press.px, Some(element), true);
    }

    /// A press the runtime saw. On nothing labelled unless a probe spoke first.
    fn raw_press(&mut self, button: mouse::Button) {
        let now = now_unix_s();

        if now - self.last_probe_s < UNLABELLED_AFTER_S {
            return;
        }

        self.send_press(now, button, self.cursor, None, false);
    }

    /// Sends one press to the collector.
    fn send_press(
        &self,
        t_unix_s : f64,
        button   : mouse::Button,
        px       : Point,
        element  : Option<TrainerElement>,
        hit      : bool,
    ) {
        let button = match button {
            mouse::Button::Left  => "left",
            mouse::Button::Right => "right",
            _                    => return,
        };

        self.link.send(&TrainerMessage::Press {
            t_unix_s : t_unix_s,
            button   : button.to_string(),
            px       : [f64::from(px.x), f64::from(px.y)],
            element  : element,
            luma     : theme_luma(),
            tag      : TrainerTag {
                task    : self.generation,
                step    : 0,
                hit     : hit,
                posture : POSTURES[self.posture].0.to_string(),
                theme   : self.scene.theme.name().to_string(),
            },
        });
    }

    /// A control's own click.
    fn clicked(&mut self, id: WidgetId) {
        if self.screen != Screen::App {
            if id == WidgetId::Continue {
                let kind = AppKind::random(&mut self.rng);

                self.next_scene(kind);
            }

            return;
        }

        match id {
            WidgetId::Switch(k) => {
                let Some(kind) = AppKind::ALL.get(usize::from(k)).copied() else {
                    return;
                };

                self.next_scene(kind);
            }

            WidgetId::Shuffle   => {
                let kind = AppKind::random(&mut self.rng);

                self.next_scene(kind);
            }

            _                   => self.scene.apply(id, &mut self.rng),
        }
    }

    /// Text typed into a field.
    fn input(&mut self, id: WidgetId, value: String) {
        match id {
            WidgetId::SearchInput => self.scene.search = value,
            WidgetId::Field(i)    => {
                if let Page::Form { fields } = self.scene.page_mut()
                    && let Some(field) = fields.get_mut(usize::from(i))
                    && let FieldKind::Text { value: v } = &mut field.kind
                {
                    *v = value;
                }
            }
            _                     => {}
        }
    }

    /// A fresh window of archetype `kind`, with the furniture where coverage wants it.
    fn next_scene(&mut self, kind: AppKind) {
        let bias = bias_from(&self.coverage);

        self.generation += 1;
        self.scene       = Scene::generate(kind, bias, &mut self.rng);
        self.screen      = Screen::App;

        self.core.set_header_title(kind.name().to_string());

        info!(generation = self.generation, kind = ?kind, "window generated");
    }

    /// Shows posture prompt `i` with its Continue button somewhere new.
    fn start_prompt(&mut self, i: usize) {
        self.posture      = i;
        self.screen       = Screen::Posture(i);
        self.since_prompt = 0;

        let anchors = [0.1_f32, 0.5, 0.9];

        self.prompt_anchor = (
            anchors[self.rng.random_range(0..3)],
            anchors[self.rng.random_range(0..3)],
        );

        info!(posture = POSTURES[i].0, "posture prompt");
    }
}

// --- Views ---

impl App {
    /// The status line at the bottom of the window.
    fn status(&self) -> String {
        let link = {
            if self.link.connected() {
                format!("collector connected, {} sent", self.link.sent())
            }
            else if self.config.offline {
                "offline".to_string()
            }
            else {
                format!("COLLECTOR NOT CONNECTED — {} presses lost", self.link.dropped())
            }
        };

        let (min, max) = self.coverage.range();

        format!(
            "{} #{} · {} labelled · coverage {}–{} per bin · posture: {} · {}",
            self.scene.kind.name(), self.generation, self.labelled, min, max,
            POSTURES[self.posture].0, link,
        )
    }

    /// The posture prompt.
    fn view_prompt(&self, frame: u64, i: usize) -> Element<'_, Message> {
        let (_, instruction) = POSTURES[i];

        let button = widget::button::suggested("Continue").on_press(Message::Clicked(WidgetId::Continue));

        let card = widget::container(
            widget::column::with_children(vec![
                widget::text::title2(instruction).into(),
                widget::text::body("Hold that posture. Continue opens a new window to look around in.").into(),
                self.probe(frame, WidgetId::Continue, button).into(),
            ])
            .spacing(24.0)
            .align_x(Horizontal::Center),
        )
        .padding(32.0)
        .max_width(640.0)
        .class(theme::Container::Card);

        let (ax, ay) = self.prompt_anchor;

        widget::column::with_children(vec![
            widget::container(card)
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(48.0)
                .align_x(align_h(ax))
                .align_y(align_v(ay))
                .into(),
            self.view_status(),
        ])
        .into()
    }

    /// The application screen.
    fn view_app(&self, frame: u64) -> Element<'_, Message> {
        let s = &self.scene;

        // An archetype with no tools has no toolbar at all rather than an empty bar.
        let toolbar = (!s.tools.is_empty()).then(|| {
            let tools = s.tools.iter().enumerate().map(|(i, (icon, _))| {
                let button = widget::button::icon(widget::icon::from_name(*icon))
                    .on_press(Message::Clicked(WidgetId::Tool(i as u8)));

                self.probe(frame, WidgetId::Tool(i as u8), button).into()
            }).collect::<Vec<Element<'_, Message>>>();

            widget::container(widget::row::with_children(tools).spacing(s.density.gap()))
                .padding([6.0, 12.0])
                .width(Length::Fill)
        });

        let sidebar = s.side.map(|_| {
            let items = s.nav.iter().enumerate().map(|(i, name)| {
                let active = i as u8 == s.active_nav;
                let button = widget::button::custom(widget::text::body(name.clone()))
                    .width(Length::Fill)
                    .padding([s.density.row_pad(), 12.0])
                    .class(if active { theme::Button::Suggested } else { theme::Button::Text })
                    .on_press(Message::Clicked(WidgetId::Nav(i as u8)));

                self.probe(frame, WidgetId::Nav(i as u8), button).width(Length::Fill).into()
            }).collect::<Vec<Element<'_, Message>>>();

            widget::container(
                widget::scrollable(widget::column::with_children(items).spacing(s.density.gap() / 2.0)),
            )
            .padding(8.0)
            .width(Length::Fixed(s.sidebar_w))
            .height(Length::Fill)
            .class(theme::Container::Background)
        });

        // The tab bar only exists when the archetype has tabs.
        let tabs = (!s.tabs.is_empty()).then(|| {
            let items = s.tabs.iter().enumerate().map(|(t, title)| {
                let active = t as u8 == s.active_tab;
                let button = widget::button::custom(widget::text::body(title.clone()))
                    .padding([8.0, 16.0])
                    .class(if active { theme::Button::Suggested } else { theme::Button::Text })
                    .on_press(Message::Clicked(WidgetId::Tab(t as u8)));

                self.probe(frame, WidgetId::Tab(t as u8), button).into()
            }).collect::<Vec<Element<'_, Message>>>();

            widget::container(widget::row::with_children(items).spacing(4.0)).padding([8.0, 16.0])
        });

        let content: Element<'_, Message> = {
            match s.split {
                true  => self.view_split(frame),
                false => self.view_scrollable_page(frame, s.page()),
            }
        };

        // Only an archetype that commits something has an action row.
        let actions = (!s.actions.is_empty()).then(|| {
            let buttons = s.actions.iter().enumerate().map(|(i, name)| {
                let i      = i as u8;
                let button = {
                    if Some(i) == s.primary_action() {
                        widget::button::suggested(name.clone())
                    }
                    else {
                        widget::button::standard(name.clone())
                    }
                }
                .on_press(Message::Clicked(WidgetId::Action(i)));

                self.probe(frame, WidgetId::Action(i), button).into()
            }).collect::<Vec<Element<'_, Message>>>();

            let mut row: Vec<Element<'_, Message>> = vec![iced::widget::Space::new().width(Length::Fill).into()];
            row.extend(buttons);

            widget::container(widget::row::with_children(row).spacing(8.0))
                .padding([8.0, 16.0])
                .width(Length::Fill)
        });

        let mut main_parts: Vec<Element<'_, Message>> = Vec::with_capacity(3);

        main_parts.extend(tabs.map(Into::into));
        main_parts.push(content);
        main_parts.extend(actions.map(Into::into));

        let main = widget::column::with_children(main_parts)
            .width(Length::Fill)
            .height(Length::Fill);

        let body: Element<'_, Message> = match (s.side, sidebar) {
            (Some(Side::Left), Some(bar))  => widget::row::with_children(vec![bar.into(), main.into()]).into(),
            (Some(Side::Right), Some(bar)) => widget::row::with_children(vec![main.into(), bar.into()]).into(),
            _                              => main.into(),
        };

        // The edges are exclusive, so the toolbar is placed exactly once.
        let (toolbar_top, toolbar_bottom) = match s.toolbar_at {
            Edge::Top    => (toolbar, None),
            Edge::Bottom => (None, toolbar),
        };

        let mut parts: Vec<Element<'_, Message>> = Vec::with_capacity(4);

        parts.extend(toolbar_top.map(Into::into));
        parts.push(widget::container(body).height(Length::Fill).into());
        parts.extend(toolbar_bottom.map(Into::into));
        parts.push(self.view_status());

        let base: Element<'_, Message> = widget::column::with_children(parts)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

        match &s.dialog {
            Some(dialog) => iced::widget::stack([base, self.view_dialog(frame, dialog)]).into(),
            None         => base,
        }
    }

    /// The status caption, small, at the bottom of whichever screen is up.
    fn view_status(&self) -> Element<'_, Message> {
        widget::container(widget::text::caption(self.status()))
            .padding([4.0, 16.0])
            .width(Length::Fill)
            .into()
    }

    /// A page in its own scroll area.
    fn view_scrollable_page<'a>(&'a self, frame: u64, page: &'a Page)
        -> Element<'a, Message>
    {
        widget::scrollable(
            widget::container(self.view_page(frame, page)).padding([8.0, 16.0]).width(Length::Fill),
        )
        .height(Length::Fill)
        .into()
    }

    /// The list pane, a divider and the reading pane. Mail only.
    fn view_split(&self, frame: u64) -> Element<'_, Message> {
        let s = &self.scene;

        let list = widget::container(self.view_scrollable_page(frame, s.page()))
            .width(Length::Fixed(s.pane_w))
            .height(Length::Fill);

        let reading: Element<'_, Message> = match &s.reading {
            Some(page) => self.view_scrollable_page(frame, page),
            None       => {
                widget::container(widget::text::body("Select a message to read it."))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(Horizontal::Center)
                    .align_y(Vertical::Center)
                    .into()
            }
        };

        widget::row::with_children(vec![
            list.into(),
            widget::divider::vertical::default().into(),
            widget::container(reading).width(Length::Fill).height(Length::Fill).into(),
        ])
        .into()
    }

    /// One page's content.
    fn view_page<'a>(&'a self, frame: u64, page: &'a Page) -> Element<'a, Message> {
        let s = &self.scene;

        match page {
            Page::List { rows, selected } => {
                let items = rows.iter().enumerate().map(|(i, row)| {
                    let i     = i as u8;
                    let check = widget::checkbox(row.checked)
                        .on_toggle(move |_| Message::Clicked(WidgetId::RowCheck(i)));

                    let label = widget::row::with_children(vec![
                        widget::text::body(row.name.clone()).into(),
                        iced::widget::Space::new().width(Length::Fill).into(),
                        widget::text::caption(row.detail.clone()).into(),
                    ])
                    .spacing(16.0)
                    .align_y(Vertical::Center);

                    let button = widget::button::custom(label)
                        .width(Length::Fill)
                        .padding([s.density.row_pad(), 12.0])
                        .class(if *selected == Some(i) { theme::Button::Suggested } else { theme::Button::Text })
                        .on_press(Message::Clicked(WidgetId::Row(i)));

                    widget::row::with_children(vec![
                        self.probe(frame, WidgetId::RowCheck(i), check).into(),
                        self.probe(frame, WidgetId::Row(i), button).width(Length::Fill).into(),
                    ])
                    .spacing(8.0)
                    .align_y(Vertical::Center)
                    .into()
                }).collect::<Vec<Element<'_, Message>>>();

                widget::column::with_children(items).spacing(s.density.gap() / 2.0).into()
            }

            Page::Form { fields } => {
                let items = fields.iter().enumerate().map(|(i, field)| {
                    let i = i as u8;

                    let control: Element<'_, Message> = match &field.kind {
                        FieldKind::Text { value } => {
                            let input = widget::text_input::text_input("", value)
                                .on_input(move |v| Message::Input(WidgetId::Field(i), v))
                                .width(Length::Fixed(360.0));

                            self.probe(frame, WidgetId::Field(i), input).into()
                        }
                        FieldKind::Dropdown { options, selected } => {
                            let root = widget::button::standard(options[*selected].clone())
                                .on_press(Message::Clicked(WidgetId::Field(i)));
                            let root = self.probe(frame, WidgetId::Field(i), root);

                            let mut pop = popover(root).position(popover::Position::Bottom);

                            if s.open_dropdown == Some(i) {
                                let items = options.iter().enumerate().map(|(j, option)| {
                                    let button = widget::button::text(option.clone())
                                        .width(Length::Fill)
                                        .on_press(Message::Clicked(WidgetId::DropdownItem(i, j as u8)));

                                    self.probe(frame, WidgetId::DropdownItem(i, j as u8), button)
                                        .width(Length::Fill)
                                        .into()
                                }).collect::<Vec<Element<'_, Message>>>();

                                let popup = widget::container(widget::column::with_children(items).spacing(2.0))
                                    .padding(6.0)
                                    .width(Length::Fixed(220.0))
                                    .class(theme::Container::Dropdown);

                                pop = pop.popup(popup).modal(true).on_close(Message::DropdownClosed);
                            }

                            pop.into()
                        }
                        FieldKind::Toggle { on } => {
                            let toggle = widget::toggler(*on)
                                .on_toggle(move |_| Message::Clicked(WidgetId::Toggle(i)));

                            self.probe(frame, WidgetId::Toggle(i), toggle).into()
                        }
                    };

                    widget::row::with_children(vec![
                        widget::container(widget::text::body(field.label.clone()))
                            .width(Length::Fixed(240.0))
                            .into(),
                        control,
                    ])
                    .spacing(16.0)
                    .align_y(Vertical::Center)
                    .into()
                }).collect::<Vec<Element<'_, Message>>>();

                widget::column::with_children(items).spacing(s.density.gap() + 8.0).into()
            }

            Page::Article { title, paragraphs } => {
                let mut items: Vec<Element<'_, Message>> = vec![widget::text::title3(title.clone()).into()];

                for paragraph in paragraphs {
                    items.push(widget::text::body(paragraph.before.clone()).into());

                    if let Some((id, text)) = &paragraph.link {
                        let link = widget::button::link(text.clone())
                            .on_press(Message::Clicked(WidgetId::Link(*id)));

                        items.push(self.probe(frame, WidgetId::Link(*id), link).into());
                        items.push(widget::text::body(paragraph.after.clone()).into());
                    }
                }

                widget::container(widget::column::with_children(items).spacing(s.density.gap() + 4.0))
                    .max_width(900.0)
                    .into()
            }

            Page::Grid { cards } => self.view_grid(frame, cards),
        }
    }

    /// A grid of store cards, as many per row as the content area fits.
    fn view_grid(&self, frame: u64, cards: &[crate::scene::Card]) -> Element<'_, Message> {
        let sidebar_w   = self.scene.side.map(|_| self.scene.sidebar_w).unwrap_or(0.0);
        let available_w = self.window.0 - sidebar_w - GRID_PAD;
        let per_row     = ((available_w / CARD_PITCH).floor() as usize).max(1);

        let rows = cards.chunks(per_row).enumerate().map(|(r, chunk)| {
            let cells = chunk.iter().enumerate().map(|(c, card)| {
                let i = (r * per_row + c) as u8;

                // The card's body and its button are siblings: probes never nest, so
                // the button has to sit outside the body's probe.
                let body = widget::button::custom(
                    widget::column::with_children(vec![
                        widget::text::title4(card.name.clone()).into(),
                        widget::text::caption(card.detail.clone()).into(),
                    ])
                    .spacing(4.0),
                )
                .width(Length::Fill)
                .class(theme::Button::Text)
                .on_press(Message::Clicked(WidgetId::Card(i)));

                let action = widget::button::standard(
                    if card.installed { "Remove" } else { "Install" },
                )
                .on_press(Message::Clicked(WidgetId::CardAction(i)));

                widget::container(
                    widget::column::with_children(vec![
                        self.probe(frame, WidgetId::Card(i), body).width(Length::Fill).into(),
                        self.probe(frame, WidgetId::CardAction(i), action).into(),
                    ])
                    .spacing(8.0)
                    .align_x(Horizontal::Left),
                )
                .padding(12.0)
                .width(Length::Fixed(CARD_W))
                .class(theme::Container::Card)
                .into()
            }).collect::<Vec<Element<'_, Message>>>();

            widget::row::with_children(cells).spacing(16.0).into()
        }).collect::<Vec<Element<'_, Message>>>();

        widget::column::with_children(rows).spacing(16.0).into()
    }

    /// The dialog layer over the content.
    fn view_dialog(&self, frame: u64, dialog: &Dialog) -> Element<'_, Message> {
        let actions = dialog.actions.iter().enumerate().map(|(k, name)| {
            let k      = k as u8;
            let last   = usize::from(k) + 1 == dialog.actions.len();
            let button = {
                if last && name == "Remove" {
                    widget::button::destructive(name.clone())
                }
                else if last {
                    widget::button::suggested(name.clone())
                }
                else {
                    widget::button::standard(name.clone())
                }
            }
            .on_press(Message::Clicked(WidgetId::DialogAction(k)));

            self.probe(frame, WidgetId::DialogAction(k), button).into()
        }).collect::<Vec<Element<'_, Message>>>();

        let mut row: Vec<Element<'_, Message>> = vec![iced::widget::Space::new().width(Length::Fill).into()];
        row.extend(actions);

        let card = widget::container(
            widget::column::with_children(vec![
                widget::text::title3(dialog.title.clone()).into(),
                widget::text::body(dialog.body.clone()).into(),
                widget::row::with_children(row).spacing(8.0).into(),
            ])
            .spacing(16.0),
        )
        .padding(24.0)
        .width(Length::Fixed(420.0))
        .class(theme::Container::Dialog(true));

        let (ax, ay) = dialog.anchor;

        widget::container(card)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(64.0)
            .align_x(align_h(ax))
            .align_y(align_v(ay))
            .into()
    }
}

// --- Helpers ---

/// The layout bias a histogram asks for: the emptier half gets the furniture.
///
/// Clamped away from certainty so neither side is ever ruled out entirely.
fn bias_from(coverage: &Coverage) -> Bias {
    let (left, top) = coverage.halves();

    Bias {
        left : (1.0 - left).clamp(0.2, 0.8),
        top  : (1.0 - top).clamp(0.2, 0.8),
    }
}

/// Horizontal alignment for an anchor fraction.
fn align_h(fx: f32) -> Horizontal {
    if fx < 0.33 {
        Horizontal::Left
    }
    else if fx > 0.66 {
        Horizontal::Right
    }
    else {
        Horizontal::Center
    }
}

/// Vertical alignment for an anchor fraction.
fn align_v(fy: f32) -> Vertical {
    if fy < 0.33 {
        Vertical::Top
    }
    else if fy > 0.66 {
        Vertical::Bottom
    }
    else {
        Vertical::Center
    }
}

/// Mean luminance of the active theme's window background, [0, 1].
fn theme_luma() -> f64 {
    let theme = theme::active();
    let base  = theme.cosmic().background(false).base;

    f64::from(0.2126 * base.red + 0.7152 * base.green + 0.0722 * base.blue)
}

/// The configured output's logical origin and size from the desk config.
fn load_output(config: &Config) -> anyhow::Result<(GlobalPx, (f64, f64))> {
    let text     = std::fs::read_to_string(&config.desk)?;
    let geometry = gaze_core::DesktopGeometry::from_toml(&text)?;

    let out = geometry.outputs.iter()
        .find(|o| o.name == config.output)
        .ok_or_else(|| anyhow::anyhow!("output {} is not in {}", config.output, config.desk.display()))?;

    Ok((GlobalPx { x: out.logical_x, y: out.logical_y }, (out.logical_w, out.logical_h)))
}
