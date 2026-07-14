//! N4 quick-open palette ranking: a fuzzy matcher over the vault's
//! projects / files / boards, yielding ranked hits each carrying a `kind`
//! and the EXACT deep link the palette's Enter key navigates to.
//!
//! This is the gateable value of the palette (PLAN2.md N4): a fuzzy query
//! must rank the intended board first and the Enter payload must be its exact
//! `/#/workspace?…` deep link. The ranking is a pure function over the same
//! index rows + manifest metadata the lighttable listing uses (`boards.rs`),
//! so it is deterministic and rebuilt-from-disk stable (invariant 1), and it
//! needs no running stack to test.
//!
//! Scoring is an fzf-style subsequence matcher: the query must appear as an
//! (ordered, not necessarily contiguous) subsequence of the candidate; the
//! score rewards contiguous runs, word-boundary starts, and a match at the
//! very start of the label, so "chkout" ranks "Checkout Button" above
//! "Search checkout log", and an exact/prefix hit always wins.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::boards::FileMeta;
use crate::db::BoardRow;
use crate::query::workspace_deep_link;

/// The kind of a palette hit — drives the icon/label the page renders and,
/// with it, the verb set that applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PaletteKind {
    Project,
    File,
    Board,
}

impl PaletteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PaletteKind::Project => "project",
            PaletteKind::File => "file",
            PaletteKind::Board => "board",
        }
    }
}

/// One rankable palette item. `deep_link` is the exact URL Enter navigates to;
/// `rel_path`/`board_id`/`file_id`/`page_id` back the verbs (reveal, export,
/// copy-link, peek).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaletteItem {
    pub kind: PaletteKind,
    /// The primary matched string (board name / file basename / project name).
    pub label: String,
    /// Secondary context shown dimmed (e.g. `project · file`).
    pub sublabel: String,
    pub deep_link: String,
    pub rel_path: String,
    pub file_id: String,
    pub page_id: String,
    pub board_id: String,
    pub project: String,
}

/// A ranked palette hit: the item plus its match score (higher = better).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaletteHit {
    #[serde(flatten)]
    pub item: PaletteItem,
    pub score: i32,
}

/// The `.penpot` basename (without the extension) — the file's display name.
fn file_display_name(rel_path: &str) -> String {
    let base = rel_path.rsplit('/').next().unwrap_or(rel_path);
    base.strip_suffix(".penpot").unwrap_or(base).to_string()
}

/// Assemble the full palette corpus from the index's board rows joined with
/// the manifest metadata: one item per board, one per distinct file, one per
/// distinct project. Deterministic order (projects, then files, then boards,
/// each by their identifying key) so a rebuilt index yields identical items.
pub fn assemble_items(
    rows: &[BoardRow],
    meta: &BTreeMap<String, FileMeta>,
    team_id: &str,
) -> Vec<PaletteItem> {
    let mut items: Vec<PaletteItem> = Vec::new();

    // Projects (distinct, sorted).
    let mut projects: Vec<String> = Vec::new();
    for m in meta.values() {
        if !projects.contains(&m.project) {
            projects.push(m.project.clone());
        }
    }
    projects.sort();
    for p in &projects {
        items.push(PaletteItem {
            kind: PaletteKind::Project,
            label: p.clone(),
            sublabel: "project".to_string(),
            // A project has no single canvas; Enter drops to the dashboard
            // escape hatch (the palette page can also just filter the home).
            deep_link: "/#/dashboard/recent".to_string(),
            rel_path: String::new(),
            file_id: String::new(),
            page_id: String::new(),
            board_id: String::new(),
            project: p.clone(),
        });
    }

    // Files (one per manifest entry, sorted by rel_path for determinism).
    // A file's page-less deep link opens the file at its default page.
    let mut file_ids: Vec<(&String, &FileMeta)> = meta.iter().collect();
    file_ids.sort_by(|a, b| a.1.rel_path.cmp(&b.1.rel_path));
    for (file_id, m) in file_ids {
        items.push(PaletteItem {
            kind: PaletteKind::File,
            label: file_display_name(&m.rel_path),
            sublabel: m.project.clone(),
            deep_link: workspace_deep_link(team_id, file_id, None),
            rel_path: m.rel_path.clone(),
            file_id: file_id.clone(),
            page_id: String::new(),
            board_id: String::new(),
            project: m.project.clone(),
        });
    }

    // Boards (rows are already ordered by (rel_path, board_id) from the db).
    for row in rows {
        let Some(m) = meta.get(&row.owner_id) else { continue };
        items.push(PaletteItem {
            kind: PaletteKind::Board,
            label: row.name.clone(),
            sublabel: format!("{} · {}", m.project, file_display_name(&m.rel_path)),
            deep_link: workspace_deep_link(
                team_id,
                &row.file_id,
                (!row.page_id.is_empty()).then_some(row.page_id.as_str()),
            ),
            rel_path: m.rel_path.clone(),
            file_id: row.file_id.clone(),
            page_id: row.page_id.clone(),
            board_id: row.board_id.clone(),
            project: m.project.clone(),
        });
    }

    items
}

// --- scoring ---------------------------------------------------------------

// Score weights (tuned so exact/prefix wins, contiguity and word-starts
// matter, and a board beats a file/project on an equal textual match so the
// palette lands you on a canvas by default).
const SCORE_MATCH: i32 = 16; // per matched character
const BONUS_CONSECUTIVE: i32 = 18; // adjacent to the previous match
const BONUS_WORD_START: i32 = 22; // match begins a word (after sep or caps)
const BONUS_LABEL_START: i32 = 30; // match is at index 0 of the label
const PENALTY_LEADING_GAP: i32 = -3; // per unmatched char before the first hit
const PENALTY_GAP: i32 = -2; // per gap between matched chars
const KIND_BONUS_BOARD: i32 = 8;
const KIND_BONUS_FILE: i32 = 4;
const KIND_BONUS_PROJECT: i32 = 0;

fn kind_bonus(kind: PaletteKind) -> i32 {
    match kind {
        PaletteKind::Board => KIND_BONUS_BOARD,
        PaletteKind::File => KIND_BONUS_FILE,
        PaletteKind::Project => KIND_BONUS_PROJECT,
    }
}

fn is_word_boundary(prev: Option<char>, cur: char) -> bool {
    match prev {
        None => true,
        Some(p) => {
            let sep = !p.is_alphanumeric();
            let camel = p.is_lowercase() && cur.is_uppercase();
            sep || camel
        }
    }
}

/// Fuzzy-score `query` against `candidate`. Returns `None` when `query` is not
/// an ordered subsequence of `candidate` (case-insensitive). Higher is better.
/// Greedy earliest-match with the standard fzf bonuses — deterministic and
/// O(len(candidate)). An empty query scores 0 (everything matches equally).
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    let q: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).collect();
    if q.is_empty() {
        return Some(0);
    }
    let cand: Vec<char> = candidate.chars().collect();
    let ql: Vec<char> = q.iter().map(|c| c.to_ascii_lowercase()).collect();

    let mut score = 0i32;
    let mut qi = 0usize;
    let mut prev_match: Option<usize> = None;
    let mut first_match: Option<usize> = None;

    for (ci, &cc) in cand.iter().enumerate() {
        if qi >= ql.len() {
            break;
        }
        if cc.to_ascii_lowercase() == ql[qi] {
            score += SCORE_MATCH;
            let prev_char = if ci > 0 { Some(cand[ci - 1]) } else { None };
            if ci == 0 {
                score += BONUS_LABEL_START;
            } else if is_word_boundary(prev_char, cc) {
                score += BONUS_WORD_START;
            }
            match prev_match {
                Some(pm) if pm + 1 == ci => score += BONUS_CONSECUTIVE,
                Some(pm) => score += PENALTY_GAP * (ci - pm - 1) as i32,
                None => {}
            }
            if first_match.is_none() {
                first_match = Some(ci);
                score += PENALTY_LEADING_GAP * ci as i32;
            }
            prev_match = Some(ci);
            qi += 1;
        }
    }

    if qi == ql.len() {
        // Prefer shorter candidates on an otherwise equal match (a tight hit).
        let len_penalty = (cand.len() as i32) / 8;
        Some(score - len_penalty)
    } else {
        None
    }
}

/// Rank the palette corpus against `query`, best first, capped at `limit`.
/// Matches on the label with a small fallback weight on the sublabel (so
/// "acme cover" can find the "Cover" board of project "Acme"). Ties break
/// deterministically by (kind priority, label, deep_link).
pub fn rank(items: &[PaletteItem], query: &str, limit: usize) -> Vec<PaletteHit> {
    let trimmed = query.trim();
    let mut hits: Vec<PaletteHit> = items
        .iter()
        .filter_map(|it| {
            let label_score = fuzzy_score(trimmed, &it.label);
            // Sublabel matches count for less (half weight, no kind bonus).
            let sub_score = fuzzy_score(trimmed, &it.sublabel).map(|s| s / 2 - 4);
            let best = match (label_score, sub_score) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            }?;
            Some(PaletteHit { score: best + kind_bonus(it.kind), item: it.clone() })
        })
        .collect();

    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| kind_rank(a.item.kind).cmp(&kind_rank(b.item.kind)))
            .then_with(|| a.item.label.to_lowercase().cmp(&b.item.label.to_lowercase()))
            .then_with(|| a.item.deep_link.cmp(&b.item.deep_link))
    });
    hits.truncate(limit);
    hits
}

/// Deterministic tiebreak priority (lower first): boards, then files, then
/// projects — so an equal-scoring board outranks a file of the same name.
fn kind_rank(kind: PaletteKind) -> u8 {
    match kind {
        PaletteKind::Board => 0,
        PaletteKind::File => 1,
        PaletteKind::Project => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brow(owner: &str, file: &str, page: &str, board: &str, name: &str) -> BoardRow {
        BoardRow {
            owner_id: owner.into(),
            file_id: file.into(),
            page_id: page.into(),
            board_id: board.into(),
            name: name.into(),
            rel_path: String::new(),
        }
    }

    fn meta(project: &str, rel: &str) -> FileMeta {
        FileMeta {
            project: project.into(),
            rel_path: rel.into(),
            last_synced_at: "2026-07-14T00:00:00Z".into(),
        }
    }

    #[test]
    fn subsequence_required_and_prefix_beats_scattered() {
        // "chkout" is a subsequence of both, but the contiguous/word-start
        // hit must win.
        let tight = fuzzy_score("chkout", "Checkout Button").unwrap();
        let loose = fuzzy_score("chkout", "Search checkout log").unwrap();
        assert!(tight > loose, "tight={tight} loose={loose}");
        // Not a subsequence → no match.
        assert_eq!(fuzzy_score("zzz", "Checkout"), None);
        // Empty query matches everything at 0.
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn exact_prefix_outranks_midword() {
        let pre = fuzzy_score("check", "Checkout").unwrap();
        let mid = fuzzy_score("check", "Recheck now").unwrap();
        assert!(pre > mid);
    }

    #[test]
    fn assemble_builds_projects_files_boards_with_deeplinks() {
        let rows = vec![
            brow("f1", "f1", "p1", "bA", "Hero"),
            brow("f1", "f1", "p1", "bB", "Checkout Button"),
        ];
        let mut m = BTreeMap::new();
        m.insert("f1".to_string(), meta("Acme", "Acme/home.penpot"));
        let items = assemble_items(&rows, &m, "team-7");
        // 1 project + 1 file + 2 boards.
        assert_eq!(items.len(), 4);
        let board = items.iter().find(|i| i.label == "Checkout Button").unwrap();
        assert_eq!(board.kind, PaletteKind::Board);
        assert_eq!(board.deep_link, "/#/workspace?team-id=team-7&file-id=f1&page-id=p1");
        let file = items.iter().find(|i| i.kind == PaletteKind::File).unwrap();
        assert_eq!(file.label, "home");
        assert_eq!(file.deep_link, "/#/workspace?team-id=team-7&file-id=f1");
        let proj = items.iter().find(|i| i.kind == PaletteKind::Project).unwrap();
        assert_eq!(proj.label, "Acme");
    }

    #[test]
    fn fuzzy_query_ranks_target_board_first_with_exact_deeplink() {
        // The gateable N4 assertion: a fuzzy query ranks the intended board
        // first and the Enter payload is its exact deep link.
        let rows = vec![
            brow("f1", "f1", "p1", "b1", "Home Hero"),
            brow("f1", "f1", "p1", "b2", "Checkout Button"),
            brow("f2", "f2", "p2", "b3", "Checkbox states"),
            brow("f2", "f2", "p2", "b4", "Footer"),
        ];
        let mut m = BTreeMap::new();
        m.insert("f1".to_string(), meta("Acme", "Acme/home.penpot"));
        m.insert("f2".to_string(), meta("Acme", "Acme/ui.penpot"));
        let items = assemble_items(&rows, &m, "t");
        let hits = rank(&items, "checkout", 10);
        assert_eq!(hits[0].item.label, "Checkout Button");
        assert_eq!(
            hits[0].item.deep_link,
            "/#/workspace?team-id=t&file-id=f1&page-id=p1"
        );
        assert_eq!(hits[0].item.kind, PaletteKind::Board);
    }

    #[test]
    fn board_outranks_equal_named_file_and_project() {
        // A board, a file and a project all literally named "Brand".
        let rows = vec![brow("f1", "f1", "p1", "b1", "Brand")];
        let mut m = BTreeMap::new();
        m.insert("f1".to_string(), meta("Brand", "Brand.penpot"));
        let items = assemble_items(&rows, &m, "t");
        let hits = rank(&items, "brand", 10);
        // Exact match on all three; board first (kind bonus + tiebreak).
        assert_eq!(hits[0].item.kind, PaletteKind::Board);
        assert!(hits[0].item.deep_link.contains("file-id=f1"));
    }

    #[test]
    fn empty_query_returns_everything_capped() {
        let rows = vec![brow("f1", "f1", "p1", "b1", "A"), brow("f1", "f1", "p1", "b2", "B")];
        let mut m = BTreeMap::new();
        m.insert("f1".to_string(), meta("P", "P/a.penpot"));
        let items = assemble_items(&rows, &m, "t");
        let hits = rank(&items, "  ", 2);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn ranking_is_deterministic_and_fast_at_vault_scale() {
        // 1000 boards across 100 files / 4 projects (the N1 torture shape).
        let projects = ["Acme", "Globex", "Initech", "Umbrella"];
        let mut rows = Vec::new();
        let mut m = BTreeMap::new();
        for f in 0..100 {
            let fid = format!("file-{f:03}");
            let proj = projects[f % projects.len()];
            m.insert(fid.clone(), meta(proj, &format!("{proj}/file-{f:03}.penpot")));
            for b in 0..10 {
                let name = if f == 73 && b == 4 {
                    "Checkout Button".to_string()
                } else {
                    format!("Board {f}-{b}")
                };
                rows.push(brow(&fid, &fid, &format!("page-{f}"), &format!("b-{f}-{b}"), &name));
            }
        }
        let items = assemble_items(&rows, &m, "team");
        assert!(items.len() >= 1000);

        let started = std::time::Instant::now();
        let hits = rank(&items, "checkout", 20);
        let elapsed = started.elapsed();

        assert_eq!(hits[0].item.label, "Checkout Button");
        assert_eq!(hits[0].item.file_id, "file-073");
        // Deterministic: a second run gives byte-identical ranking.
        assert_eq!(rank(&items, "checkout", 20), hits);
        // Latency budget: ranking the whole corpus is sub-50ms (single query).
        assert!(
            elapsed.as_millis() < 50,
            "palette ranking took {elapsed:?} over {} items",
            items.len()
        );
        eprintln!(
            "PALETTE_RANK_LATENCY items={} took_us={}",
            items.len(),
            started.elapsed().as_micros()
        );
    }
}
