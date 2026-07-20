//! D3: the pure, toolkit-agnostic menu-bar model. Mirrors `tray/model.rs`'s
//! split exactly, for the same reason — a native menu cannot be clicked in
//! CI, so **every branch (which items exist, which are enabled, which carry
//! a command) lives here and is unit-tested**; `menubar/mod.rs` is a dumb
//! translation of this model into `tauri::menu::*` with no branching of its
//! own.
//!
//! **This module must not reference Tauri at all** — that absence is what
//! makes the gate possible (the model can be built and asserted against
//! headlessly, with no GUI session).

use crate::recent::RecentEntry;
use crate::windows::OpenWindow;

/// Every action a menu item can dispatch. `mod.rs` matches this exhaustively
/// (no wildcard arm) so a variant added here without being wired there fails
/// to compile instead of shipping a dead click.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    NewFile,
    NewProject,
    OpenFile,
    OpenRecent(String),
    OpenVault,
    Import,
    Export,
    RevealInFinder,
    ShowHome,
    ShowSearch,
    ShowPalette,
    ShowPackages,
    ShowTemplates,
    FocusWindow(String),
    About,
    KnownLimits,
}

/// One clickable (or disabled-informational) menu row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub label: String,
    pub accelerator: Option<String>,
    pub enabled: bool,
    /// `None` for a disabled placeholder row (e.g. the empty Open Recent
    /// entry) — see [`every_enabled_item_carries_a_command`] in the tests:
    /// every ENABLED item must carry one, but a disabled placeholder need not.
    pub command: Option<Command>,
}

/// One entry in a menu section, in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Item(Item),
    Separator,
    /// A toolkit-native item this model does not construct itself (the Edit
    /// section's undo/redo/cut/copy/paste/select-all, and the Window
    /// section's minimize/zoom/close) — named by the string `mod.rs` maps to
    /// the matching `PredefinedMenuItem::*` constructor. Kept out of `Item`
    /// because these have no id/command of ours to dispatch: the OS/webview
    /// handles them directly.
    Predefined(&'static str),
}

pub struct MenuSection {
    pub title: String,
    pub entries: Vec<Entry>,
}

pub struct MenuModel {
    pub sections: Vec<MenuSection>,
}

/// File-section item ids the rest of the crate (and the gate) key off of.
pub const NEW_FILE_ID: &str = "file.new-file";
pub const NEW_PROJECT_ID: &str = "file.new-project";
pub const OPEN_FILE_ID: &str = "file.open";
/// Prefix for every Open Recent row, including the empty-store placeholder —
/// `mod.rs` groups the contiguous run of entries with this prefix into a
/// real `Submenu` (a translation detail; the model itself has no submenu
/// concept, only a flat, ordered entry list).
pub const RECENT_PREFIX: &str = "file.recent";
const RECENT_NONE_ID: &str = "file.recent:none";
pub const OPEN_VAULT_ID: &str = "file.open-vault";
pub const IMPORT_ID: &str = "file.import";
pub const EXPORT_ID: &str = "file.export";
pub const REVEAL_ID: &str = "file.reveal";

pub const SHOW_HOME_ID: &str = "view.home";
pub const SHOW_SEARCH_ID: &str = "view.search";
pub const SHOW_PALETTE_ID: &str = "view.palette";
pub const SHOW_PACKAGES_ID: &str = "view.packages";
pub const SHOW_TEMPLATES_ID: &str = "view.templates";

/// Window-menu item ids are `window.focus:<label>` — the window's Tauri
/// LABEL (unique by construction, `WindowRegistry` is keyed on it), not its
/// title, since dispatch needs the label to call `get_webview_window`.
const WINDOW_FOCUS_PREFIX: &str = "window.focus:";

pub const ABOUT_ID: &str = "help.about";
pub const KNOWN_LIMITS_ID: &str = "help.known-limits";

fn file_section(recent: &[RecentEntry], key_has_file: bool) -> MenuSection {
    let mut entries = vec![
        Entry::Item(Item {
            id: NEW_FILE_ID.into(),
            label: "New File".into(),
            accelerator: Some("CmdOrCtrl+N".into()),
            enabled: true,
            command: Some(Command::NewFile),
        }),
        Entry::Item(Item {
            id: NEW_PROJECT_ID.into(),
            label: "New Project".into(),
            accelerator: None,
            enabled: true,
            command: Some(Command::NewProject),
        }),
        Entry::Separator,
        Entry::Item(Item {
            id: OPEN_FILE_ID.into(),
            label: "Open…".into(),
            accelerator: Some("CmdOrCtrl+O".into()),
            enabled: true,
            command: Some(Command::OpenFile),
        }),
    ];

    // Open Recent ▸ — a single disabled placeholder when the store is empty,
    // never an empty submenu (a submenu with nothing in it reads as broken,
    // not "nothing to show yet").
    if recent.is_empty() {
        entries.push(Entry::Item(Item {
            id: RECENT_NONE_ID.into(),
            label: "No recent files".into(),
            accelerator: None,
            enabled: false,
            command: None,
        }));
    } else {
        for r in recent {
            entries.push(Entry::Item(Item {
                id: format!("{RECENT_PREFIX}:{}", r.file_id),
                label: r.title.clone(),
                accelerator: None,
                enabled: true,
                command: Some(Command::OpenRecent(r.file_id.clone())),
            }));
        }
    }

    entries.push(Entry::Item(Item {
        id: OPEN_VAULT_ID.into(),
        label: "Open Vault…".into(),
        accelerator: None,
        enabled: true,
        command: Some(Command::OpenVault),
    }));
    entries.push(Entry::Separator);
    entries.push(Entry::Item(Item {
        id: IMPORT_ID.into(),
        label: "Import…".into(),
        accelerator: None,
        enabled: true,
        command: Some(Command::Import),
    }));
    // Export… and Reveal in Finder only make sense when the key window is a
    // file — see the module doc on `build_menu_model`.
    entries.push(Entry::Item(Item {
        id: EXPORT_ID.into(),
        label: "Export…".into(),
        accelerator: None,
        enabled: key_has_file,
        command: Some(Command::Export),
    }));
    entries.push(Entry::Separator);
    entries.push(Entry::Item(Item {
        id: REVEAL_ID.into(),
        label: "Reveal in Finder".into(),
        accelerator: None,
        enabled: key_has_file,
        command: Some(Command::RevealInFinder),
    }));

    MenuSection { title: "File".into(), entries }
}

/// The Edit section is nothing but toolkit-native items that delegate to
/// whichever webview is focused — there is no branching to unit-test here,
/// which is exactly why it's `Predefined` rather than modelled as `Item`s.
fn edit_section() -> MenuSection {
    MenuSection {
        title: "Edit".into(),
        entries: vec![
            Entry::Predefined("undo"),
            Entry::Predefined("redo"),
            Entry::Separator,
            Entry::Predefined("cut"),
            Entry::Predefined("copy"),
            Entry::Predefined("paste"),
            Entry::Predefined("select-all"),
        ],
    }
}

fn view_section() -> MenuSection {
    let item = |id: &str, label: &str, accelerator: Option<&str>, command: Command| {
        Entry::Item(Item {
            id: id.into(),
            label: label.into(),
            accelerator: accelerator.map(Into::into),
            enabled: true,
            command: Some(command),
        })
    };
    MenuSection {
        title: "View".into(),
        entries: vec![
            item(SHOW_HOME_ID, "Home", None, Command::ShowHome),
            item(SHOW_SEARCH_ID, "Search", Some("CmdOrCtrl+F"), Command::ShowSearch),
            item(SHOW_PALETTE_ID, "Palette", None, Command::ShowPalette),
            item(SHOW_PACKAGES_ID, "Packages", None, Command::ShowPackages),
            item(SHOW_TEMPLATES_ID, "Templates", None, Command::ShowTemplates),
        ],
    }
}

/// One item per open window (see `WindowRegistry::list()`'s doc for the
/// stable "home first, then files by title" order this inherits) plus the
/// predefined minimize/zoom/close triad.
///
/// KNOWN LIMIT: the model has no way to mark which window is currently
/// key — `windows` (a plain `&[OpenWindow]`) carries no "this one is key"
/// flag, and `key_has_file` is only a bool (file vs. no-file), not an
/// identity. Marking the key window would need either `OpenWindow` itself to
/// carry that flag or a second parameter naming its label; neither exists in
/// the interface this task was given, so the Window section lists every
/// window unmarked rather than guessing. Flagged here instead of silently
/// shipping a menu that claims to mark the key window and doesn't.
fn window_section(windows: &[OpenWindow]) -> MenuSection {
    let mut entries: Vec<Entry> = windows
        .iter()
        .map(|w| {
            Entry::Item(Item {
                id: format!("{WINDOW_FOCUS_PREFIX}{}", w.label),
                label: w.title.clone(),
                accelerator: None,
                enabled: true,
                command: Some(Command::FocusWindow(w.label.clone())),
            })
        })
        .collect();
    if !windows.is_empty() {
        entries.push(Entry::Separator);
    }
    entries.push(Entry::Predefined("minimize"));
    entries.push(Entry::Predefined("zoom"));
    entries.push(Entry::Predefined("close-window"));
    MenuSection { title: "Window".into(), entries }
}

fn help_section() -> MenuSection {
    MenuSection {
        title: "Help".into(),
        entries: vec![
            Entry::Item(Item {
                id: ABOUT_ID.into(),
                label: "About Penpot Local".into(),
                accelerator: None,
                enabled: true,
                command: Some(Command::About),
            }),
            Entry::Item(Item {
                id: KNOWN_LIMITS_ID.into(),
                label: "Known Limits".into(),
                accelerator: None,
                enabled: true,
                command: Some(Command::KnownLimits),
            }),
        ],
    }
}

/// The pure builder: current windows + the recent-files store + whether the
/// key (frontmost) window is showing a file → the full menu model.
///
/// **No Preferences item and no `CmdOrCtrl+,`** — D4 owns Preferences; a
/// pre-D4 entry with nothing behind it would violate the "no orphaned or
/// dead menu items" rule the gate enforces (see `preferences_is_absent_in_d3`
/// below).
///
/// **Context-sensitivity:** Export… and Reveal in Finder are meaningless
/// unless the key window is a file, so `key_has_file: false` disables both.
/// Open Recent with an empty store is one disabled placeholder row, never an
/// empty submenu.
pub fn build_menu_model(windows: &[OpenWindow], recent: &[RecentEntry], key_has_file: bool) -> MenuModel {
    MenuModel {
        sections: vec![
            file_section(recent, key_has_file),
            edit_section(),
            view_section(),
            window_section(windows),
            help_section(),
        ],
    }
}

/// Resolve a clicked item id back to its `Command`. Ids are unique by
/// construction (see `item_ids_are_unique` below), so the first match is the
/// only match; a duplicate would make this silently ambiguous, which is
/// exactly the failure mode that test guards against.
pub fn command_for_id(model: &MenuModel, id: &str) -> Option<Command> {
    model
        .sections
        .iter()
        .flat_map(|s| &s.entries)
        .find_map(|e| match e {
            Entry::Item(i) if i.id == id => i.command.clone(),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(label: &str, file: Option<&str>, title: &str) -> OpenWindow {
        OpenWindow { label: label.into(), file_id: file.map(Into::into), title: title.into() }
    }

    fn all_items(m: &MenuModel) -> Vec<&Item> {
        m.sections.iter().flat_map(|s| s.entries.iter()).filter_map(|e| match e {
            Entry::Item(i) => Some(i), _ => None,
        }).collect()
    }

    #[test]
    fn the_top_level_sections_are_the_five_we_promise() {
        let m = build_menu_model(&[], &[], false);
        let titles: Vec<&str> = m.sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["File", "Edit", "View", "Window", "Help"]);
    }

    #[test]
    fn the_promised_accelerators_are_present_and_correct() {
        let m = build_menu_model(&[], &[], false);
        let acc = |id: &str| all_items(&m).into_iter().find(|i| i.id == id)
            .unwrap_or_else(|| panic!("no item {id}")).accelerator.clone();
        assert_eq!(acc("file.new-file"), Some("CmdOrCtrl+N".into()));
        assert_eq!(acc("file.open"), Some("CmdOrCtrl+O".into()));
        assert_eq!(acc("view.search"), Some("CmdOrCtrl+F".into()));
    }

    #[test]
    fn preferences_is_absent_in_d3() {
        // D4 adds it together with the window it opens. A dead item would
        // break the no-orphaned-items rule the gate enforces.
        let m = build_menu_model(&[], &[], false);
        assert!(all_items(&m).iter().all(|i| !i.id.contains("preferences")));
        assert!(all_items(&m).iter().all(|i| i.accelerator.as_deref() != Some("CmdOrCtrl+,")));
    }

    #[test]
    fn every_enabled_item_carries_a_command() {
        // The no-dead-items rule, as an assertion.
        let m = build_menu_model(&[win("main", None, "Penpot Local")], &[], false);
        for i in all_items(&m) {
            if i.enabled {
                assert!(i.command.is_some(), "enabled item {} has no command", i.id);
            }
        }
    }

    #[test]
    fn item_ids_are_unique() {
        // Duplicate ids would make command_for_id ambiguous and silently
        // dispatch the wrong action.
        let m = build_menu_model(&[win("main", None, "Home"), win("file-f1", Some("f1"), "Alpha")],
                                 &[RecentEntry { file_id: "f1".into(), title: "Alpha".into(), page_id: None, opened_at: "x".into() }],
                                 true);
        let mut ids: Vec<&str> = all_items(&m).iter().map(|i| i.id.as_str()).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate menu item ids");
    }

    #[test]
    fn file_only_actions_are_disabled_when_the_key_window_is_not_a_file() {
        let m = build_menu_model(&[win("main", None, "Home")], &[], false);
        for id in ["file.export", "file.reveal"] {
            let i = all_items(&m).into_iter().find(|i| i.id == id).unwrap();
            assert!(!i.enabled, "{id} should be disabled with no file window key");
        }
        let m2 = build_menu_model(&[win("file-f1", Some("f1"), "Alpha")], &[], true);
        for id in ["file.export", "file.reveal"] {
            let i = all_items(&m2).into_iter().find(|i| i.id == id).unwrap();
            assert!(i.enabled, "{id} should be enabled when a file window is key");
        }
    }

    #[test]
    fn empty_recent_shows_one_disabled_placeholder_not_an_empty_submenu() {
        let m = build_menu_model(&[], &[], false);
        let recents: Vec<&Item> = all_items(&m).into_iter().filter(|i| i.id.starts_with("file.recent")).collect();
        assert_eq!(recents.len(), 1);
        assert!(!recents[0].enabled);
        assert!(recents[0].command.is_none());
    }

    #[test]
    fn the_window_menu_lists_every_open_window() {
        let wins = [win("main", None, "Penpot Local"), win("file-f1", Some("f1"), "Alpha")];
        let m = build_menu_model(&wins, &[], true);
        let window_section = m.sections.iter().find(|s| s.title == "Window").unwrap();
        let labels: Vec<&str> = window_section.entries.iter().filter_map(|e| match e {
            Entry::Item(i) => Some(i.label.as_str()), _ => None,
        }).collect();
        assert!(labels.contains(&"Penpot Local") && labels.contains(&"Alpha"), "{labels:?}");
    }

    #[test]
    fn command_for_id_round_trips_every_enabled_item() {
        let m = build_menu_model(&[win("main", None, "Home")], &[], false);
        for i in all_items(&m) {
            if i.enabled {
                assert!(command_for_id(&m, &i.id).is_some(), "no command resolved for {}", i.id);
            }
        }
    }
}
