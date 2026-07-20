//! N3 lighttable board listing: the `/__api/vault/boards` payload.
//!
//! Joins the index's `kind = 'board'` rows (`db::all_boards`) with two disk
//! sources the vault already owns — the sync manifest (project name +
//! recency, `.penpot-sync.json`) and each file's N2 exports dir
//! (`<name>.exports/.exports-state.json`, which maps a board id to the file
//! stem its render was written under). Every card carries the EXACT verified
//! `/#/workspace?…` deep link and, when a render exists, a thumbnail URL the
//! app's `/__api/vault/thumb` route serves; otherwise `thumb` is `null` and
//! the page shows N2's degraded placeholder.
//!
//! **D2 gap fix:** a file created through `/__home`'s front door has a page
//! but no board — `db::all_boards` never yields a row for it, so it used to
//! be invisible (and, since the per-card action buttons are the only way to
//! rename/duplicate/move/delete a file, unreachable too). `assemble_cards`
//! now emits a placeholder [`BoardCard`] (`kind = CardKind::File`) for every
//! manifest entry that has zero indexed board rows, straight from the
//! manifest + a cheap disk peek at the file's own page ids — never the
//! Penpot DB (this crate stays disk-only).
//!
//! All of this is derived, read-only state (invariant 1): nothing here writes,
//! and everything is rebuilt from disk alone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::db::BoardRow;
use crate::query::workspace_deep_link;

/// Directory suffix for a file's rendered exports (mirrors
/// `board_export::EXPORTS_DIR_SUFFIX`; duplicated so vault-index needs no
/// dependency on the exporter crate).
pub const EXPORTS_DIR_SUFFIX: &str = ".exports";
/// State record inside an exports dir (mirrors `board_export::STATE_FILE_NAME`).
pub const EXPORTS_STATE_FILE: &str = ".exports-state.json";
const PENPOT_DIR_SUFFIX: &str = ".penpot";

/// `client-x/home.penpot` → `client-x/home.exports` (mirrors
/// `board_export::exports_rel_path`).
pub fn exports_rel_path(penpot_rel: &str) -> String {
    let stem = penpot_rel
        .strip_suffix(PENPOT_DIR_SUFFIX)
        .unwrap_or(penpot_rel);
    format!("{stem}{EXPORTS_DIR_SUFFIX}")
}

/// Sort order for the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// Newest sync first (the default lighttable order).
    Recency,
    /// Board name A→Z.
    Name,
}

impl Sort {
    /// Parse the `sort=` query param; unknown/absent → recency.
    pub fn parse(s: Option<&str>) -> Sort {
        match s {
            Some("name") => Sort::Name,
            _ => Sort::Recency,
        }
    }
}

/// Per-file metadata joined onto every board (from the manifest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    pub project: String,
    /// The project's real Penpot id (UUID), distinct from its display name —
    /// carried through to `BoardCard.project_id` so the home page can pass a
    /// real `projectId` to `/__api/vault/manage/duplicate` without a lookup.
    pub project_id: String,
    pub rel_path: String,
    /// RFC 3339 UTC `lastSyncedAt` — the recency key.
    pub last_synced_at: String,
}

/// Distinguishes a real board card from the D2-gap-fix placeholder card for a
/// file that has zero indexed boards yet. Carried explicitly (rather than
/// left for the page to guess from `boardId`'s synthetic `file:` prefix or a
/// null `thumb`) so the home page can render — and gate behavior like
/// Peek's "Present" — off one unambiguous field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CardKind {
    Board,
    File,
}

/// One lighttable card, serialized camelCase for the HTTP API.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BoardCard {
    pub kind: CardKind,
    pub file_id: String,
    /// Empty when `kind == File` and no page id could be read off disk (see
    /// [`first_page_id`]) — the deep link then omits `page-id` rather than
    /// carrying a guessed/invalid one.
    pub page_id: String,
    /// The board's real id for `kind == Board`; a synthetic `file:<fileId>`
    /// key for `kind == File` (there is no board to key on, but the page's
    /// keyed diff/patch grid still needs one stable, globally-unique id per
    /// card).
    pub board_id: String,
    pub name: String,
    pub project: String,
    /// The project's real Penpot id — see `FileMeta::project_id`.
    pub project_id: String,
    pub rel_path: String,
    pub last_synced_at: String,
    /// The exact verified `/#/workspace?team-id&file-id&page-id` deep link.
    pub deep_link: String,
    /// Thumbnail URL served by `/__api/vault/thumb`, or `null` when no render
    /// exists yet (the page renders N2's degraded placeholder) — always
    /// `null` for `kind == File` (there is no board to render).
    pub thumb: Option<String>,
}

/// The whole listing payload.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BoardListing {
    pub count: usize,
    /// Distinct project names present (for the filter control), sorted.
    pub projects: Vec<String>,
    pub boards: Vec<BoardCard>,
}

/// The thumbnail URL for a board, pointing at the app's `/__api/vault/thumb`
/// route. `rel` (the `.penpot` path) is percent-encoded; the serving side
/// re-resolves the actual file stem from the trusted exports-state record, so
/// the only client inputs are a within-vault `.penpot` path and a board uuid.
pub fn thumb_url(rel_path: &str, board_id: &str) -> String {
    format!(
        "/__api/vault/thumb?rel={}&board={}&fmt=png",
        percent_encode(rel_path),
        percent_encode(board_id)
    )
}

/// Percent-encode a query-component value: keep RFC 3986 unreserved bytes,
/// encode everything else (including `/`, spaces, and all multibyte UTF-8).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Pure assembly: join board rows with per-file metadata, resolve each card's
/// thumbnail via `thumb_for(owner_id, board_id) -> Option<thumb_url>`, fill in
/// a placeholder card for every manifest entry with zero indexed boards via
/// `first_page_id(owner_id) -> Option<page_id>`, filter by project, and sort.
/// Deterministic for a given index — a rebuilt index yields a byte-identical
/// listing.
///
/// `thumb_for` returns `Some(url)` iff a render exists for that board; the
/// HTTP layer supplies a closure backed by the exports-state stem map + disk.
/// `first_page_id` returns the page id to deep-link a boardless file's
/// placeholder card into (see [`first_page_id`] the disk-facing helper of the
/// same name); the HTTP layer supplies a closure backed by that function.
pub fn assemble_cards(
    rows: &[BoardRow],
    meta: &BTreeMap<String, FileMeta>,
    team_id: &str,
    thumb_for: impl Fn(&str, &str) -> Option<String>,
    first_page_id: impl Fn(&str) -> Option<String>,
    project_filter: Option<&str>,
    sort: Sort,
) -> BoardListing {
    // Distinct projects across the WHOLE manifest — not just files that have
    // an indexed board — so the filter control (and the D2 project picker
    // parity it backs) offers a project the instant one of its files exists
    // on disk, even before that file has a single board. This was the same
    // bug in a different guise: `"projects":[]` despite tracked files.
    let mut projects: Vec<String> = meta.values().map(|m| m.project.clone()).collect();
    projects.sort();
    projects.dedup();

    let mut cards: Vec<BoardCard> = rows
        .iter()
        .filter_map(|row| {
            let m = meta.get(&row.owner_id)?;
            if let Some(f) = project_filter {
                if m.project != f {
                    return None;
                }
            }
            Some(BoardCard {
                kind: CardKind::Board,
                deep_link: workspace_deep_link(
                    team_id,
                    &row.file_id,
                    (!row.page_id.is_empty()).then_some(row.page_id.as_str()),
                ),
                thumb: thumb_for(&row.owner_id, &row.board_id),
                file_id: row.file_id.clone(),
                page_id: row.page_id.clone(),
                board_id: row.board_id.clone(),
                name: row.name.clone(),
                project: m.project.clone(),
                project_id: m.project_id.clone(),
                rel_path: m.rel_path.clone(),
                last_synced_at: m.last_synced_at.clone(),
            })
        })
        .collect();

    // Requirement 1 (the D2 gap fix): every manifest entry appears, even with
    // zero indexed boards. Every owner already covered by a board card above
    // is skipped here — this never duplicates, and a file that later gains a
    // board simply stops qualifying for this loop on the next listing.
    let owners_with_boards: BTreeSet<&str> = rows.iter().map(|r| r.owner_id.as_str()).collect();
    for (owner_id, m) in meta {
        if owners_with_boards.contains(owner_id.as_str()) {
            continue;
        }
        if let Some(f) = project_filter {
            if m.project != f {
                continue;
            }
        }
        let page_id = first_page_id(owner_id).unwrap_or_default();
        cards.push(BoardCard {
            kind: CardKind::File,
            deep_link: workspace_deep_link(
                team_id,
                owner_id,
                (!page_id.is_empty()).then_some(page_id.as_str()),
            ),
            thumb: None,
            file_id: owner_id.clone(),
            page_id,
            board_id: file_card_key(owner_id),
            name: file_display_name(&m.rel_path),
            project: m.project.clone(),
            project_id: m.project_id.clone(),
            rel_path: m.rel_path.clone(),
            last_synced_at: m.last_synced_at.clone(),
        });
    }

    match sort {
        // Newest first; ties broken by (rel_path, board_id) for determinism.
        Sort::Recency => cards.sort_by(|a, b| {
            b.last_synced_at
                .cmp(&a.last_synced_at)
                .then_with(|| a.rel_path.cmp(&b.rel_path))
                .then_with(|| a.board_id.cmp(&b.board_id))
        }),
        // Case-insensitive name; ties broken deterministically.
        Sort::Name => cards.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.rel_path.cmp(&b.rel_path))
                .then_with(|| a.board_id.cmp(&b.board_id))
        }),
    }

    BoardListing { count: cards.len(), projects, boards: cards }
}

/// The synthetic, globally-unique `board_id` a placeholder file card carries.
/// Never collides with a real board uuid (those never contain `:`).
fn file_card_key(file_id: &str) -> String {
    format!("file:{file_id}")
}

/// The `.penpot` basename (without the extension) — the file's display name
/// on its placeholder card, since a boardless file has no board name to show
/// (mirrors `palette.rs`'s helper of the same purpose).
fn file_display_name(rel_path: &str) -> String {
    let base = rel_path.rsplit('/').next().unwrap_or(rel_path);
    base.strip_suffix(PENPOT_DIR_SUFFIX).unwrap_or(base).to_string()
}

// ---------------------------------------------------------------------------
// Disk-facing helpers (exports-state stem map + thumbnail resolution)
// ---------------------------------------------------------------------------

/// The subset of `.exports-state.json` we read: board id → file stem.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportsStateLite {
    #[serde(default)]
    boards: Vec<BoardStemRecord>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BoardStemRecord {
    object_id: String,
    file_stem: String,
}

/// Load a `board_id → file_stem` map from a file's exports dir. Missing /
/// malformed state (no render yet) → empty map; never an error (the state is
/// disposable bookkeeping).
pub fn load_stem_map(vault_root: &Path, penpot_rel: &str) -> BTreeMap<String, String> {
    let state_path = vault_root
        .join(exports_rel_path(penpot_rel))
        .join(EXPORTS_STATE_FILE);
    let Ok(raw) = std::fs::read(&state_path) else {
        return BTreeMap::new();
    };
    let Ok(state) = serde_json::from_slice::<ExportsStateLite>(&raw) else {
        return BTreeMap::new();
    };
    state
        .boards
        .into_iter()
        .map(|b| (b.object_id, b.file_stem))
        .collect()
}

/// The lexicographically-first page id under a file's own `files/<file-id>/
/// pages/` dir (each page's normalized JSON is named `<page-id>.json` there —
/// see `crates/vault-index/src/extract.rs`'s module docs for the verified
/// tree layout). Backs a placeholder file card's deep link.
///
/// This crate is disk-only by design and never parses `files/<file-id>.json`'s
/// `data.pages` (the array that carries Penpot's actual page ORDER) — UUIDs
/// have no natural order of their own, but the case this exists for (a file
/// just created through `/__home`) has exactly one page, so "lexicographically
/// first" only ever matters as a deterministic tiebreak, never a real ordering
/// choice. `None` when the dir doesn't exist, is empty, or can't be read — the
/// caller (`assemble_cards`) must treat that as genuinely unknown and omit
/// `page-id` from the deep link rather than guess one.
pub fn first_page_id(vault_root: &Path, penpot_rel: &str, file_id: &str) -> Option<String> {
    let pages_dir = vault_root.join(penpot_rel).join("files").join(file_id).join("pages");
    let mut ids: Vec<String> = std::fs::read_dir(&pages_dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let name = e.file_name();
            name.to_str().and_then(|n| n.strip_suffix(".json")).map(str::to_string)
        })
        .collect();
    ids.sort();
    ids.into_iter().next()
}

/// Resolve the on-disk path of a board's thumbnail (`<stem>.<ext>` inside the
/// file's exports dir), returning it only when the file actually exists.
/// `ext` is a plain extension (`png`/`svg`). Used by the app's thumb route.
pub fn resolve_thumb_path(
    vault_root: &Path,
    penpot_rel: &str,
    board_id: &str,
    ext: &str,
) -> Option<std::path::PathBuf> {
    let stem = load_stem_map(vault_root, penpot_rel).remove(board_id)?;
    let path = vault_root
        .join(exports_rel_path(penpot_rel))
        .join(format!("{stem}.{ext}"));
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(owner: &str, file: &str, page: &str, board: &str, name: &str, rel: &str) -> BoardRow {
        BoardRow {
            owner_id: owner.into(),
            file_id: file.into(),
            page_id: page.into(),
            board_id: board.into(),
            name: name.into(),
            rel_path: rel.into(),
        }
    }

    fn meta(project: &str, rel: &str, at: &str) -> FileMeta {
        FileMeta {
            project: project.into(),
            // Tests don't assert on the id itself; reusing the name keeps
            // every existing call site unchanged.
            project_id: format!("{project}-id"),
            rel_path: rel.into(),
            last_synced_at: at.into(),
        }
    }

    #[test]
    fn exports_rel_path_mirrors_the_exporter() {
        assert_eq!(exports_rel_path("c/home.penpot"), "c/home.exports");
        assert_eq!(exports_rel_path("root.penpot"), "root.exports");
        assert_eq!(exports_rel_path("odd"), "odd.exports");
    }

    #[test]
    fn thumb_url_percent_encodes_path_and_board() {
        assert_eq!(
            thumb_url("Client A/home page.penpot", "b-1"),
            "/__api/vault/thumb?rel=Client%20A%2Fhome%20page.penpot&board=b-1&fmt=png"
        );
        // Unicode + reserved chars are fully encoded.
        assert_eq!(
            thumb_url("Città/più.penpot", "x"),
            "/__api/vault/thumb?rel=Citt%C3%A0%2Fpi%C3%B9.penpot&board=x&fmt=png"
        );
    }

    #[test]
    fn assemble_joins_meta_builds_deeplink_and_thumb() {
        let rows = vec![
            row("f1", "f1", "p1", "bA", "Hero", "P/a.penpot"),
            row("f1", "f1", "p1", "bB", "Footer", "P/a.penpot"),
        ];
        let mut m = BTreeMap::new();
        m.insert("f1".to_string(), meta("Proj", "P/a.penpot", "2026-07-14T10:00:00Z"));
        // Only bA has a render.
        let listing = assemble_cards(
            &rows,
            &m,
            "team-7",
            |_owner, board| (board == "bA").then(|| thumb_url("P/a.penpot", board)),
            |_owner| None,
            None,
            Sort::Name,
        );
        assert_eq!(listing.count, 2);
        assert_eq!(listing.projects, vec!["Proj".to_string()]);
        // Name sort: Footer before Hero.
        assert_eq!(listing.boards[0].name, "Footer");
        assert_eq!(listing.boards[1].name, "Hero");
        // Deep link is the exact verified shape.
        assert_eq!(
            listing.boards[1].deep_link,
            "/#/workspace?team-id=team-7&file-id=f1&page-id=p1"
        );
        // Thumb present for bA, degraded (None) for bB.
        let hero = listing.boards.iter().find(|c| c.board_id == "bA").unwrap();
        let footer = listing.boards.iter().find(|c| c.board_id == "bB").unwrap();
        assert_eq!(hero.thumb.as_deref(), Some("/__api/vault/thumb?rel=P%2Fa.penpot&board=bA&fmt=png"));
        assert_eq!(footer.thumb, None);
        // The card carries the project's real id, not just its display name —
        // the home page needs it for the duplicate verb's `projectId`.
        assert_eq!(hero.project_id, "Proj-id");
    }

    #[test]
    fn assemble_filters_by_project_but_lists_all_projects() {
        let rows = vec![
            row("f1", "f1", "p1", "b1", "One", "A/one.penpot"),
            row("f2", "f2", "p2", "b2", "Two", "B/two.penpot"),
        ];
        let mut m = BTreeMap::new();
        m.insert("f1".to_string(), meta("Alpha", "A/one.penpot", "2026-07-14T10:00:00Z"));
        m.insert("f2".to_string(), meta("Beta", "B/two.penpot", "2026-07-14T11:00:00Z"));
        let listing = assemble_cards(&rows, &m, "t", |_, _| None, |_| None, Some("Beta"), Sort::Recency);
        assert_eq!(listing.count, 1);
        assert_eq!(listing.boards[0].project, "Beta");
        // The control still offers both projects.
        assert_eq!(listing.projects, vec!["Alpha".to_string(), "Beta".to_string()]);
    }

    #[test]
    fn recency_sort_newest_first_with_deterministic_tiebreak() {
        let rows = vec![
            row("f1", "f1", "p1", "bx", "X", "A/a.penpot"),
            row("f2", "f2", "p2", "by", "Y", "B/b.penpot"),
            row("f2", "f2", "p2", "bz", "Z", "B/b.penpot"),
        ];
        let mut m = BTreeMap::new();
        m.insert("f1".to_string(), meta("P", "A/a.penpot", "2026-07-14T09:00:00Z"));
        m.insert("f2".to_string(), meta("P", "B/b.penpot", "2026-07-14T12:00:00Z"));
        let listing = assemble_cards(&rows, &m, "t", |_, _| None, |_| None, None, Sort::Recency);
        // f2 (12:00) boards first, tie broken by board_id (by < bz), then f1.
        assert_eq!(
            listing
                .boards
                .iter()
                .map(|c| c.board_id.as_str())
                .collect::<Vec<_>>(),
            vec!["by", "bz", "bx"]
        );
    }

    #[test]
    fn board_without_manifest_entry_is_dropped() {
        // A board whose owner vanished from the manifest (stale index row mid
        // sync): no meta → not listed (never point at a nonexistent file).
        let rows = vec![row("ghost", "ghost", "p", "b", "N", "X/x.penpot")];
        let listing =
            assemble_cards(&rows, &BTreeMap::new(), "t", |_, _| None, |_| None, None, Sort::Recency);
        assert_eq!(listing.count, 0);
    }

    /// The D2 gap fix, the core assertion: a manifest entry with zero indexed
    /// boards still produces a card (kind = File), carries the project id the
    /// duplicate verb needs, and a synthetic-but-unique board_id.
    #[test]
    fn file_with_no_boards_gets_a_placeholder_card() {
        let m: BTreeMap<String, FileMeta> =
            [("f1".to_string(), meta("Client Redesign", "Client Redesign/Homepage.penpot", "2026-07-19T10:00:00Z"))]
                .into_iter()
                .collect();
        let listing = assemble_cards(&[], &m, "team-7", |_, _| None, |owner| {
            assert_eq!(owner, "f1");
            Some("page-1".to_string())
        }, None, Sort::Recency);
        assert_eq!(listing.count, 1);
        let card = &listing.boards[0];
        assert_eq!(card.kind, CardKind::File);
        assert_eq!(card.file_id, "f1");
        assert_eq!(card.name, "Homepage");
        assert_eq!(card.project, "Client Redesign");
        assert_eq!(card.project_id, "Client Redesign-id");
        assert_eq!(card.thumb, None);
        assert_eq!(card.board_id, "file:f1");
        assert_eq!(
            card.deep_link,
            "/#/workspace?team-id=team-7&file-id=f1&page-id=page-1"
        );
    }

    /// When `first_page_id` genuinely can't determine a page (empty/missing
    /// pages dir), the card still appears — its deep link just omits
    /// `page-id` (an established, working shape elsewhere in this crate)
    /// rather than carrying a guessed one.
    #[test]
    fn file_with_no_boards_and_no_page_id_still_gets_a_card() {
        let m: BTreeMap<String, FileMeta> =
            [("f1".to_string(), meta("P", "P/new.penpot", "2026-07-19T10:00:00Z"))].into_iter().collect();
        let listing = assemble_cards(&[], &m, "team-7", |_, _| None, |_| None, None, Sort::Recency);
        assert_eq!(listing.count, 1);
        let card = &listing.boards[0];
        assert_eq!(card.kind, CardKind::File);
        assert_eq!(card.page_id, "");
        assert_eq!(card.deep_link, "/#/workspace?team-id=team-7&file-id=f1");
    }

    /// A file WITH boards must keep producing exactly its board card(s) —
    /// never an extra placeholder file card alongside them.
    #[test]
    fn file_with_boards_is_not_also_given_a_placeholder_card() {
        let rows = vec![
            row("f1", "f1", "p1", "bA", "Hero", "P/a.penpot"),
            row("f1", "f1", "p1", "bB", "Footer", "P/a.penpot"),
        ];
        let m: BTreeMap<String, FileMeta> =
            [("f1".to_string(), meta("Proj", "P/a.penpot", "2026-07-14T10:00:00Z"))].into_iter().collect();
        let listing = assemble_cards(&rows, &m, "t", |_, _| None, |_| panic!("must not be called"), None, Sort::Recency);
        assert_eq!(listing.count, 2, "no extra placeholder card for a file that already has boards");
        assert!(listing.boards.iter().all(|c| c.kind == CardKind::Board));
    }

    /// A mix: one file with boards, one without — the boardless file still
    /// gets exactly one card, the other keeps its board cards, none dropped
    /// or duplicated.
    #[test]
    fn mixed_manifest_yields_board_cards_and_one_placeholder_each() {
        let rows = vec![row("f1", "f1", "p1", "b1", "Cover", "A/one.penpot")];
        let m: BTreeMap<String, FileMeta> = [
            ("f1".to_string(), meta("Alpha", "A/one.penpot", "2026-07-14T10:00:00Z")),
            ("f2".to_string(), meta("Alpha", "A/two.penpot", "2026-07-14T11:00:00Z")),
        ]
        .into_iter()
        .collect();
        let listing = assemble_cards(&rows, &m, "t", |_, _| None, |_| None, None, Sort::Recency);
        assert_eq!(listing.count, 2);
        let board = listing.boards.iter().find(|c| c.file_id == "f1").unwrap();
        assert_eq!(board.kind, CardKind::Board);
        assert_eq!(board.board_id, "b1");
        let placeholder = listing.boards.iter().find(|c| c.file_id == "f2").unwrap();
        assert_eq!(placeholder.kind, CardKind::File);
        assert_eq!(placeholder.board_id, "file:f2");
    }

    /// Requirement 3: `projects` must include a project whose files have no
    /// boards at all — the exact shape of the observed bug (`"projects":[]`
    /// despite tracked files) in the project-listing half of the payload.
    #[test]
    fn projects_include_a_project_whose_files_have_no_boards() {
        let m: BTreeMap<String, FileMeta> =
            [("f1".to_string(), meta("Client Redesign", "Client Redesign/Homepage.penpot", "2026-07-19T10:00:00Z"))]
                .into_iter()
                .collect();
        let listing = assemble_cards(&[], &m, "t", |_, _| None, |_| None, None, Sort::Recency);
        assert_eq!(listing.projects, vec!["Client Redesign".to_string()]);
    }

    /// The project filter applies to placeholder cards the same as board
    /// cards.
    #[test]
    fn project_filter_applies_to_placeholder_cards_too() {
        let m: BTreeMap<String, FileMeta> = [
            ("f1".to_string(), meta("Alpha", "A/one.penpot", "2026-07-14T10:00:00Z")),
            ("f2".to_string(), meta("Beta", "B/two.penpot", "2026-07-14T11:00:00Z")),
        ]
        .into_iter()
        .collect();
        let listing = assemble_cards(&[], &m, "t", |_, _| None, |_| None, Some("Beta"), Sort::Recency);
        assert_eq!(listing.count, 1);
        assert_eq!(listing.boards[0].file_id, "f2");
    }

    #[test]
    fn first_page_id_reads_the_lexicographically_first_page_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let pages = root.join("P/a.penpot/files/f1/pages");
        std::fs::create_dir_all(&pages).unwrap();
        std::fs::write(pages.join("page-2.json"), b"{}").unwrap();
        std::fs::write(pages.join("page-1.json"), b"{}").unwrap();
        // A stray non-.json entry must be ignored, not chosen.
        std::fs::write(pages.join("notes.txt"), b"x").unwrap();
        assert_eq!(first_page_id(root, "P/a.penpot", "f1").as_deref(), Some("page-1"));
    }

    #[test]
    fn first_page_id_missing_dir_is_none_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(first_page_id(tmp.path(), "P/a.penpot", "f1"), None);
    }

    #[test]
    fn first_page_id_empty_dir_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("P/a.penpot/files/f1/pages")).unwrap();
        assert_eq!(first_page_id(tmp.path(), "P/a.penpot", "f1"), None);
    }

    #[test]
    fn stem_map_and_thumb_resolution_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let exports = root.join("P/home.exports");
        std::fs::create_dir_all(&exports).unwrap();
        std::fs::write(
            exports.join(EXPORTS_STATE_FILE),
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 1,
                "fileId": "f1",
                "renderedFromHash": "h",
                "renderedAt": "2026-07-14T00:00:00Z",
                "boards": [
                    {"objectId": "b1", "pageId": "p1", "name": "Cover", "fileStem": "Cover"},
                    {"objectId": "b2", "pageId": "p1", "name": "A/B", "fileStem": "A-B"}
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(exports.join("Cover.png"), b"png").unwrap();
        // b2's png intentionally missing (render pending for that stem).

        let stems = load_stem_map(root, "P/home.penpot");
        assert_eq!(stems.get("b1").map(String::as_str), Some("Cover"));
        assert_eq!(stems.get("b2").map(String::as_str), Some("A-B"));

        // b1 resolves (file exists); b2 does not (missing png); unknown board none.
        assert_eq!(
            resolve_thumb_path(root, "P/home.penpot", "b1", "png"),
            Some(exports.join("Cover.png"))
        );
        assert_eq!(resolve_thumb_path(root, "P/home.penpot", "b2", "png"), None);
        assert_eq!(resolve_thumb_path(root, "P/home.penpot", "nope", "png"), None);
    }

    #[test]
    fn stem_map_missing_state_is_empty_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_stem_map(tmp.path(), "P/home.penpot").is_empty());
    }
}
