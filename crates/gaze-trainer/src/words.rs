//! Vocabulary for generated content: names that read as an application's, so finding
//! "the row called Harbour Ledger" is a real search and not a scan for a marker.

use rand::Rng;
use rand::seq::IndexedRandom;

/// Menu titles, in the order applications put them.
pub const MENUS: &[&str] = &["File", "Edit", "View", "Insert", "Format", "Tools", "Help"];

/// Menu items. Those ending in an ellipsis open a dialog, as they would anywhere.
pub const MENU_ITEMS: &[&str] = &[
    "New", "Open…", "Save", "Save As…", "Export…", "Print…", "Close",
    "Undo", "Redo", "Cut", "Copy", "Paste", "Select All", "Find…", "Replace…",
    "Zoom In", "Zoom Out", "Full Screen", "Sidebar", "Status Bar",
    "Preferences…", "Keyboard Shortcuts…", "About", "Check for Updates…",
    "Import…", "Share…", "Rename…", "Duplicate", "Move to Trash",
];

/// Sidebar entries for the settings archetype.
pub const SETTINGS: &[&str] = &[
    "Wi-Fi", "Bluetooth", "Displays", "Sound", "Power", "Appearance", "Keyboard",
    "Mouse", "Notifications", "Accounts", "Date & Time", "Privacy", "About",
];

/// Sidebar entries for the file manager archetype, before the generated bookmarks.
pub const PLACES: &[&str] = &[
    "Home", "Documents", "Downloads", "Pictures", "Music", "Videos", "Desktop",
    "Trash", "Recent", "Starred",
];

/// Sidebar entries for the mail archetype, before the generated labels.
pub const FOLDERS: &[&str] = &[
    "Inbox", "Drafts", "Sent", "Archive", "Starred", "Spam", "Trash",
];

/// Sidebar entries and tab titles for the store archetype.
pub const CATEGORIES: &[&str] = &[
    "Featured", "Productivity", "Development", "Games", "Graphics", "Audio", "Video",
    "Utilities", "Education", "Installed", "Updates",
];

/// Message subjects for the mail archetype.
pub const SUBJECTS: &[&str] = &[
    "Re: Thursday's numbers", "Invoice for August", "Access request", "Draft for review",
    "Notes from the standup", "Your order has shipped", "Renewal reminder",
    "Re: retry policy", "Photos from the weekend", "Quarterly summary attached",
    "Meeting moved to 15:00", "Security alert: new sign-in", "Welcome aboard",
    "Follow-up on the migration", "Re: naming", "Expenses need approval",
];

/// Message senders for the mail archetype.
pub const SENDERS: &[&str] = &[
    "Anna Voss", "Priya Nair", "Tomás Ruiz", "Ida Lindqvist", "Marcus Ofori",
    "Hana Sato", "Devon Clarke", "Noor Haddad", "billing@example", "no-reply@example",
];

/// Times a mail client puts in a message's second column.
pub const TIMES: &[&str] = &[
    "09:14", "11:02", "13:47", "16:20", "Yesterday", "Mon", "Tue", "Wed", "3 Aug",
    "27 Jul",
];

/// File extensions for generated file names.
pub const EXTENSIONS: &[&str] = &["rs", "md", "toml", "txt", "py"];

/// What a store card says under its name.
pub const CARD_DETAILS: &[&str] = &[
    "4.6 ★ · 1.2k ratings", "Free", "Editor's pick", "Updated last week", "12 MB",
    "Verified publisher", "New in this category", "4.1 ★ · 340 ratings", "Trial",
    "Works offline",
];

/// Toolbar buttons: an icon name from the Adwaita/Pop set and a label.
pub const TOOLS: &[(&str, &str)] = &[
    ("go-previous-symbolic", "Back"),
    ("go-next-symbolic", "Forward"),
    ("view-refresh-symbolic", "Refresh"),
    ("document-new-symbolic", "New"),
    ("folder-new-symbolic", "New Folder"),
    ("edit-find-symbolic", "Find"),
    ("edit-copy-symbolic", "Copy"),
    ("edit-paste-symbolic", "Paste"),
    ("edit-delete-symbolic", "Delete"),
    ("mail-send-symbolic", "Send"),
    ("emblem-shared-symbolic", "Share"),
    ("view-list-symbolic", "List"),
    ("view-grid-symbolic", "Grid"),
    ("preferences-system-symbolic", "Settings"),
    ("bookmark-new-symbolic", "Bookmark"),
];

/// Tab titles.
pub const TABS: &[&str] = &[
    "General", "Details", "History", "Comments", "Files", "Activity", "Members",
    "Permissions", "Advanced", "Summary", "Changes", "Checks",
];

/// First halves of generated names.
pub const ADJECTIVES: &[&str] = &[
    "Harbour", "Quiet", "Copper", "Winter", "Marble", "Lantern", "Cedar", "Amber",
    "Granite", "Velvet", "Meadow", "Slate", "Ember", "Willow", "Saffron", "Cobalt",
    "Fennel", "Juniper", "Orchard", "Pewter", "Raven", "Tidal", "Umber", "Zephyr",
];

/// Second halves of generated names.
pub const NOUNS: &[&str] = &[
    "Ledger", "Report", "Draft", "Invoice", "Sketch", "Archive", "Notebook", "Manifest",
    "Schedule", "Outline", "Contract", "Summary", "Proposal", "Roster", "Budget", "Recipe",
    "Journal", "Transcript", "Catalogue", "Brief", "Playlist", "Itinerary", "Survey", "Log",
];

/// Row details: what a file manager or a mail client puts in the second column.
pub const DETAILS: &[&str] = &[
    "Modified yesterday", "2.4 MB", "Shared with 3 people", "Edited 12 min ago",
    "14 items", "Draft", "Awaiting review", "Last opened Tuesday", "Read-only",
    "Synced", "1.1 GB", "3 comments", "Version 4", "Archived", "Pinned",
];

/// Form field labels for text inputs.
pub const TEXT_FIELDS: &[&str] = &[
    "Name", "Title", "Email", "Location", "Description", "Tag", "Owner", "Nickname",
    "Server", "Username", "Project", "Label",
];

/// Form field labels for dropdowns, with their options.
pub const DROPDOWNS: &[(&str, &[&str])] = &[
    ("Language", &["English", "Deutsch", "Français", "Español", "日本語"]),
    ("Sort by", &["Name", "Date", "Size", "Type", "Owner"]),
    ("Theme", &["System", "Light", "Dark", "High Contrast"]),
    ("Priority", &["Low", "Normal", "High", "Urgent"]),
    ("Visibility", &["Private", "Team", "Public"]),
    ("Format", &["PDF", "PNG", "SVG", "Markdown", "Plain Text"]),
    ("Frequency", &["Never", "Daily", "Weekly", "Monthly"]),
];

/// Form field labels for toggles.
pub const TOGGLES: &[&str] = &[
    "Notifications", "Auto-save", "Show hidden files", "Sync on Wi-Fi only",
    "Dark mode", "Spell check", "Two-factor authentication", "Compact rows",
    "Open links in new tab", "Remember window size",
];

/// Primary action button labels.
pub const PRIMARY: &[&str] = &["Save", "Apply", "Open", "Send", "Create", "Done", "Continue"];

/// Secondary action button labels.
pub const SECONDARY: &[&str] = &["Cancel", "Discard", "Back", "Reset", "Close"];

/// Article sentences. A link is spliced into some of them.
pub const SENTENCES: &[&str] = &[
    "The migration finished ahead of schedule, with the last batch verified this morning.",
    "Most of the remaining work is documentation, which the team has agreed to split.",
    "Latency on the new path is down by a third, though the tail still needs attention.",
    "The review raised two questions about the retry policy and one about naming.",
    "Nothing in the profile changed after the cache was moved to the second tier.",
    "A short summary of the decision is attached, with the alternatives that were rejected.",
    "The panel meets again on Thursday to look at the revised figures.",
    "Anyone who has not yet confirmed access should do so before the freeze.",
    "There is a known issue with exports over ten thousand rows, tracked separately.",
    "The draft is open for comments until the end of the week.",
];

/// Link texts spliced into sentences.
pub const LINKS: &[&str] = &[
    "the full report", "release notes", "this thread", "the design document", "the issue",
    "last week's minutes", "the dashboard", "the changelog", "the schedule", "the checklist",
];

/// A generated proper name, "Adjective Noun".
pub fn name(rng: &mut impl Rng) -> String {
    format!("{} {}", ADJECTIVES.choose(rng).unwrap(), NOUNS.choose(rng).unwrap())
}

/// `n` distinct generated names.
pub fn names(rng: &mut impl Rng, n: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(n);

    while out.len() < n {
        let candidate = name(rng);

        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }

    out
}

/// A generated file name, `adjective_noun.ext`, the way a source tree reads.
pub fn file_name(rng: &mut impl Rng) -> String {
    format!(
        "{}_{}.{}",
        ADJECTIVES.choose(rng).unwrap().to_lowercase(),
        NOUNS.choose(rng).unwrap().to_lowercase(),
        EXTENSIONS.choose(rng).unwrap(),
    )
}

/// `n` distinct generated file names.
pub fn file_names(rng: &mut impl Rng, n: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(n);

    while out.len() < n {
        let candidate = file_name(rng);

        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }

    out
}

/// `n` distinct picks from a static list, in list order.
pub fn pick_n<'a>(rng: &mut impl Rng, list: &[&'a str], n: usize) -> Vec<&'a str> {
    let mut picked: Vec<usize> = Vec::new();

    while picked.len() < n.min(list.len()) {
        let i = rng.random_range(0..list.len());

        if !picked.contains(&i) {
            picked.push(i);
        }
    }

    picked.sort_unstable();

    picked.into_iter().map(|i| list[i]).collect()
}
