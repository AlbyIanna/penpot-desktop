//! Pure menu-model builder: `SyncStatusSnapshot` → list of menu entries +
//! aggregate icon state. The tray itself cannot be driven headlessly, so
//! **every branch lives here** and is unit-tested; the Tauri wiring in
//! `tray/mod.rs` is a dumb translation of this model.

use chrono::{DateTime, Utc};

use crate::status::{ExportStatusSnapshot, FileState, SyncStatusSnapshot};

/// Menu item id of the pause/resume toggle.
pub const PAUSE_TOGGLE_ID: &str = "sync-pause-toggle";
/// Menu item id of the quit entry.
pub const QUIT_ID: &str = "sync-quit";
/// Menu item id of "Open Designs Folder" (M5 reveal-in-file-manager).
pub const OPEN_DESIGNS_ID: &str = "open-designs-folder";
/// Menu item id of "Enable git versioning" (M5 git helper).
pub const GIT_INIT_ID: &str = "designs-git-init";
/// Menu item id of "Quick open…" (N4 palette — reachable from the tray so the
/// palette is usable/testable without the global shortcut, PLAN2.md risk 7).
pub const QUICK_OPEN_ID: &str = "quick-open-palette";
/// Menu item id of "Checkpoint now" (N4b manual git checkpoint verb).
pub const CHECKPOINT_ID: &str = "vault-checkpoint-now";
/// Menu item id of "Open Vault…" (N5 — switch to a different design vault).
pub const OPEN_VAULT_ID: &str = "vault-open";
/// Per-file rows have id `sync-file:<relative path>`; the wiring layer strips
/// this prefix to get the path to reveal.
pub const FILE_ROW_PREFIX: &str = "sync-file:";
/// Maximum per-file rows before collapsing into "… and N more".
pub const MAX_FILE_ROWS: usize = 10;
/// Maximum characters of an error message shown in the menu.
const MAX_ERROR_CHARS: usize = 60;

/// Aggregate daemon state, rendered as the tray icon glyph.
/// Escalation order (highest wins): Attention > Paused > Syncing > Idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateState {
    Idle,
    Syncing,
    Paused,
    /// At least one conflict or error needs the user's attention.
    Attention,
}

/// One entry of the tray menu, UI-toolkit-agnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    /// Non-interactive informational row (rendered as a disabled item).
    Info { id: String, label: String },
    /// A clickable per-file row (id `sync-file:<key>`): clicking reveals the
    /// file's directory in the file manager. Only emitted when the designs
    /// dir is known; otherwise files render as [`MenuEntry::Info`].
    File { key: String, label: String },
    /// "Open Designs Folder" (enabled). Always has id [`OPEN_DESIGNS_ID`].
    OpenDesigns { label: String },
    /// "Enable git versioning" (enabled). Always has id [`GIT_INIT_ID`].
    GitInit { label: String },
    /// "Quick open…" — opens the N4 palette overlay. Id [`QUICK_OPEN_ID`].
    QuickOpen { label: String },
    /// "Checkpoint now" — the N4b manual git checkpoint. Id [`CHECKPOINT_ID`].
    Checkpoint { label: String },
    /// "Open Vault…" — the N5 switch-vault action. Id [`OPEN_VAULT_ID`].
    OpenVault { label: String },
    /// The pause/resume toggle (enabled). Always has id [`PAUSE_TOGGLE_ID`].
    PauseToggle { label: String },
    Separator,
    /// Quit the app (enabled). Always has id [`QUIT_ID`].
    Quit { label: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuModel {
    pub aggregate: AggregateState,
    pub entries: Vec<MenuEntry>,
}

/// Sort rank: conflicts and errors surface first, then in-flight work,
/// then pending, then synced. Ties keep BTreeMap (path) order.
fn state_rank(state: &FileState) -> u8 {
    match state {
        FileState::Conflict { .. } => 0,
        FileState::Error { .. } => 1,
        FileState::Importing | FileState::Exporting => 2,
        FileState::Pending => 3,
        FileState::Synced => 4,
    }
}

fn state_glyph(state: &FileState) -> &'static str {
    match state {
        FileState::Synced => "✓",
        FileState::Pending => "•",
        FileState::Importing => "↑", // FS → DB
        FileState::Exporting => "↓", // DB → FS
        FileState::Conflict { .. } => "⚠",
        FileState::Error { .. } => "✕",
    }
}

/// `"Client A/homepage.penpot"` → `"homepage"`.
fn display_name(key: &str) -> &str {
    let base = key.rsplit('/').next().unwrap_or(key);
    base.strip_suffix(".penpot").unwrap_or(base)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Human relative time for the "Last sync:" row. `now` is a parameter so the
/// function stays pure/testable.
pub fn relative_time(rfc3339: &str, now: DateTime<Utc>) -> String {
    let Ok(t) = DateTime::parse_from_rfc3339(rfc3339) else {
        return "unknown".to_string();
    };
    let secs = (now - t.with_timezone(&Utc)).num_seconds();
    if secs < 10 {
        // Includes small negative skew: clocks are never perfectly aligned.
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

fn aggregate(snapshot: &SyncStatusSnapshot, exports: Option<&ExportStatusSnapshot>) -> AggregateState {
    let attention = snapshot.last_error.is_some()
        || exports.is_some_and(|e| e.last_error.is_some())
        || snapshot
            .files
            .values()
            .any(|s| matches!(s, FileState::Conflict { .. } | FileState::Error { .. }));
    if attention {
        AggregateState::Attention
    } else if snapshot.paused {
        AggregateState::Paused
    } else if snapshot.files.values().any(|s| {
        matches!(
            s,
            FileState::Pending | FileState::Importing | FileState::Exporting
        )
    }) || exports.is_some_and(|e| e.rendering.is_some() || e.files_pending > 0)
    {
        AggregateState::Syncing
    } else {
        AggregateState::Idle
    }
}

/// The "Exports: …" info row (M5 board-export service status), present only
/// when the service runs (`PENPOT_LOCAL_EXPORTS=1`). Pure so every branch is
/// unit-testable.
pub fn exports_label(exports: &ExportStatusSnapshot, now: DateTime<Utc>) -> String {
    if let Some(err) = &exports.last_error {
        return format!("Exports: error — {}", truncate_chars(err, MAX_ERROR_CHARS));
    }
    if let Some(rel_path) = &exports.rendering {
        return format!("Exports: rendering {}…", display_name(rel_path));
    }
    if exports.files_pending > 0 {
        return format!("Exports: {} file(s) queued", exports.files_pending);
    }
    match &exports.last_render_at {
        Some(ts) => format!(
            "Exports: up to date ({} files, rendered {})",
            exports.files_up_to_date,
            relative_time(ts, now)
        ),
        None if exports.files_up_to_date > 0 => {
            format!("Exports: up to date ({} files)", exports.files_up_to_date)
        }
        None => "Exports: idle".to_string(),
    }
}

/// The pure builder: snapshot (+ `now` for relative time) → menu model.
/// `designs_available` = the designs dir is known to the wiring layer, so
/// file-manager actions (per-file reveal, "Open Designs Folder", git init)
/// can actually be performed; when `false` those entries are absent and
/// file rows are plain disabled info rows (pre-M5 shape, byte-identical).
pub fn build_menu_model(
    snapshot: &SyncStatusSnapshot,
    now: DateTime<Utc>,
    designs_available: bool,
    exports: Option<&ExportStatusSnapshot>,
) -> MenuModel {
    let mut entries = Vec::new();

    // "Last sync: …" header (disabled info row).
    let last_sync = match &snapshot.last_sync_at {
        Some(ts) => relative_time(ts, now),
        None => "never".to_string(),
    };
    entries.push(MenuEntry::Info {
        id: "sync-last-sync".into(),
        label: format!("Last sync: {last_sync}"),
    });

    // "Exports: …" line (M5) — only when the board-export service runs;
    // absent = the exact pre-M5 menu shape.
    if let Some(exports) = exports {
        entries.push(MenuEntry::Info {
            id: "exports-status".into(),
            label: exports_label(exports, now),
        });
    }

    // Daemon-level error, if any.
    if let Some(err) = &snapshot.last_error {
        entries.push(MenuEntry::Info {
            id: "sync-last-error".into(),
            label: format!("Error: {}", truncate_chars(err, MAX_ERROR_CHARS)),
        });
    }

    // Per-file section: conflicts/errors first, then active, pending, synced;
    // BTreeMap gives stable path order within each rank (stable sort).
    if !snapshot.files.is_empty() {
        entries.push(MenuEntry::Separator);
        let mut files: Vec<(&String, &FileState)> = snapshot.files.iter().collect();
        files.sort_by_key(|(_, state)| state_rank(state));
        let total = files.len();
        for (key, state) in files.iter().take(MAX_FILE_ROWS) {
            let mut label = format!("{}  {}", state_glyph(state), display_name(key));
            if let FileState::Conflict { copy_path } = state {
                label.push_str(&format!(" — conflict copy: {}", display_name(copy_path)));
            }
            if designs_available {
                // Clickable: reveals the file's directory in the file manager.
                entries.push(MenuEntry::File { key: (*key).clone(), label });
            } else {
                entries.push(MenuEntry::Info {
                    id: format!("{FILE_ROW_PREFIX}{key}"),
                    label,
                });
            }
        }
        if total > MAX_FILE_ROWS {
            entries.push(MenuEntry::Info {
                id: "sync-file-overflow".into(),
                label: format!("… and {} more", total - MAX_FILE_ROWS),
            });
        }
    }

    if designs_available {
        entries.push(MenuEntry::Separator);
        entries.push(MenuEntry::QuickOpen {
            label: "Quick open…".to_string(),
        });
        entries.push(MenuEntry::Checkpoint {
            label: "Checkpoint now".to_string(),
        });
        entries.push(MenuEntry::OpenDesigns {
            label: "Open Designs Folder".to_string(),
        });
        entries.push(MenuEntry::OpenVault {
            label: "Open Vault…".to_string(),
        });
        entries.push(MenuEntry::GitInit {
            label: "Enable git versioning".to_string(),
        });
    }

    entries.push(MenuEntry::Separator);
    entries.push(MenuEntry::PauseToggle {
        label: if snapshot.paused {
            "Resume syncing".to_string()
        } else {
            "Pause syncing".to_string()
        },
    });
    entries.push(MenuEntry::Separator);
    entries.push(MenuEntry::Quit {
        label: "Quit Penpot Local".to_string(),
    });

    MenuModel {
        aggregate: aggregate(snapshot, exports),
        entries,
    }
}

// ---------------------------------------------------------------------------
// Tray icon bitmaps (generated programmatically; macOS template style:
// pure black + alpha, so the OS recolors them for light/dark menubars).
// ---------------------------------------------------------------------------

pub const ICON_SIZE: u32 = 32;

/// 32×32 RGBA pixels for the given aggregate state. Shapes (not colors)
/// distinguish states, because template icons render monochrome:
/// Idle = filled circle, Syncing = ring, Paused = pause bars,
/// Attention = exclamation mark.
pub fn icon_pixels(state: AggregateState) -> Vec<u8> {
    let n = ICON_SIZE as i32;
    let mut buf = vec![0u8; (ICON_SIZE * ICON_SIZE * 4) as usize];
    let c = (n as f32 - 1.0) / 2.0; // 15.5
    for y in 0..n {
        for x in 0..n {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let d2 = dx * dx + dy * dy;
            let on = match state {
                AggregateState::Idle => d2 <= 100.0, // r = 10
                AggregateState::Syncing => (49.0..=121.0).contains(&d2), // ring r 7..11
                AggregateState::Paused => {
                    ((9..=13).contains(&x) || (19..=23).contains(&x)) && (7..=25).contains(&y)
                }
                AggregateState::Attention => {
                    (13..=18).contains(&x) && ((5..=19).contains(&y) || (23..=27).contains(&y))
                }
            };
            if on {
                let i = ((y * n + x) * 4) as usize;
                buf[i..i + 4].copy_from_slice(&[0, 0, 0, 255]);
            }
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::SyncStatusSnapshot;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-13T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn snapshot(files: &[(&str, FileState)]) -> SyncStatusSnapshot {
        SyncStatusSnapshot {
            last_sync_at: None,
            files: files
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            paused: false,
            last_error: None,
        }
    }

    fn labels(model: &MenuModel) -> Vec<String> {
        model
            .entries
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Info { label, .. } => Some(label.clone()),
                _ => None,
            })
            .collect()
    }

    fn pause_label(model: &MenuModel) -> String {
        model
            .entries
            .iter()
            .find_map(|e| match e {
                MenuEntry::PauseToggle { label } => Some(label.clone()),
                _ => None,
            })
            .expect("pause toggle always present")
    }

    #[test]
    fn empty_snapshot_menu_shape() {
        let model = build_menu_model(&SyncStatusSnapshot::default(), now(), false, None);
        assert_eq!(model.aggregate, AggregateState::Idle);
        assert_eq!(labels(&model), vec!["Last sync: never"]);
        assert_eq!(pause_label(&model), "Pause syncing");
        assert!(matches!(model.entries.last(), Some(MenuEntry::Quit { .. })));
        // No file section → no separator between header and toggle beyond the
        // fixed ones: header, sep, toggle, sep, quit.
        assert_eq!(model.entries.len(), 5);
    }

    #[test]
    fn relative_time_buckets() {
        let n = now();
        assert_eq!(relative_time("2026-07-13T11:59:55Z", n), "just now");
        assert_eq!(relative_time("2026-07-13T12:00:03Z", n), "just now"); // small skew
        assert_eq!(relative_time("2026-07-13T11:59:15Z", n), "45s ago");
        assert_eq!(relative_time("2026-07-13T11:53:00Z", n), "7m ago");
        assert_eq!(relative_time("2026-07-13T09:00:00Z", n), "3h ago");
        assert_eq!(relative_time("2026-07-10T12:00:00Z", n), "3d ago");
        assert_eq!(relative_time("2026-07-13T13:00:00+01:00", n), "just now"); // tz offset
        assert_eq!(relative_time("not-a-timestamp", n), "unknown");
    }

    #[test]
    fn every_state_gets_its_glyph() {
        let model = build_menu_model(
            &snapshot(&[
                ("P/a.penpot", FileState::Synced),
                ("P/b.penpot", FileState::Pending),
                ("P/c.penpot", FileState::Importing),
                ("P/d.penpot", FileState::Exporting),
                (
                    "P/e.penpot",
                    FileState::Conflict {
                        copy_path: "P/e.conflict-20260713T120000.penpot".into(),
                    },
                ),
                ("P/f.penpot", FileState::Error { message: "boom".into() }),
            ]),
            now(),
            false,
            None,
        );
        let rows = labels(&model);
        assert!(rows.iter().any(|l| l == "✓  a"));
        assert!(rows.iter().any(|l| l == "•  b"));
        assert!(rows.iter().any(|l| l == "↑  c"));
        assert!(rows.iter().any(|l| l == "↓  d"));
        assert!(rows
            .iter()
            .any(|l| l == "⚠  e — conflict copy: e.conflict-20260713T120000"));
        assert!(rows.iter().any(|l| l == "✕  f"));
    }

    #[test]
    fn conflicts_and_errors_sort_first() {
        let model = build_menu_model(
            &snapshot(&[
                ("P/aaa.penpot", FileState::Synced),
                ("P/bbb.penpot", FileState::Pending),
                ("P/yyy.penpot", FileState::Error { message: "x".into() }),
                (
                    "P/zzz.penpot",
                    FileState::Conflict {
                        copy_path: "P/zzz.conflict-1.penpot".into(),
                    },
                ),
            ]),
            now(),
            false,
            None,
        );
        let rows = labels(&model);
        // rows[0] is the "Last sync" header.
        assert!(rows[1].starts_with("⚠  zzz"), "conflict first, got {rows:?}");
        assert!(rows[2].starts_with("✕  yyy"), "error second, got {rows:?}");
        assert!(rows[3].starts_with("•  bbb"));
        assert!(rows[4].starts_with("✓  aaa"));
    }

    #[test]
    fn overflow_collapses_beyond_max_rows() {
        let files: Vec<(String, FileState)> = (0..14)
            .map(|i| (format!("P/f{i:02}.penpot"), FileState::Synced))
            .collect();
        let refs: Vec<(&str, FileState)> = files
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();
        let model = build_menu_model(&snapshot(&refs), now(), false, None);
        let rows = labels(&model);
        let file_rows = rows.iter().filter(|l| l.starts_with("✓")).count();
        assert_eq!(file_rows, MAX_FILE_ROWS);
        assert_eq!(rows.last().unwrap(), "… and 4 more");
    }

    #[test]
    fn exactly_max_rows_has_no_overflow() {
        let files: Vec<(String, FileState)> = (0..MAX_FILE_ROWS)
            .map(|i| (format!("P/f{i:02}.penpot"), FileState::Synced))
            .collect();
        let refs: Vec<(&str, FileState)> = files
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();
        let model = build_menu_model(&snapshot(&refs), now(), false, None);
        assert!(!labels(&model).iter().any(|l| l.contains("more")));
    }

    #[test]
    fn pause_label_flips_and_aggregate_shows_paused() {
        let mut s = snapshot(&[("P/a.penpot", FileState::Synced)]);
        s.paused = true;
        let model = build_menu_model(&s, now(), false, None);
        assert_eq!(pause_label(&model), "Resume syncing");
        assert_eq!(model.aggregate, AggregateState::Paused);
        s.paused = false;
        let model = build_menu_model(&s, now(), false, None);
        assert_eq!(pause_label(&model), "Pause syncing");
        assert_eq!(model.aggregate, AggregateState::Idle);
    }

    #[test]
    fn aggregate_escalation_order() {
        // Syncing when anything is in flight.
        let s = snapshot(&[("P/a.penpot", FileState::Exporting)]);
        assert_eq!(build_menu_model(&s, now(), false, None).aggregate, AggregateState::Syncing);
        // Attention beats paused AND syncing.
        let mut s = snapshot(&[
            ("P/a.penpot", FileState::Pending),
            (
                "P/b.penpot",
                FileState::Conflict { copy_path: "P/b.conflict-1.penpot".into() },
            ),
        ]);
        s.paused = true;
        assert_eq!(build_menu_model(&s, now(), false, None).aggregate, AggregateState::Attention);
        // Daemon-level last_error alone is attention-worthy.
        let mut s = snapshot(&[("P/a.penpot", FileState::Synced)]);
        s.last_error = Some("rpc down".into());
        assert_eq!(build_menu_model(&s, now(), false, None).aggregate, AggregateState::Attention);
    }

    #[test]
    fn last_error_row_is_shown_and_truncated() {
        let mut s = snapshot(&[]);
        s.last_error = Some("e".repeat(200));
        let model = build_menu_model(&s, now(), false, None);
        let rows = labels(&model);
        let err_row = rows.iter().find(|l| l.starts_with("Error: ")).unwrap();
        assert!(err_row.chars().count() <= "Error: ".chars().count() + 60);
        assert!(err_row.ends_with('…'));
    }

    #[test]
    fn last_sync_uses_relative_time() {
        let mut s = snapshot(&[]);
        s.last_sync_at = Some("2026-07-13T11:53:00Z".into());
        let model = build_menu_model(&s, now(), false, None);
        assert_eq!(labels(&model)[0], "Last sync: 7m ago");
    }

    #[test]
    fn toggle_and_quit_ids_are_stable() {
        // The wiring layer matches on these ids; make sure the model always
        // emits exactly one of each.
        let model = build_menu_model(&SyncStatusSnapshot::default(), now(), false, None);
        let toggles = model
            .entries
            .iter()
            .filter(|e| matches!(e, MenuEntry::PauseToggle { .. }))
            .count();
        let quits = model
            .entries
            .iter()
            .filter(|e| matches!(e, MenuEntry::Quit { .. }))
            .count();
        assert_eq!((toggles, quits), (1, 1));
        assert_eq!(PAUSE_TOGGLE_ID, "sync-pause-toggle");
        assert_eq!(QUIT_ID, "sync-quit");
    }

    #[test]
    fn designs_available_adds_actions_and_clickable_file_rows() {
        let model = build_menu_model(
            &snapshot(&[("Client A/home.penpot", FileState::Synced)]),
            now(),
            true,
            None,
        );
        // File rows become enabled File entries carrying the raw key (the
        // wiring layer joins it onto the designs root to reveal it).
        let file_rows: Vec<(&str, &str)> = model
            .entries
            .iter()
            .filter_map(|e| match e {
                MenuEntry::File { key, label } => Some((key.as_str(), label.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(file_rows, vec![("Client A/home.penpot", "✓  home")]);
        // No disabled Info duplicate of the file row.
        assert!(!model.entries.iter().any(
            |e| matches!(e, MenuEntry::Info { id, .. } if id.starts_with(FILE_ROW_PREFIX))
        ));
        // Exactly one of each action entry, with the expected labels.
        let opens = model
            .entries
            .iter()
            .filter(|e| matches!(e, MenuEntry::OpenDesigns { label } if label == "Open Designs Folder"))
            .count();
        let gits = model
            .entries
            .iter()
            .filter(|e| matches!(e, MenuEntry::GitInit { label } if label == "Enable git versioning"))
            .count();
        assert_eq!((opens, gits), (1, 1));
        // N5: exactly one "Open Vault…" switch action.
        let open_vaults = model
            .entries
            .iter()
            .filter(|e| matches!(e, MenuEntry::OpenVault { label } if label == "Open Vault…"))
            .count();
        assert_eq!(open_vaults, 1);
    }

    #[test]
    fn designs_unavailable_keeps_the_pre_m5_menu_shape() {
        let model = build_menu_model(
            &snapshot(&[("P/a.penpot", FileState::Synced)]),
            now(),
            false,
            None,
        );
        assert!(
            !model.entries.iter().any(|e| matches!(
                e,
                MenuEntry::File { .. } | MenuEntry::OpenDesigns { .. } | MenuEntry::GitInit { .. }
            )),
            "no designs dir → no file-manager actions, file rows stay disabled Info"
        );
        assert!(model
            .entries
            .iter()
            .any(|e| matches!(e, MenuEntry::Info { id, .. } if id == "sync-file:P/a.penpot")));
    }

    #[test]
    fn conflict_file_rows_are_clickable_too_and_keep_the_copy_hint() {
        let model = build_menu_model(
            &snapshot(&[(
                "P/e.penpot",
                FileState::Conflict {
                    copy_path: "P/e.conflict-20260713T120000.penpot".into(),
                },
            )]),
            now(),
            true,
            None,
        );
        assert!(model.entries.iter().any(|e| matches!(
            e,
            MenuEntry::File { key, label }
                if key == "P/e.penpot" && label.contains("conflict copy: e.conflict-20260713T120000")
        )));
    }

    // ---------------- exports row (M5 board-export integration) ----------------

    #[test]
    fn no_exports_snapshot_keeps_the_pre_m5_menu_shape() {
        let model = build_menu_model(&SyncStatusSnapshot::default(), now(), false, None);
        assert!(
            !model
                .entries
                .iter()
                .any(|e| matches!(e, MenuEntry::Info { id, .. } if id == "exports-status")),
            "exports row must be absent when the service is off"
        );
    }

    #[test]
    fn exports_label_covers_every_branch() {
        let mut e = ExportStatusSnapshot::default();
        assert_eq!(exports_label(&e, now()), "Exports: idle");

        e.files_up_to_date = 3;
        assert_eq!(exports_label(&e, now()), "Exports: up to date (3 files)");

        e.last_render_at = Some("2026-07-13T11:53:00Z".into());
        assert_eq!(
            exports_label(&e, now()),
            "Exports: up to date (3 files, rendered 7m ago)"
        );

        e.files_pending = 2;
        assert_eq!(exports_label(&e, now()), "Exports: 2 file(s) queued");

        e.rendering = Some("Client A/homepage.penpot".into());
        assert_eq!(exports_label(&e, now()), "Exports: rendering homepage…");

        e.last_error = Some("x".repeat(200));
        let label = exports_label(&e, now());
        assert!(label.starts_with("Exports: error — "), "{label}");
        assert!(label.ends_with('…'), "long errors truncated: {label}");
    }

    #[test]
    fn exports_row_is_rendered_and_drives_the_aggregate() {
        let s = snapshot(&[("P/a.penpot", FileState::Synced)]);

        // Idle exports: row present, aggregate stays Idle.
        let e = ExportStatusSnapshot {
            files_up_to_date: 1,
            ..Default::default()
        };
        let model = build_menu_model(&s, now(), false, Some(&e));
        assert!(model.entries.iter().any(
            |m| matches!(m, MenuEntry::Info { id, label } if id == "exports-status" && label.starts_with("Exports:"))
        ));
        assert_eq!(model.aggregate, AggregateState::Idle);

        // Rendering escalates to Syncing.
        let e = ExportStatusSnapshot {
            rendering: Some("P/a.penpot".into()),
            ..Default::default()
        };
        assert_eq!(
            build_menu_model(&s, now(), false, Some(&e)).aggregate,
            AggregateState::Syncing
        );

        // A render error escalates to Attention.
        let e = ExportStatusSnapshot {
            last_error: Some("render failed".into()),
            ..Default::default()
        };
        assert_eq!(
            build_menu_model(&s, now(), false, Some(&e)).aggregate,
            AggregateState::Attention
        );
    }

    #[test]
    fn icon_bitmaps_differ_per_state_and_are_opaque_black() {
        let states = [
            AggregateState::Idle,
            AggregateState::Syncing,
            AggregateState::Paused,
            AggregateState::Attention,
        ];
        let pixels: Vec<Vec<u8>> = states.iter().map(|s| icon_pixels(*s)).collect();
        for p in &pixels {
            assert_eq!(p.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
            // Some opaque pixels, and all opaque pixels are black (template).
            let mut opaque = 0;
            for px in p.chunks(4) {
                if px[3] == 255 {
                    opaque += 1;
                    assert_eq!(&px[..3], &[0, 0, 0]);
                } else {
                    assert_eq!(px, &[0, 0, 0, 0]);
                }
            }
            assert!(opaque > 20, "shape too small: {opaque} px");
        }
        for i in 0..pixels.len() {
            for j in (i + 1)..pixels.len() {
                assert_ne!(pixels[i], pixels[j], "states {i} and {j} look identical");
            }
        }
    }
}
