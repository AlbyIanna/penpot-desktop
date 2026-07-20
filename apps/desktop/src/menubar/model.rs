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
    /// D4 — open (or focus) the native Preferences window.
    Preferences,
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

/// A toolkit-native ("predefined") menu item kind — the Edit section's
/// undo/redo/cut/copy/paste/select-all, the Window section's
/// minimize/zoom/close, and (as of the D3-review CRITICAL fix) the
/// application section's about/services/hide family/quit. This used to be a
/// bare `&'static str` that `mod.rs`'s `predefined_item` matched with a
/// `panic!` fallback arm for an unrecognized name — a real risk since that
/// function runs inside `rebuild`, itself called from window-event callbacks
/// (a panic there takes down a UI callback, not just a menu build). An enum
/// closes that hole at compile time instead: `mod.rs`'s translation matches
/// it EXHAUSTIVELY (no wildcard arm), so a variant added here without a
/// matching arm there fails the BUILD, and there is no runtime "unknown name"
/// case left to panic on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Predefined {
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Minimize,
    Zoom,
    CloseWindow,
    About,
    Services,
    Hide,
    HideOthers,
    ShowAll,
    Quit,
}

/// One entry in a menu section, in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Item(Item),
    Separator,
    /// A toolkit-native item this model does not construct itself — see
    /// [`Predefined`]. Kept out of `Item` because these have no id/command of
    /// ours to dispatch: the OS/webview handles them directly.
    Predefined(Predefined),
}

pub struct MenuSection {
    pub title: String,
    pub entries: Vec<Entry>,
}

pub struct MenuModel {
    pub sections: Vec<MenuSection>,
}

/// The running app's display name — used both for the application submenu's
/// title (see [`app_section`]) and, consistently, everywhere else this crate
/// already hardcodes it (the About dialog, the home window's title/registry
/// entry). `model.rs` cannot ask Tauri for the real bundle name (no Tauri
/// imports allowed here — see the module doc), so this is the one literal
/// the rest of the module builds on.
pub const APP_NAME: &str = "Penpot Local";

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

/// D4 — the Preferences item's id, in the application section (see
/// [`app_section`]) where macOS users expect it, carrying `CmdOrCtrl+,`.
pub const PREFERENCES_ID: &str = "app.preferences";

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
            Entry::Predefined(Predefined::Undo),
            Entry::Predefined(Predefined::Redo),
            Entry::Separator,
            Entry::Predefined(Predefined::Cut),
            Entry::Predefined(Predefined::Copy),
            Entry::Predefined(Predefined::Paste),
            Entry::Predefined(Predefined::SelectAll),
        ],
    }
}

/// The application submenu: About / Preferences… / Services / Hide family /
/// Quit. This is the CRITICAL D3-review fix: `mod.rs` installs the whole menu
/// bar wholesale via `AppHandle::set_menu`, which on macOS REPLACES the
/// OS-default application menu rather than merging with it — without this
/// section, the app has no About/Services/Hide/Quit, ⌘Q is dead, and macOS
/// renders "File" in the application-name slot. Must stay the FIRST section
/// [`build_menu_model`] returns (macOS treats the menu bar's first submenu as
/// the application menu); [`app_section_is_first`] below pins that order so
/// this cannot silently regress.
///
/// D4 adds Preferences here, between About and Services — exactly where
/// macOS users expect `CmdOrCtrl+,` to live. It is the one real `Item` (with
/// a `Command`) in an otherwise all-`Predefined` section; D3 deliberately
/// left this slot empty (see `preferences_is_present_in_d4`'s doc for the
/// history) because the window it opens did not exist yet.
fn app_section() -> MenuSection {
    MenuSection {
        title: APP_NAME.into(),
        entries: vec![
            Entry::Predefined(Predefined::About),
            Entry::Separator,
            Entry::Item(Item {
                id: PREFERENCES_ID.into(),
                label: "Preferences…".into(),
                accelerator: Some("CmdOrCtrl+,".into()),
                enabled: true,
                command: Some(Command::Preferences),
            }),
            Entry::Separator,
            Entry::Predefined(Predefined::Services),
            Entry::Predefined(Predefined::Hide),
            Entry::Predefined(Predefined::HideOthers),
            Entry::Predefined(Predefined::ShowAll),
            Entry::Separator,
            Entry::Predefined(Predefined::Quit),
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

/// Leading marker prefixed onto the key (frontmost) window's row in the
/// Window menu, matching `tray/model.rs`'s existing convention of a glyph
/// plus two spaces ahead of the label (see `state_glyph`/its tests there) —
/// same vocabulary, applied to "this is the current one" instead of sync
/// state.
const KEY_WINDOW_MARKER: &str = "\u{2713}  "; // "✓  "

/// One item per open window (see `WindowRegistry::list()`'s doc for the
/// stable "home first, then files by title" order this inherits) plus the
/// predefined minimize/zoom/close triad. The row whose label matches
/// `key_label` gets [`KEY_WINDOW_MARKER`] prefixed onto its label — the D3
/// review's IMPORTANT finding 3 fix: previously the section had no parameter
/// carrying the key window's IDENTITY (only `key_has_file`, a bool with no
/// "which one"), so it could list every window but never mark which one was
/// current.
fn window_section(windows: &[OpenWindow], key_label: Option<&str>) -> MenuSection {
    let mut entries: Vec<Entry> = windows
        .iter()
        .map(|w| {
            let is_key = key_label == Some(w.label.as_str());
            let label = if is_key { format!("{KEY_WINDOW_MARKER}{}", w.title) } else { w.title.clone() };
            Entry::Item(Item {
                id: format!("{WINDOW_FOCUS_PREFIX}{}", w.label),
                label,
                accelerator: None,
                enabled: true,
                command: Some(Command::FocusWindow(w.label.clone())),
            })
        })
        .collect();
    if !windows.is_empty() {
        entries.push(Entry::Separator);
    }
    entries.push(Entry::Predefined(Predefined::Minimize));
    entries.push(Entry::Predefined(Predefined::Zoom));
    entries.push(Entry::Predefined(Predefined::CloseWindow));
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

/// The pure builder: current windows + the recent-files store + the key
/// (frontmost) window's label (if any) → the full menu model.
///
/// **Preferences (D4):** the application section carries a real `Preferences…`
/// item with `CmdOrCtrl+,`, dispatching `Command::Preferences` — see
/// [`app_section`] and `preferences_is_present_in_d4` below. D3 deliberately
/// shipped without it (there was no window yet to open); adding a menu entry
/// with nothing behind it would have violated the "no orphaned or dead menu
/// items" rule the gate enforces just as much as omitting it once the window
/// exists would.
///
/// **Context-sensitivity:** Export… and Reveal in Finder are meaningless
/// unless the key window is a file. `key_label` is looked up against
/// `windows` to derive that — a single source of truth, rather than a
/// separate `key_has_file` bool a caller could pass out of sync with
/// `key_label` itself. Open Recent with an empty store is one disabled
/// placeholder row, never an empty submenu.
///
/// **Leading application section:** the first section returned is always
/// [`app_section`] (About/Services/Hide family/Quit) — see its doc comment
/// for why that is not optional on macOS. [`app_section_is_first`] pins this.
pub fn build_menu_model(windows: &[OpenWindow], recent: &[RecentEntry], key_label: Option<&str>) -> MenuModel {
    let key_has_file = key_label
        .and_then(|label| windows.iter().find(|w| w.label == label))
        .is_some_and(|w| w.file_id.is_some());
    MenuModel {
        sections: vec![
            app_section(),
            file_section(recent, key_has_file),
            edit_section(),
            view_section(),
            window_section(windows, key_label),
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
    fn the_top_level_sections_are_the_six_we_promise() {
        let m = build_menu_model(&[], &[], None);
        let titles: Vec<&str> = m.sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec![APP_NAME, "File", "Edit", "View", "Window", "Help"]);
    }

    /// D3-review CRITICAL fix (finding 1): `mod.rs` installs the menu bar
    /// wholesale via `AppHandle::set_menu`, which REPLACES the OS-default
    /// application menu on macOS rather than merging with it. Without an
    /// application submenu as the very first section, the app loses
    /// About/Services/Hide/Quit — including ⌘Q. This test pins that
    /// ordering and content so it cannot silently regress: it checks the
    /// section the TRANSLATION consumes first (`model.sections[0]`), not
    /// just that a `Quit` entry exists somewhere.
    #[test]
    fn app_section_is_first_and_carries_quit() {
        let m = build_menu_model(&[], &[], None);
        let first = &m.sections[0];
        assert_eq!(first.title, APP_NAME, "the first section must be the application menu");
        assert!(
            first.entries.iter().any(|e| matches!(e, Entry::Predefined(Predefined::Quit))),
            "the application section must carry Quit or ⌘Q has nowhere to attach: {:?}",
            first.entries
        );
        assert!(
            first.entries.iter().any(|e| matches!(e, Entry::Predefined(Predefined::About))),
            "the application section must carry About: {:?}",
            first.entries
        );
    }

    #[test]
    fn the_promised_accelerators_are_present_and_correct() {
        let m = build_menu_model(&[], &[], None);
        let acc = |id: &str| all_items(&m).into_iter().find(|i| i.id == id)
            .unwrap_or_else(|| panic!("no item {id}")).accelerator.clone();
        assert_eq!(acc("file.new-file"), Some("CmdOrCtrl+N".into()));
        assert_eq!(acc("file.open"), Some("CmdOrCtrl+O".into()));
        assert_eq!(acc("view.search"), Some("CmdOrCtrl+F".into()));
    }

    /// Flips `preferences_is_absent_in_d3` (D3 shipped without Preferences —
    /// there was no window yet to open; a menu entry with nothing behind it
    /// would have broken the no-orphaned-items rule just as much as omitting
    /// it now that the window exists would). D4 adds a real Preferences item
    /// carrying `CmdOrCtrl+,`, sitting in the application section (the first
    /// section — see `app_section_is_first_and_carries_quit`) right where
    /// macOS users expect it, between About and Services.
    #[test]
    fn preferences_is_present_in_d4() {
        let m = build_menu_model(&[], &[], None);
        let prefs_item = all_items(&m)
            .into_iter()
            .find(|i| i.id == PREFERENCES_ID)
            .expect("a Preferences item must exist");
        assert_eq!(prefs_item.label, "Preferences…");
        assert_eq!(prefs_item.accelerator.as_deref(), Some("CmdOrCtrl+,"));
        assert!(prefs_item.enabled);
        assert_eq!(prefs_item.command, Some(Command::Preferences));

        // It lives in the application section specifically, not just
        // somewhere in the model.
        let first = &m.sections[0];
        assert_eq!(first.title, APP_NAME);
        assert!(
            first.entries.iter().any(
                |e| matches!(e, Entry::Item(i) if i.id == PREFERENCES_ID)
            ),
            "Preferences must sit in the application section: {:?}",
            first.entries
        );
    }

    #[test]
    fn every_enabled_item_carries_a_command() {
        // The no-dead-items rule, as an assertion.
        let m = build_menu_model(&[win("main", None, "Penpot Local")], &[], None);
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
                                 Some("file-f1"));
        let mut ids: Vec<&str> = all_items(&m).iter().map(|i| i.id.as_str()).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate menu item ids");
    }

    #[test]
    fn file_only_actions_are_disabled_when_the_key_window_is_not_a_file() {
        let m = build_menu_model(&[win("main", None, "Home")], &[], None);
        for id in ["file.export", "file.reveal"] {
            let i = all_items(&m).into_iter().find(|i| i.id == id).unwrap();
            assert!(!i.enabled, "{id} should be disabled with no file window key");
        }
        let m2 = build_menu_model(&[win("file-f1", Some("f1"), "Alpha")], &[], Some("file-f1"));
        for id in ["file.export", "file.reveal"] {
            let i = all_items(&m2).into_iter().find(|i| i.id == id).unwrap();
            assert!(i.enabled, "{id} should be enabled when a file window is key");
        }
    }

    /// A `key_label` that doesn't match any open window (e.g. the key window
    /// just closed and the registry hasn't caught up) must behave exactly
    /// like `None`, not panic or spuriously enable file-only actions.
    #[test]
    fn an_unmatched_key_label_behaves_like_no_key() {
        let m = build_menu_model(&[win("main", None, "Home")], &[], Some("nonexistent-label"));
        for id in ["file.export", "file.reveal"] {
            let i = all_items(&m).into_iter().find(|i| i.id == id).unwrap();
            assert!(!i.enabled, "{id} should stay disabled for an unmatched key label");
        }
    }

    #[test]
    fn empty_recent_shows_one_disabled_placeholder_not_an_empty_submenu() {
        let m = build_menu_model(&[], &[], None);
        let recents: Vec<&Item> = all_items(&m).into_iter().filter(|i| i.id.starts_with("file.recent")).collect();
        assert_eq!(recents.len(), 1);
        assert!(!recents[0].enabled);
        assert!(recents[0].command.is_none());
    }

    #[test]
    fn the_window_menu_lists_every_open_window() {
        let wins = [win("main", None, "Penpot Local"), win("file-f1", Some("f1"), "Alpha")];
        let m = build_menu_model(&wins, &[], None);
        let window_section = m.sections.iter().find(|s| s.title == "Window").unwrap();
        let labels: Vec<&str> = window_section.entries.iter().filter_map(|e| match e {
            Entry::Item(i) => Some(i.label.as_str()), _ => None,
        }).collect();
        assert!(labels.contains(&"Penpot Local") && labels.contains(&"Alpha"), "{labels:?}");
    }

    /// D3-review IMPORTANT fix (finding 3): the Window menu must mark which
    /// window is currently key, using the same leading-glyph vocabulary
    /// `tray/model.rs` already uses for status. The non-key row must stay
    /// unmarked (plain title), and the key row must gain the marker.
    #[test]
    fn the_key_window_is_marked_and_others_are_not() {
        let wins = [win("main", None, "Penpot Local"), win("file-f1", Some("f1"), "Alpha")];
        let m = build_menu_model(&wins, &[], Some("file-f1"));
        let window_section = m.sections.iter().find(|s| s.title == "Window").unwrap();
        let labels: Vec<&str> = window_section.entries.iter().filter_map(|e| match e {
            Entry::Item(i) => Some(i.label.as_str()), _ => None,
        }).collect();
        assert!(labels.contains(&"Penpot Local"), "{labels:?}");
        assert!(labels.contains(&"\u{2713}  Alpha"), "key window Alpha should be marked: {labels:?}");
        assert!(!labels.contains(&"\u{2713}  Penpot Local"), "non-key window must stay unmarked: {labels:?}");
    }

    #[test]
    fn command_for_id_round_trips_every_enabled_item() {
        let m = build_menu_model(&[win("main", None, "Home")], &[], None);
        for i in all_items(&m) {
            if i.enabled {
                assert!(command_for_id(&m, &i.id).is_some(), "no command resolved for {}", i.id);
            }
        }
    }

    /// D3-review MINOR fix (finding 6a): the compiler only proves the
    /// REVERSE direction (`mod.rs`'s exhaustive `run_command` match ensures
    /// every `Command` variant is handled if produced), never that every
    /// variant is actually produced by some model build. This test builds
    /// the model under enough different states to exercise every
    /// Command-producing branch, then matches EXHAUSTIVELY (no wildcard arm)
    /// over `Command` — so a variant added to the enum without a
    /// corresponding branch here fails to compile, not just silently passes
    /// with that variant unseen.
    #[test]
    fn every_command_variant_is_produced_by_some_model_build() {
        let recent = [RecentEntry {
            file_id: "f1".into(),
            title: "Alpha".into(),
            page_id: None,
            opened_at: "x".into(),
        }];
        let wins = [win("main", None, "Penpot Local"), win("file-f1", Some("f1"), "Alpha")];
        let models = [build_menu_model(&wins, &recent, Some("file-f1")), build_menu_model(&[], &[], None)];

        #[derive(Default)]
        struct Seen {
            new_file: bool,
            new_project: bool,
            open_file: bool,
            open_recent: bool,
            open_vault: bool,
            import: bool,
            export: bool,
            reveal_in_finder: bool,
            show_home: bool,
            show_search: bool,
            show_palette: bool,
            show_packages: bool,
            show_templates: bool,
            focus_window: bool,
            about: bool,
            known_limits: bool,
            preferences: bool,
        }
        let mut seen = Seen::default();
        for m in &models {
            for section in &m.sections {
                for entry in &section.entries {
                    let Entry::Item(item) = entry else { continue };
                    let Some(command) = &item.command else { continue };
                    match command {
                        Command::NewFile => seen.new_file = true,
                        Command::NewProject => seen.new_project = true,
                        Command::OpenFile => seen.open_file = true,
                        Command::OpenRecent(_) => seen.open_recent = true,
                        Command::OpenVault => seen.open_vault = true,
                        Command::Import => seen.import = true,
                        Command::Export => seen.export = true,
                        Command::RevealInFinder => seen.reveal_in_finder = true,
                        Command::ShowHome => seen.show_home = true,
                        Command::ShowSearch => seen.show_search = true,
                        Command::ShowPalette => seen.show_palette = true,
                        Command::ShowPackages => seen.show_packages = true,
                        Command::ShowTemplates => seen.show_templates = true,
                        Command::FocusWindow(_) => seen.focus_window = true,
                        Command::About => seen.about = true,
                        Command::KnownLimits => seen.known_limits = true,
                        Command::Preferences => seen.preferences = true,
                    }
                }
            }
        }
        assert!(seen.new_file, "Command::NewFile never produced by any model build");
        assert!(seen.new_project, "Command::NewProject never produced by any model build");
        assert!(seen.open_file, "Command::OpenFile never produced by any model build");
        assert!(seen.open_recent, "Command::OpenRecent never produced by any model build");
        assert!(seen.open_vault, "Command::OpenVault never produced by any model build");
        assert!(seen.import, "Command::Import never produced by any model build");
        assert!(seen.export, "Command::Export never produced by any model build");
        assert!(seen.reveal_in_finder, "Command::RevealInFinder never produced by any model build");
        assert!(seen.show_home, "Command::ShowHome never produced by any model build");
        assert!(seen.show_search, "Command::ShowSearch never produced by any model build");
        assert!(seen.show_palette, "Command::ShowPalette never produced by any model build");
        assert!(seen.show_packages, "Command::ShowPackages never produced by any model build");
        assert!(seen.show_templates, "Command::ShowTemplates never produced by any model build");
        assert!(seen.focus_window, "Command::FocusWindow never produced by any model build");
        assert!(seen.about, "Command::About never produced by any model build");
        assert!(seen.known_limits, "Command::KnownLimits never produced by any model build");
        assert!(seen.preferences, "Command::Preferences never produced by any model build");
    }
}
