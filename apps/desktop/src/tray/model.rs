//! Pure menu-model builder: `SyncStatusSnapshot` → list of menu entries +
//! aggregate icon state. The tray itself cannot be driven headlessly, so
//! **every branch lives here** and is unit-tested; the Tauri wiring in
//! `tray/mod.rs` is a dumb translation of this model.

use chrono::{DateTime, Utc};

use crate::status::{FileState, SyncStatusSnapshot};

/// Menu item id of the pause/resume toggle.
pub const PAUSE_TOGGLE_ID: &str = "sync-pause-toggle";
/// Menu item id of the quit entry.
pub const QUIT_ID: &str = "sync-quit";
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

fn aggregate(snapshot: &SyncStatusSnapshot) -> AggregateState {
    let attention = snapshot.last_error.is_some()
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
    }) {
        AggregateState::Syncing
    } else {
        AggregateState::Idle
    }
}

/// The pure builder: snapshot (+ `now` for relative time) → menu model.
pub fn build_menu_model(snapshot: &SyncStatusSnapshot, now: DateTime<Utc>) -> MenuModel {
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
            entries.push(MenuEntry::Info {
                id: format!("sync-file:{key}"),
                label,
            });
        }
        if total > MAX_FILE_ROWS {
            entries.push(MenuEntry::Info {
                id: "sync-file-overflow".into(),
                label: format!("… and {} more", total - MAX_FILE_ROWS),
            });
        }
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
        aggregate: aggregate(snapshot),
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
        let model = build_menu_model(&SyncStatusSnapshot::default(), now());
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
        let model = build_menu_model(&snapshot(&refs), now());
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
        let model = build_menu_model(&snapshot(&refs), now());
        assert!(!labels(&model).iter().any(|l| l.contains("more")));
    }

    #[test]
    fn pause_label_flips_and_aggregate_shows_paused() {
        let mut s = snapshot(&[("P/a.penpot", FileState::Synced)]);
        s.paused = true;
        let model = build_menu_model(&s, now());
        assert_eq!(pause_label(&model), "Resume syncing");
        assert_eq!(model.aggregate, AggregateState::Paused);
        s.paused = false;
        let model = build_menu_model(&s, now());
        assert_eq!(pause_label(&model), "Pause syncing");
        assert_eq!(model.aggregate, AggregateState::Idle);
    }

    #[test]
    fn aggregate_escalation_order() {
        // Syncing when anything is in flight.
        let s = snapshot(&[("P/a.penpot", FileState::Exporting)]);
        assert_eq!(build_menu_model(&s, now()).aggregate, AggregateState::Syncing);
        // Attention beats paused AND syncing.
        let mut s = snapshot(&[
            ("P/a.penpot", FileState::Pending),
            (
                "P/b.penpot",
                FileState::Conflict { copy_path: "P/b.conflict-1.penpot".into() },
            ),
        ]);
        s.paused = true;
        assert_eq!(build_menu_model(&s, now()).aggregate, AggregateState::Attention);
        // Daemon-level last_error alone is attention-worthy.
        let mut s = snapshot(&[("P/a.penpot", FileState::Synced)]);
        s.last_error = Some("rpc down".into());
        assert_eq!(build_menu_model(&s, now()).aggregate, AggregateState::Attention);
    }

    #[test]
    fn last_error_row_is_shown_and_truncated() {
        let mut s = snapshot(&[]);
        s.last_error = Some("e".repeat(200));
        let model = build_menu_model(&s, now());
        let rows = labels(&model);
        let err_row = rows.iter().find(|l| l.starts_with("Error: ")).unwrap();
        assert!(err_row.chars().count() <= "Error: ".chars().count() + 60);
        assert!(err_row.ends_with('…'));
    }

    #[test]
    fn last_sync_uses_relative_time() {
        let mut s = snapshot(&[]);
        s.last_sync_at = Some("2026-07-13T11:53:00Z".into());
        let model = build_menu_model(&s, now());
        assert_eq!(labels(&model)[0], "Last sync: 7m ago");
    }

    #[test]
    fn toggle_and_quit_ids_are_stable() {
        // The wiring layer matches on these ids; make sure the model always
        // emits exactly one of each.
        let model = build_menu_model(&SyncStatusSnapshot::default(), now());
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
