//! A generated application: its layout, its content, and how it reacts to clicks.
//!
//! There are six archetypes ([`AppKind`]) so the layouts differ: a settings window is
//! a rail and a form, a file manager is a wide list under a busy toolbar, a mail
//! client is a list beside a reading pane, a store is a grid of cards. Within an
//! archetype the shape varies (sidebar side and width, toolbar edge, density, theme)
//! so the same control lands in different places from window to window, and the
//! content varies so reading the window is a search rather than a habit. The reactions
//! are the ordinary ones: a menu opens, an item closes it and may raise a dialog, a
//! tab switches its page, a search shows results. They are there so the eye has the
//! same reasons to move that it has in real work.

use rand::Rng;
use rand::seq::IndexedRandom;

use crate::probe::Label;
use crate::words;

/// Which of the two themes the scene draws in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeKind {
    Dark,
    Light,
}

/// Which side the sidebar sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// Top or bottom of the content area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
}

/// How tightly the content is packed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Density {
    Compact,
    Regular,
    Loose,
}

/// Which application the window is pretending to be.
///
/// The archetype decides the furniture: whether there is a sidebar and what is in it,
/// how many toolbar buttons, whether there are tabs, and what a page looks like. It is
/// what makes one window read differently from the next rather than the same window
/// with its parts shuffled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppKind {
    Settings,
    Files,
    Mail,
    Editor,
    Browser,
    Store,
}

/// Where coverage wants the furniture, as probabilities.
///
/// The application derives these from the label histogram so the emptiest half of the
/// screen gets the sidebar and the toolbar more often than the full one.
#[derive(Clone, Copy, Debug)]
pub struct Bias {
    /// Probability of putting the sidebar on the left.
    pub left : f64,
    /// Probability of putting the toolbar on top.
    pub top  : f64,
}

/// Identity of every labelled control. Stable within a scene.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WidgetId {
    Menu(u8),
    MenuItem(u8, u8),
    Nav(u8),
    Tool(u8),
    Tab(u8),
    Row(u8),
    RowCheck(u8),
    Field(u8),
    DropdownItem(u8, u8),
    Toggle(u8),
    Link(u8),
    /// A card in a grid page.
    Card(u8),
    /// The Install or Remove button on a card.
    CardAction(u8),
    Action(u8),
    DialogAction(u8),
    SearchInput,
    SearchButton,
    /// The application switcher, an index into [`AppKind::ALL`].
    Switch(u8),
    /// The button that generates a fresh window of a random archetype.
    Shuffle,
    /// The button that ends a posture prompt.
    Continue,
}

/// One menu in the bar.
#[derive(Clone, Debug)]
pub struct Menu {
    pub title : String,
    pub items : Vec<String>,
}

/// One row in a list page.
#[derive(Clone, Debug)]
pub struct Row {
    pub name    : String,
    pub detail  : String,
    pub checked : bool,
}

/// One card in a grid page.
#[derive(Clone, Debug)]
pub struct Card {
    pub name      : String,
    pub detail    : String,
    /// Whether the card's button says Remove rather than Install.
    pub installed : bool,
}

/// What a form field holds.
#[derive(Clone, Debug)]
pub enum FieldKind {
    Text     { value: String },
    Dropdown { options: Vec<String>, selected: usize },
    Toggle   { on: bool },
}

/// One form field.
#[derive(Clone, Debug)]
pub struct Field {
    pub label : String,
    pub kind  : FieldKind,
}

/// One paragraph of an article, with at most one link spliced in.
#[derive(Clone, Debug)]
pub struct Paragraph {
    pub before : String,
    /// The link's id and text.
    pub link   : Option<(u8, String)>,
    pub after  : String,
}

/// What a tab shows.
#[derive(Clone, Debug)]
pub enum Page {
    List    { rows: Vec<Row>, selected: Option<u8> },
    Form    { fields: Vec<Field> },
    Article { title: String, paragraphs: Vec<Paragraph> },
    Grid    { cards: Vec<Card> },
}

/// A modal card over the content.
#[derive(Clone, Debug)]
pub struct Dialog {
    pub title   : String,
    pub body    : String,
    pub actions : Vec<String>,
    /// Where the card sits in the content area, as fractions.
    pub anchor  : (f32, f32),
}

/// The whole generated application.
#[derive(Clone, Debug)]
pub struct Scene {
    pub theme         : ThemeKind,
    /// Which archetype the window is.
    pub kind          : AppKind,
    pub side          : Option<Side>,
    pub sidebar_w     : f32,
    pub toolbar_at    : Edge,
    pub density       : Density,
    pub menus         : Vec<Menu>,
    pub nav           : Vec<String>,
    pub active_nav    : u8,
    pub tools         : Vec<(&'static str, String)>,
    /// Tab titles. Empty when the archetype has no tabs, and then [`Scene::pages`]
    /// still holds exactly one page.
    pub tabs          : Vec<String>,
    pub active_tab    : u8,
    /// One page per tab, or one page when there are no tabs.
    pub pages         : Vec<Page>,
    /// Bottom-right buttons, secondary first, primary last. May be empty.
    pub actions       : Vec<String>,
    /// Whether a reading pane sits beside the page. Mail only.
    pub split         : bool,
    /// Width of the list pane when [`Scene::split`], logical pixels.
    pub pane_w        : f32,
    /// The article in the reading pane, or `None` for the placeholder.
    pub reading       : Option<Page>,
    pub dialog        : Option<Dialog>,
    pub open_menu     : Option<u8>,
    pub open_dropdown : Option<u8>,
    pub show_search   : bool,
    pub search        : String,
}

// --- Density ---

impl Density {
    /// Vertical padding inside a row, logical pixels.
    pub fn row_pad(self) -> f32 {
        match self {
            Self::Compact => 4.0,
            Self::Regular => 8.0,
            Self::Loose   => 14.0,
        }
    }

    /// Gap between stacked controls, logical pixels.
    pub fn gap(self) -> f32 {
        match self {
            Self::Compact => 4.0,
            Self::Regular => 8.0,
            Self::Loose   => 16.0,
        }
    }
}

// --- ThemeKind ---

impl ThemeKind {
    /// The value written on the wire.
    pub fn name(self) -> &'static str {
        match self {
            Self::Dark  => "dark",
            Self::Light => "light",
        }
    }
}

// --- AppKind ---

impl AppKind {
    /// Every archetype, in the order the switcher shows them.
    pub const ALL: [AppKind; 6] = [
        AppKind::Settings,
        AppKind::Files,
        AppKind::Mail,
        AppKind::Editor,
        AppKind::Browser,
        AppKind::Store,
    ];

    /// A random archetype.
    pub fn random(rng: &mut impl Rng) -> AppKind {
        *AppKind::ALL.choose(rng).unwrap()
    }

    /// The window title.
    pub fn name(self) -> &'static str {
        match self {
            Self::Settings => "Settings",
            Self::Files    => "Files",
            Self::Mail     => "Mail",
            Self::Editor   => "Editor",
            Self::Browser  => "Browser",
            Self::Store    => "Store",
        }
    }

    /// The symbolic icon the switcher shows for it.
    pub fn icon(self) -> &'static str {
        match self {
            Self::Settings => "preferences-system-symbolic",
            Self::Files    => "folder-symbolic",
            Self::Mail     => "mail-unread-symbolic",
            Self::Editor   => "accessories-text-editor-symbolic",
            Self::Browser  => "web-browser-symbolic",
            Self::Store    => "system-software-install-symbolic",
        }
    }
}

// --- Bias ---

impl Bias {
    /// No preference either way, for tests and for an empty histogram.
    pub fn balanced() -> Bias {
        Bias { left: 0.5, top: 0.5 }
    }
}

// --- Scene ---

impl Scene {
    /// A fresh window of archetype `kind`, with the furniture placed the way `bias`
    /// asks for.
    pub fn generate(kind: AppKind, bias: Bias, rng: &mut impl Rng) -> Scene {
        let theme   = if rng.random_bool(0.5) { ThemeKind::Dark } else { ThemeKind::Light };
        let density = *[Density::Compact, Density::Regular, Density::Loose].choose(rng).unwrap();

        // The sidebar is a fixture of some archetypes and an option in others; which
        // side it lands on is coverage's call.
        let sidebar_p = match kind {
            AppKind::Settings => 1.0,
            AppKind::Files    => 0.8,
            AppKind::Mail     => 1.0,
            AppKind::Editor   => 0.4,
            AppKind::Browser  => 0.0,
            AppKind::Store    => 0.6,
        };

        let side = {
            if !rng.random_bool(sidebar_p) {
                None
            }
            else if rng.random_bool(bias.left) {
                Some(Side::Left)
            }
            else {
                Some(Side::Right)
            }
        };

        let sidebar_w = match kind {
            AppKind::Settings => rng.random_range(220.0..=300.0),
            _                 => rng.random_range(180.0..=320.0),
        };

        // A browser's toolbar is its address bar row; it does not move to the bottom.
        let toolbar_at = {
            if kind == AppKind::Browser || rng.random_bool(bias.top) {
                Edge::Top
            }
            else {
                Edge::Bottom
            }
        };

        let menus = generate_menus(rng);
        let nav   = generate_nav(kind, rng);
        let tools = generate_tools(kind, rng);
        let tabs  = generate_tabs(kind, rng);
        let pages = (0..tabs.len().max(1)).map(|_| generate_page_for(kind, rng)).collect();

        let show_search = match kind {
            AppKind::Settings => rng.random_bool(0.5),
            AppKind::Files    => rng.random_bool(0.8),
            AppKind::Mail     => rng.random_bool(0.9),
            AppKind::Editor   => rng.random_bool(0.4),
            AppKind::Browser  => true,
            AppKind::Store    => true,
        };

        // Only a settings window commits anything, so only it has an action row.
        let actions = match kind {
            AppKind::Settings => vec![
                words::SECONDARY.choose(rng).unwrap().to_string(),
                words::PRIMARY.choose(rng).unwrap().to_string(),
            ],
            _                 => Vec::new(),
        };

        Scene {
            theme         : theme,
            kind          : kind,
            side          : side,
            sidebar_w     : sidebar_w,
            toolbar_at    : toolbar_at,
            density       : density,
            menus         : menus,
            nav           : nav,
            active_nav    : 0,
            tools         : tools,
            tabs          : tabs,
            active_tab    : 0,
            pages         : pages,
            actions       : actions,
            split         : kind == AppKind::Mail,
            pane_w        : rng.random_range(380.0..=460.0),
            reading       : None,
            dialog        : None,
            open_menu     : None,
            open_dropdown : None,
            show_search   : show_search,
            search        : String::new(),
        }
    }

    /// The page under the active tab.
    pub fn page(&self) -> &Page {
        &self.pages[usize::from(self.active_tab)]
    }

    /// The page under the active tab, mutably.
    pub fn page_mut(&mut self) -> &mut Page {
        &mut self.pages[usize::from(self.active_tab)]
    }

    /// Index of the primary action (the last one), or `None` when the window has no
    /// action row.
    pub fn primary_action(&self) -> Option<u8> {
        self.actions.len().checked_sub(1).map(|i| i as u8)
    }

    /// The label a control carries, or `None` for an id the scene does not have.
    pub fn label(&self, id: WidgetId) -> Option<Label> {
        let (kind, text): (&'static str, Option<String>) = match id {
            WidgetId::Menu(m)         => ("button", Some(self.menus.get(usize::from(m))?.title.clone())),
            WidgetId::MenuItem(m, i)  => {
                let menu = self.menus.get(usize::from(m))?;

                ("button", Some(menu.items.get(usize::from(i))?.clone()))
            }
            WidgetId::Nav(i)          => ("button", Some(self.nav.get(usize::from(i))?.clone())),
            WidgetId::Tool(i)         => ("icon", Some(self.tools.get(usize::from(i))?.1.clone())),
            WidgetId::Tab(i)          => ("button", Some(self.tabs.get(usize::from(i))?.clone())),
            WidgetId::Row(i)          => ("text", Some(self.row(i)?.name.clone())),
            WidgetId::RowCheck(i)     => ("checkbox", Some(self.row(i)?.name.clone())),
            WidgetId::Field(i)        => {
                let field = self.field(i)?;

                let kind = match field.kind {
                    FieldKind::Text { .. }     => "input",
                    FieldKind::Dropdown { .. } => "button",
                    FieldKind::Toggle { .. }   => "checkbox",
                };

                (kind, Some(field.label.clone()))
            }
            WidgetId::DropdownItem(f, j) => {
                let FieldKind::Dropdown { options, .. } = &self.field(f)?.kind else {
                    return None;
                };

                ("button", Some(options.get(usize::from(j))?.clone()))
            }
            WidgetId::Toggle(i)       => ("checkbox", Some(self.field(i)?.label.clone())),
            WidgetId::Link(i)         => ("link", Some(self.link_text(i)?)),
            WidgetId::Card(i)         => ("button", Some(self.card(i)?.name.clone())),
            WidgetId::CardAction(i)   => ("button", Some(card_verb(self.card(i)?).to_string())),
            WidgetId::Action(i)       => ("button", Some(self.actions.get(usize::from(i))?.clone())),
            WidgetId::DialogAction(i) => {
                ("button", Some(self.dialog.as_ref()?.actions.get(usize::from(i))?.clone()))
            }
            WidgetId::SearchInput     => ("input", Some("Search".to_string())),
            WidgetId::SearchButton    => ("button", Some("Search".to_string())),
            WidgetId::Switch(k)       => {
                ("icon", Some(AppKind::ALL.get(usize::from(k))?.name().to_string()))
            }
            WidgetId::Shuffle         => ("icon", Some("New window".to_string())),
            WidgetId::Continue        => ("button", Some("Continue".to_string())),
        };

        Some(Label { id: id, kind: kind, text: text })
    }

    /// Row `i` of the active page, when it is a list.
    pub fn row(&self, i: u8) -> Option<&Row> {
        match self.page() {
            Page::List { rows, .. } => rows.get(usize::from(i)),
            _                       => None,
        }
    }

    /// Field `i` of the active page, when it is a form.
    pub fn field(&self, i: u8) -> Option<&Field> {
        match self.page() {
            Page::Form { fields } => fields.get(usize::from(i)),
            _                     => None,
        }
    }

    /// Card `i` of the active page, when it is a grid.
    pub fn card(&self, i: u8) -> Option<&Card> {
        match self.page() {
            Page::Grid { cards } => cards.get(usize::from(i)),
            _                    => None,
        }
    }

    /// The article the links belong to: the reading pane when the window is split,
    /// the page itself otherwise.
    pub fn article(&self) -> Option<&Page> {
        match self.split {
            true  => self.reading.as_ref(),
            false => Some(self.page()),
        }
    }

    /// The text of link `i` in the article on screen.
    pub fn link_text(&self, i: u8) -> Option<String> {
        match self.article()? {
            Page::Article { paragraphs, .. } => paragraphs.iter()
                .filter_map(|p| p.link.as_ref())
                .find(|(id, _)| *id == i)
                .map(|(_, text)| text.clone()),
            _                                => None,
        }
    }

    /// Reacts to a click on `id` the way the application would.
    ///
    /// [`WidgetId::Switch`] and [`WidgetId::Shuffle`] belong to the application, not to
    /// the scene, so they are ignored here.
    pub fn apply(&mut self, id: WidgetId, rng: &mut impl Rng) {
        match id {
            WidgetId::Menu(m)          => {
                self.open_dropdown = None;
                self.open_menu     = if self.open_menu == Some(m) { None } else { Some(m) };
            }

            WidgetId::MenuItem(m, i)   => {
                self.open_menu = None;

                if let Some(item) = self.menus.get(usize::from(m))
                    .and_then(|menu| menu.items.get(usize::from(i)))
                    .cloned()
                    && item.ends_with('…')
                {
                    self.dialog = Some(dialog_for(item.trim_end_matches('…'), rng));
                }
            }

            WidgetId::Nav(i)           => {
                self.active_nav = i;
                self.active_tab = 0;
                self.reading    = None;
                self.pages      = (0..self.tabs.len().max(1))
                    .map(|_| generate_page_for(self.kind, rng))
                    .collect();
            }

            WidgetId::Tool(i)          => {
                if let Some((_, label)) = self.tools.get(usize::from(i)).cloned() {
                    self.dialog = Some(dialog_for(&label, rng));
                }
            }

            WidgetId::Tab(i)           => {
                self.active_tab    = i;
                self.open_dropdown = None;
            }

            WidgetId::Row(i)           => {
                let split = self.split;

                if let Page::List { selected, .. } = self.page_mut() {
                    *selected = Some(i);
                }

                // A mail list opens its message beside itself rather than in place.
                if split {
                    self.reading = Some(generate_article(rng));
                }
            }

            WidgetId::RowCheck(i)      => {
                if let Page::List { rows, .. } = self.page_mut()
                    && let Some(row) = rows.get_mut(usize::from(i))
                {
                    row.checked = !row.checked;
                }
            }

            WidgetId::Field(i)         => {
                let is_dropdown = matches!(self.field(i).map(|f| &f.kind),
                                           Some(FieldKind::Dropdown { .. }));

                if is_dropdown {
                    self.open_dropdown = if self.open_dropdown == Some(i) { None } else { Some(i) };
                }
            }

            WidgetId::DropdownItem(f, j) => {
                self.open_dropdown = None;

                if let Page::Form { fields } = self.page_mut()
                    && let Some(Field { kind: FieldKind::Dropdown { selected, .. }, .. }) =
                        fields.get_mut(usize::from(f))
                {
                    *selected = usize::from(j);
                }
            }

            WidgetId::Toggle(i)        => {
                if let Page::Form { fields } = self.page_mut()
                    && let Some(Field { kind: FieldKind::Toggle { on }, .. }) =
                        fields.get_mut(usize::from(i))
                {
                    *on = !*on;
                }
            }

            WidgetId::Link(_)          => {
                match self.split {
                    true  => self.reading = Some(generate_article(rng)),
                    false => *self.page_mut() = generate_article(rng),
                }
            }

            WidgetId::Card(i)          => {
                // The card is read before the dialog is built so the scene is not
                // borrowed while it is assigned.
                let card = self.card(i).map(|c| (c.name.clone(), card_verb(c)));

                if let Some((name, verb)) = card {
                    self.dialog = Some(Dialog {
                        title   : name.clone(),
                        body    : format!("{verb} {name}? It is about 40 MB."),
                        actions : vec!["Cancel".into(), verb.to_string()],
                        anchor  : random_anchor(rng),
                    });
                }
            }

            WidgetId::CardAction(i)    => {
                if let Page::Grid { cards } = self.page_mut()
                    && let Some(card) = cards.get_mut(usize::from(i))
                {
                    card.installed = !card.installed;
                }
            }

            WidgetId::Action(_)        => {
                if let Page::List { rows, selected } = self.page_mut() {
                    *selected = None;

                    for row in rows.iter_mut() {
                        row.checked = false;
                    }
                }
            }

            WidgetId::DialogAction(_)  => {
                self.dialog = None;
            }

            WidgetId::SearchInput      => {}

            WidgetId::SearchButton     => {
                let query = self.search.trim().to_lowercase();

                let n_results = rng.random_range(3..=8);

                let mut rows: Vec<Row> = words::names(rng, n_results)
                    .into_iter()
                    .map(|name| Row {
                        name    : format!("{name} — {query}"),
                        detail  : words::DETAILS.choose(rng).unwrap().to_string(),
                        checked : false,
                    })
                    .collect();

                rows.truncate(u8::MAX as usize);

                *self.page_mut() = Page::List { rows: rows, selected: None };
                self.search      = String::new();
            }

            WidgetId::Switch(_)        => {}

            WidgetId::Shuffle          => {}

            WidgetId::Continue         => {}
        }
    }

    /// Every labelled control the scene currently puts on screen.
    ///
    /// The application uses it to check that nothing it draws is unlabelled; the test
    /// below uses it to check that every id resolves.
    pub fn controls(&self) -> Vec<WidgetId> {
        let mut ids: Vec<WidgetId> = Vec::new();

        ids.extend((0..self.menus.len()).map(|m| WidgetId::Menu(m as u8)));

        // Only the open menu's items exist.
        if let Some(m) = self.open_menu
            && let Some(menu) = self.menus.get(usize::from(m))
        {
            ids.extend((0..menu.items.len()).map(|i| WidgetId::MenuItem(m, i as u8)));
        }

        if self.side.is_some() {
            ids.extend((0..self.nav.len()).map(|n| WidgetId::Nav(n as u8)));
        }

        ids.extend((0..self.tools.len()).map(|t| WidgetId::Tool(t as u8)));
        ids.extend((0..self.tabs.len()).map(|t| WidgetId::Tab(t as u8)));

        match self.page() {
            Page::List { rows, .. }          => {
                for i in 0..rows.len() as u8 {
                    ids.push(WidgetId::Row(i));
                    ids.push(WidgetId::RowCheck(i));
                }
            }
            Page::Form { fields }            => {
                for (i, field) in fields.iter().enumerate() {
                    match field.kind {
                        FieldKind::Toggle { .. } => ids.push(WidgetId::Toggle(i as u8)),
                        _                        => ids.push(WidgetId::Field(i as u8)),
                    }
                }

                // Only the open dropdown's options exist.
                if let Some(f) = self.open_dropdown
                    && let Some(Field { kind: FieldKind::Dropdown { options, .. }, .. }) =
                        fields.get(usize::from(f))
                {
                    ids.extend((0..options.len()).map(|j| WidgetId::DropdownItem(f, j as u8)));
                }
            }
            Page::Article { .. }             => {}
            Page::Grid { cards }             => {
                for i in 0..cards.len() as u8 {
                    ids.push(WidgetId::Card(i));
                    ids.push(WidgetId::CardAction(i));
                }
            }
        }

        // Links come from whichever article is on screen: the page, or the reading
        // pane when the window is split.
        if let Some(Page::Article { paragraphs, .. }) = self.article() {
            ids.extend(paragraphs.iter().filter_map(|p| p.link.as_ref()).map(|(id, _)| WidgetId::Link(*id)));
        }

        ids.extend((0..self.actions.len()).map(|a| WidgetId::Action(a as u8)));

        if let Some(dialog) = &self.dialog {
            ids.extend((0..dialog.actions.len()).map(|k| WidgetId::DialogAction(k as u8)));
        }

        if self.show_search {
            ids.push(WidgetId::SearchInput);
            ids.push(WidgetId::SearchButton);
        }

        ids.extend((0..AppKind::ALL.len()).map(|k| WidgetId::Switch(k as u8)));
        ids.push(WidgetId::Shuffle);

        ids
    }
}

// --- Generation ---

/// A page of the sort archetype `kind` shows.
pub fn generate_page_for(kind: AppKind, rng: &mut impl Rng) -> Page {
    match kind {
        AppKind::Settings => {
            match rng.random_bool(0.7) {
                true  => generate_form(rng),
                false => generate_list(rng, 6, 20),
            }
        }
        AppKind::Files    => generate_list(rng, 12, 28),
        AppKind::Mail     => generate_messages(rng),
        AppKind::Editor   => generate_article(rng),
        AppKind::Browser  => article(rng, 3, 5),
        AppKind::Store    => generate_grid(rng),
    }
}

/// Three to six menus of four to nine items.
fn generate_menus(rng: &mut impl Rng) -> Vec<Menu> {
    let n_menus = rng.random_range(3..=6);

    words::pick_n(rng, words::MENUS, n_menus)
        .into_iter()
        .map(|title| {
            let n_items = rng.random_range(4..=9);

            Menu {
                title : title.to_string(),
                items : words::pick_n(rng, words::MENU_ITEMS, n_items)
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            }
        })
        .collect()
}

/// The sidebar entries archetype `kind` would have.
fn generate_nav(kind: AppKind, rng: &mut impl Rng) -> Vec<String> {
    match kind {
        AppKind::Settings => {
            let n = rng.random_range(6..=13);

            strings(words::pick_n(rng, words::SETTINGS, n))
        }

        AppKind::Files    => {
            // A file manager's places, then a couple of the user's own bookmarks.
            let n = rng.random_range(6..=10);

            let mut nav = strings(words::pick_n(rng, words::PLACES, n));
            nav.extend(words::names(rng, 2));

            nav
        }

        AppKind::Mail     => {
            // The standard folders, then labels the user made up.
            let n        = rng.random_range(5..=7);
            let n_labels = rng.random_range(1..=3);

            let mut nav = strings(words::pick_n(rng, words::FOLDERS, n));
            nav.extend(words::names(rng, n_labels));

            nav
        }

        AppKind::Editor   => {
            let n = rng.random_range(5..=12);

            words::file_names(rng, n)
        }

        AppKind::Browser  => Vec::new(),

        AppKind::Store    => {
            let n = rng.random_range(5..=11);

            strings(words::pick_n(rng, words::CATEGORIES, n))
        }
    }
}

/// The toolbar archetype `kind` would have.
fn generate_tools(kind: AppKind, rng: &mut impl Rng) -> Vec<(&'static str, String)> {
    // A browser's toolbar is fixed: it is the navigation row, not a set of actions.
    if kind == AppKind::Browser {
        let mut tools = vec![tool("Back"), tool("Forward"), tool("Refresh")];

        if rng.random_bool(0.5) {
            tools.push(tool("Bookmark"));
        }

        return tools;
    }

    let n = match kind {
        AppKind::Settings => rng.random_range(0..=2),
        AppKind::Files    => rng.random_range(5..=8),
        AppKind::Mail     => rng.random_range(4..=6),
        AppKind::Editor   => rng.random_range(4..=7),
        AppKind::Store    => rng.random_range(2..=3),
        AppKind::Browser  => 0,
    };

    let labels: Vec<&str> = words::TOOLS.iter().map(|(_, l)| *l).collect();

    words::pick_n(rng, &labels, n).into_iter().map(tool).collect()
}

/// The tab titles archetype `kind` would have. Empty for the single-page archetypes.
fn generate_tabs(kind: AppKind, rng: &mut impl Rng) -> Vec<String> {
    match kind {
        AppKind::Files    => Vec::new(),
        AppKind::Mail     => Vec::new(),

        AppKind::Settings => {
            let n = rng.random_range(1..=3);

            strings(words::pick_n(rng, words::TABS, n))
        }

        AppKind::Editor   => {
            let n = rng.random_range(3..=6);

            words::file_names(rng, n)
        }

        AppKind::Browser  => {
            let n = rng.random_range(3..=6);

            words::names(rng, n)
        }

        AppKind::Store    => {
            let n = rng.random_range(2..=4);

            strings(words::pick_n(rng, words::CATEGORIES, n))
        }
    }
}

/// A list page with `min` to `max` rows.
fn generate_list(rng: &mut impl Rng, min: usize, max: usize) -> Page {
    let n = rng.random_range(min..=max);

    let rows = words::names(rng, n)
        .into_iter()
        .map(|name| Row {
            name    : name,
            detail  : words::DETAILS.choose(rng).unwrap().to_string(),
            checked : rng.random_bool(0.1),
        })
        .collect();

    Page::List { rows: rows, selected: None }
}

/// A mailbox: subjects on the left, a sender or a time on the right.
fn generate_messages(rng: &mut impl Rng) -> Page {
    let n = rng.random_range(10..=30);

    let rows = (0..n)
        .map(|_| {
            let name = {
                match rng.random_bool(0.7) {
                    true  => words::SUBJECTS.choose(rng).unwrap().to_string(),
                    false => words::name(rng),
                }
            };

            let detail = {
                match rng.random_bool(0.6) {
                    true  => words::SENDERS.choose(rng).unwrap().to_string(),
                    false => words::TIMES.choose(rng).unwrap().to_string(),
                }
            };

            Row {
                name    : name,
                detail  : detail,
                checked : rng.random_bool(0.1),
            }
        })
        .collect();

    Page::List { rows: rows, selected: None }
}

/// A grid of 8 to 20 store cards.
fn generate_grid(rng: &mut impl Rng) -> Page {
    let n = rng.random_range(8..=20);

    let cards = words::names(rng, n)
        .into_iter()
        .map(|name| Card {
            name      : name,
            detail    : words::CARD_DETAILS.choose(rng).unwrap().to_string(),
            installed : rng.random_bool(0.25),
        })
        .collect();

    Page::Grid { cards: cards }
}

/// A form page with 4 to 9 fields of mixed kinds.
fn generate_form(rng: &mut impl Rng) -> Page {
    let n = rng.random_range(4..=9);

    let texts     = words::pick_n(rng, words::TEXT_FIELDS, n);
    let toggles   = words::pick_n(rng, words::TOGGLES, n);
    let dropdowns = words::pick_n(rng,
        words::DROPDOWNS.iter().map(|(l, _)| *l).collect::<Vec<_>>().as_slice(), n);

    let mut fields = Vec::with_capacity(n);
    let (mut ti, mut gi, mut di) = (0, 0, 0);

    for _ in 0..n {
        let field = match rng.random_range(0..3) {
            0 if ti < texts.len()     => {
                ti += 1;

                Field {
                    label : texts[ti - 1].to_string(),
                    kind  : FieldKind::Text { value: String::new() },
                }
            }
            1 if di < dropdowns.len() => {
                di += 1;

                let (label, options) = words::DROPDOWNS.iter()
                    .find(|(l, _)| *l == dropdowns[di - 1])
                    .unwrap();

                Field {
                    label : label.to_string(),
                    kind  : FieldKind::Dropdown {
                        options  : options.iter().map(|o| o.to_string()).collect(),
                        selected : rng.random_range(0..options.len()),
                    },
                }
            }
            _ if gi < toggles.len()   => {
                gi += 1;

                Field {
                    label : toggles[gi - 1].to_string(),
                    kind  : FieldKind::Toggle { on: rng.random_bool(0.5) },
                }
            }
            _                         => continue,
        };

        fields.push(field);
    }

    if fields.is_empty() {
        fields.push(Field {
            label : "Name".into(),
            kind  : FieldKind::Text { value: String::new() },
        });
    }

    Page::Form { fields: fields }
}

/// An article page with 4 to 8 paragraphs and 2 to 4 links.
pub fn generate_article(rng: &mut impl Rng) -> Page {
    article(rng, 2, 4)
}

/// An article page with `min` to `max` links spliced into its paragraphs.
fn article(rng: &mut impl Rng, min: usize, max: usize) -> Page {
    let n     = rng.random_range(4..=8);
    let links = rng.random_range(min..=max.min(n));

    let sentences = words::pick_n(rng, words::SENTENCES, n);

    let mut with_link: Vec<usize> = Vec::new();

    while with_link.len() < links {
        let i = rng.random_range(0..n);

        if !with_link.contains(&i) {
            with_link.push(i);
        }
    }

    let link_texts = words::pick_n(rng, words::LINKS, links);
    let mut next_link = 0u8;

    let paragraphs = sentences.iter()
        .enumerate()
        .map(|(i, sentence)| {
            let second = words::SENTENCES.choose(rng).unwrap();

            if with_link.contains(&i) {
                let text = link_texts[usize::from(next_link) % link_texts.len()].to_string();
                let id   = next_link;

                next_link += 1;

                Paragraph {
                    before : format!("{sentence} See"),
                    link   : Some((id, text)),
                    after  : format!("for details. {second}"),
                }
            }
            else {
                Paragraph {
                    before : format!("{sentence} {second}"),
                    link   : None,
                    after  : String::new(),
                }
            }
        })
        .collect();

    Page::Article {
        title      : words::name(rng),
        paragraphs : paragraphs,
    }
}

/// The dialog a menu item or tool named `what` would raise.
fn dialog_for(what: &str, rng: &mut impl Rng) -> Dialog {
    Dialog {
        title   : what.to_string(),
        body    : format!("{what} the current item? You can change this later."),
        actions : vec!["Cancel".into(), what.split_whitespace().next().unwrap_or("OK").to_string()],
        anchor  : random_anchor(rng),
    }
}

/// One of nine dialog positions, as fractions of the content area.
fn random_anchor(rng: &mut impl Rng) -> (f32, f32) {
    let fx = *[0.1, 0.5, 0.9].choose(rng).unwrap();
    let fy = *[0.15, 0.5, 0.85].choose(rng).unwrap();

    (fx, fy)
}

/// What a card's button offers, given what the card already is.
fn card_verb(card: &Card) -> &'static str {
    match card.installed {
        true  => "Remove",
        false => "Install",
    }
}

/// The toolbar entry for a label in [`words::TOOLS`].
fn tool(label: &str) -> (&'static str, String) {
    let icon = words::TOOLS.iter()
        .find(|(_, l)| *l == label)
        .map(|(i, _)| *i)
        .unwrap_or("application-x-executable-symbolic");

    (icon, label.to_string())
}

/// Owned copies of a set of picked words.
fn strings(picked: Vec<&str>) -> Vec<String> {
    picked.into_iter().map(str::to_string).collect()
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_control_has_a_label() {
        let mut rng = rand::rng();

        for kind in AppKind::ALL {
            for _ in 0..50 {
                let scene = Scene::generate(kind, Bias::balanced(), &mut rng);

                for id in scene.controls() {
                    assert!(scene.label(id).is_some(), "{kind:?}: {id:?} has no label");
                }
            }
        }
    }

    #[test]
    fn menus_open_and_close_and_ellipsis_items_raise_a_dialog() {
        let mut rng   = rand::rng();
        let mut scene = Scene::generate(AppKind::Settings, Bias::balanced(), &mut rng);

        scene.apply(WidgetId::Menu(0), &mut rng);
        assert_eq!(scene.open_menu, Some(0));

        scene.apply(WidgetId::Menu(0), &mut rng);
        assert_eq!(scene.open_menu, None);

        scene.menus[0].items[0] = "Open…".into();
        scene.apply(WidgetId::Menu(0), &mut rng);
        scene.apply(WidgetId::MenuItem(0, 0), &mut rng);

        assert_eq!(scene.open_menu, None);
        assert!(scene.dialog.is_some());

        scene.apply(WidgetId::DialogAction(0), &mut rng);
        assert!(scene.dialog.is_none());
    }

    #[test]
    fn a_search_replaces_the_page_with_results_that_carry_the_query() {
        let mut rng   = rand::rng();
        let mut scene = Scene::generate(AppKind::Files, Bias::balanced(), &mut rng);

        scene.search = "cedar".into();
        scene.apply(WidgetId::SearchButton, &mut rng);

        let Page::List { rows, .. } = scene.page() else {
            panic!("results are a list");
        };

        assert!(rows.iter().all(|r| r.name.contains("cedar")));
    }

    #[test]
    fn each_archetype_keeps_its_own_shape() {
        let mut rng = rand::rng();

        for _ in 0..50 {
            let settings = Scene::generate(AppKind::Settings, Bias::balanced(), &mut rng);
            assert!(settings.side.is_some());
            assert_eq!(settings.actions.len(), 2);
            assert_eq!(settings.primary_action(), Some(1));

            let mail = Scene::generate(AppKind::Mail, Bias::balanced(), &mut rng);
            assert!(mail.split);
            assert!(mail.tabs.is_empty());
            assert_eq!(mail.pages.len(), 1);

            let browser = Scene::generate(AppKind::Browser, Bias::balanced(), &mut rng);
            assert!(browser.side.is_none());
            assert_eq!(browser.toolbar_at, Edge::Top);
            assert!(browser.show_search);
            assert!(browser.actions.is_empty());

            let store = Scene::generate(AppKind::Store, Bias::balanced(), &mut rng);
            assert!(matches!(store.page(), Page::Grid { .. }));
        }
    }

    #[test]
    fn a_mail_row_opens_in_the_reading_pane_and_a_link_replaces_it() {
        let mut rng   = rand::rng();
        let mut scene = Scene::generate(AppKind::Mail, Bias::balanced(), &mut rng);

        assert!(scene.reading.is_none());

        scene.apply(WidgetId::Row(0), &mut rng);

        assert!(matches!(scene.reading, Some(Page::Article { .. })),
                "selecting a message shows an article");

        // The link's text comes from the reading pane, not from the list.
        let link = scene.controls().into_iter()
            .find_map(|id| match id {
                WidgetId::Link(i) => Some(i),
                _                 => None,
            })
            .expect("the article has a link");

        assert!(scene.link_text(link).is_some());

        scene.apply(WidgetId::Link(link), &mut rng);

        let Some(Page::Article { .. }) = scene.reading else {
            panic!("a link replaces the reading pane");
        };

        assert!(matches!(scene.page(), Page::List { .. }), "the list pane is untouched");
    }

    #[test]
    fn a_card_raises_a_dialog_and_its_button_toggles_the_install() {
        let mut rng   = rand::rng();
        let mut scene = Scene::generate(AppKind::Store, Bias::balanced(), &mut rng);

        let before = scene.card(0).expect("the grid has cards").installed;

        scene.apply(WidgetId::Card(0), &mut rng);

        let dialog = scene.dialog.clone().expect("a card raises a dialog");
        assert_eq!(dialog.title, scene.card(0).unwrap().name);
        assert_eq!(dialog.actions.len(), 2);

        scene.apply(WidgetId::DialogAction(0), &mut rng);
        scene.apply(WidgetId::CardAction(0), &mut rng);

        assert_eq!(scene.card(0).unwrap().installed, !before);
    }
}
