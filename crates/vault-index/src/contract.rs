//! Milestone E1 — the CONTRACT extractor + version classifier.
//!
//! A package's versioned *surface* is its **contract**, not its
//! implementation (PLAN3 invariant 2, `docs/ecosystem-concept.md`
//! contract-over-implementation). This module reads the SAME normalized
//! binfile-v3 JSON the ledger hashes (`sync_core::semantic_view`, exactly as
//! [`crate::extract::extract_docs`] does) and emits, per **variant set**:
//!
//! - `variantNames`      — the component labels in the set;
//! - `exposedProperties` — the distinct `variantProperties[].name` axes;
//! - `tokensUsed`        — the set-union of `appliedTokens` paths over the
//!   set's main-instance subtrees;
//!
//! plus a library-level surface: exported color / typography names and the
//! `(path, $type)` token vocabulary from `tokens.json`.
//!
//! [`diff_contracts`] then labels a delta `patch` (implementation only),
//! `minor` (the contract grew), `major` (an element was removed / renamed /
//! `$type`-changed), or `migration` (the legacy→first-class variant-model
//! switch — a false positive the naive rule reads as a spurious minor).
//!
//! This is a straight port of the throwaway spike oracle in
//! `scripts/ecosystem-spike/{extract_contract,diff_contracts}.py`, with the
//! three caveats PLAN3 / the spike verdict demanded folded in:
//!
//! 1. **Two variant models coexist forever.** First-class variants carry
//!    `variantId` + `variantProperties`; the legacy convention is a group of
//!    components sharing a `path`, each `name` a variant. We handle both:
//!    *group* by `variantId` when present, else by shared `path`.
//! 2. **Identity is `name`/`path`, never `variantId`.** That uuid is remapped
//!    by import-as-new, so keying a set on it makes a re-imported package read
//!    as all-major noise. The spike keyed first-class sets on `variantId`;
//!    E1 keys every set on its uuid-free `path`, so `extract(A) ==
//!    extract(A')` across a settle / uuid churn.
//! 3. **Legacy→first-class migration is not growth.** A legacy set has an
//!    empty `exposedProperties`; migrating it to `variantProperties` makes the
//!    contract *look* like it grew. [`diff_contracts`] special-cases that as
//!    `migration`, never a spurious `minor`.
//!
//! Everything here is total (the [`crate::extract`] discipline): unknown
//! paths and malformed shapes are skipped, missing fields default to empty —
//! a malformed tree yields a thinner contract, never an error. It is a pure
//! function of the on-disk bytes, so it satisfies the core invariant for free:
//! delete any derived index and the contract rebuilds identically from disk.

use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Which variant model a set was recovered from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetKind {
    /// Penpot 2.7+ first-class variants: `variantId` + `variantProperties`.
    FirstClassVariant,
    /// The legacy convention: components sharing a `path`, each `name` a variant.
    PathConvention,
}

impl SetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SetKind::FirstClassVariant => "first-class-variant",
            SetKind::PathConvention => "path-convention",
        }
    }
}

/// One variant set's contract — the id-free, round-trip-stable surface a
/// consumer depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contract {
    /// Stable set identity: the shared `path`. NEVER the `variantId` (caveat 2).
    pub set: String,
    pub set_kind: SetKind,
    /// Component labels in the set (sorted, deduped).
    pub variant_names: Vec<String>,
    /// Distinct `variantProperties[].name` axes (sorted, deduped). Empty for
    /// legacy sets — the axes are undeclared on disk there.
    pub exposed_properties: Vec<String>,
    /// Set-union of `appliedTokens` paths over the set's main-instance
    /// subtrees (sorted, deduped).
    pub tokens_used: Vec<String>,
    pub component_count: usize,
}

/// A library-level exported token: a `tokens.json` leaf. `$value` is
/// implementation (a `$value`-only edit is `patch`); the `(path, $type)` pair
/// is the contract (a `$type` change is `major`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TokenExport {
    pub path: String,
    pub ty: String,
}

/// The whole file's contract: per-set contracts + the library-level surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryContract {
    /// Penpot file uuid (churns on import — excluded from every contract body).
    pub file_id: String,
    /// Per-set contracts, sorted by `(set_kind, set)`.
    pub contracts: Vec<Contract>,
    /// Exported library color names (sorted, deduped).
    pub exported_colors: Vec<String>,
    /// Exported typography names (sorted, deduped).
    pub exported_typographies: Vec<String>,
    /// Exported token `(path, $type)` leaves (sorted, deduped).
    pub exported_tokens: Vec<TokenExport>,
}

impl LibraryContract {
    /// The exported token *paths* alone (the appliedTokens vocabulary).
    pub fn exported_token_paths(&self) -> Vec<String> {
        self.exported_tokens.iter().map(|t| t.path.clone()).collect()
    }

    /// A JSON view whose per-set fields match the spike oracle's
    /// `extract_contract.py` shape (so the two can be compared field-by-field),
    /// plus the E1 library-level surface.
    pub fn to_json(&self) -> Value {
        json!({
            "fileId": self.file_id,
            "contractCount": self.contracts.len(),
            "contracts": self.contracts.iter().map(|c| json!({
                "set": c.set,
                "setKind": c.set_kind.as_str(),
                "variantNames": c.variant_names,
                "exposedProperties": c.exposed_properties,
                "tokensUsed": c.tokens_used,
                "componentCount": c.component_count,
            })).collect::<Vec<_>>(),
            "exportedColors": self.exported_colors,
            "exportedTypographies": self.exported_typographies,
            "exportedTokens": self.exported_tokens.iter().map(|t| json!({
                "path": t.path, "type": t.ty,
            })).collect::<Vec<_>>(),
        })
    }
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn parse_json(bytes: &[u8]) -> Option<Value> {
    serde_json::from_slice(bytes).ok()
}

/// Locate the file uuid: the manifest's first file id, else the `fid` that
/// owns the most `files/<fid>/…` entries (total — never panics on a thin tree).
fn find_file_id(files: &BTreeMap<String, Vec<u8>>) -> String {
    if let Some(bytes) = files.get("manifest.json") {
        if let Some(v) = parse_json(bytes) {
            if let Some(id) = v
                .get("files")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|f| f.get("id"))
                .and_then(Value::as_str)
            {
                return id.to_string();
            }
        }
    }
    // Fallback: the fid appearing in the most paths.
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for rel in files.keys() {
        let parts: Vec<&str> = rel.split('/').collect();
        if parts.len() >= 2 && parts[0] == "files" {
            let fid = parts[1].strip_suffix(".json").unwrap_or(parts[1]);
            *counts.entry(fid.to_string()).or_default() += 1;
        }
    }
    counts.into_iter().max_by_key(|(_, n)| *n).map(|(k, _)| k).unwrap_or_default()
}

/// Walk the main-instance subtree (following `shapes` child-id arrays) and
/// union every `appliedTokens` value. Mirrors the spike's `subtree_token_refs`.
fn subtree_token_refs(
    page_shapes: &BTreeMap<String, Value>,
    root_sid: &str,
    out: &mut BTreeSet<String>,
) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut stack = vec![root_sid.to_string()];
    while let Some(sid) = stack.pop() {
        if !seen.insert(sid.clone()) {
            continue;
        }
        let Some(shape) = page_shapes.get(&sid) else { continue };
        if let Some(at) = shape.get("appliedTokens").and_then(Value::as_object) {
            for v in at.values() {
                if let Some(path) = v.as_str() {
                    out.insert(path.to_string());
                }
            }
        }
        if let Some(children) = shape.get("shapes").and_then(Value::as_array) {
            for c in children {
                if let Some(cid) = c.as_str() {
                    stack.push(cid.to_string());
                }
            }
        }
    }
}

/// Collect `(path, $type)` leaves from a `tokens.json` value. Top-level keys
/// are set names (skip `$metadata`/`$themes`); the path excludes the set name
/// — the same dotted vocabulary `appliedTokens` uses (`layerBase.text`).
fn collect_tokens(root: &Value, out: &mut BTreeSet<TokenExport>) {
    let Some(sets) = root.as_object() else { return };
    for (set_name, set_val) in sets {
        if set_name.starts_with('$') {
            continue;
        }
        walk_token_group(set_val, &mut String::new(), out);
    }
}

fn walk_token_group(node: &Value, prefix: &mut String, out: &mut BTreeSet<TokenExport>) {
    let Some(map) = node.as_object() else { return };
    // A DTCG leaf carries `$value` (and usually `$type`).
    if map.contains_key("$value") {
        out.insert(TokenExport {
            path: prefix.clone(),
            ty: map.get("$type").and_then(Value::as_str).unwrap_or("").to_string(),
        });
        return;
    }
    for (key, child) in map {
        if key.starts_with('$') {
            continue;
        }
        let saved = prefix.len();
        if !prefix.is_empty() {
            prefix.push('.');
        }
        prefix.push_str(key);
        walk_token_group(child, prefix, out);
        prefix.truncate(saved);
    }
}

/// Extract the whole-file contract from a normalized `.penpot` tree
/// (`{relpath: bytes}`, `/` separators — `sync_core::read_tree`/`semantic_view`).
/// Deterministic and total.
pub fn extract_contracts(files: &BTreeMap<String, Vec<u8>>) -> LibraryContract {
    let file_id = find_file_id(files);
    let cprefix = format!("files/{file_id}/components/");
    let pprefix = format!("files/{file_id}/pages/");
    let colprefix = format!("files/{file_id}/colors/");
    let typprefix = format!("files/{file_id}/typographies/");
    let tokens_rel = format!("files/{file_id}/tokens.json");

    // 1. live components (skip tombstones).
    let mut components: Vec<Value> = Vec::new();
    for (rel, bytes) in files {
        if rel.starts_with(&cprefix) && rel.ends_with(".json") {
            if let Some(c) = parse_json(bytes) {
                if c.get("deleted").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                if c.get("id").and_then(Value::as_str).is_some() {
                    components.push(c);
                }
            }
        }
    }

    // 2. index page shapes by (pageId -> shapeId -> shape).
    let mut pages: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    for (rel, bytes) in files {
        if !(rel.starts_with(&pprefix) && rel.ends_with(".json")) {
            continue;
        }
        let stem = &rel[..rel.len() - 5];
        // files/<fid>/pages/<pid>/<sid>  (4 segments after "files/<fid>/")
        let parts: Vec<&str> = stem.split('/').collect();
        if parts.len() != 5 {
            continue; // the page doc itself is pages/<pid>.json (4 segments)
        }
        let (pid, sid) = (parts[3], parts[4]);
        if let Some(shape) = parse_json(bytes) {
            if shape.get("id").and_then(Value::as_str).is_some() {
                pages.entry(pid.to_string()).or_default().insert(sid.to_string(), shape);
            }
        }
    }

    // 3. group components into variant sets.
    //    group key = variantId when first-class, else shared path (caveat 1);
    //    the emitted set identity is always the uuid-free path (caveat 2).
    struct Group {
        first_class: bool,
        comps: Vec<Value>,
    }
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for c in components {
        let variant_id = s(&c, "variantId");
        let (gkey, first_class) = if !variant_id.is_empty() {
            (format!("v:{variant_id}"), true)
        } else {
            (format!("p:{}", s(&c, "path")), false)
        };
        let g = groups.entry(gkey).or_insert(Group { first_class, comps: Vec::new() });
        g.first_class |= first_class;
        g.comps.push(c);
    }

    let empty_page = BTreeMap::new();
    let mut contracts: Vec<Contract> = Vec::new();
    for group in groups.values() {
        let mut variant_names: BTreeSet<String> = BTreeSet::new();
        let mut prop_names: BTreeSet<String> = BTreeSet::new();
        let mut tokens_used: BTreeSet<String> = BTreeSet::new();
        let mut paths: BTreeSet<String> = BTreeSet::new();
        for c in &group.comps {
            variant_names.insert(s(c, "name"));
            let p = s(c, "path");
            if !p.is_empty() {
                paths.insert(p);
            }
            for vp in c.get("variantProperties").and_then(Value::as_array).into_iter().flatten() {
                let n = s(vp, "name");
                if !n.is_empty() {
                    prop_names.insert(n);
                }
            }
            let mi_page = s(c, "mainInstancePage");
            let mi_id = s(c, "mainInstanceId");
            if !mi_id.is_empty() {
                let page = pages.get(&mi_page).unwrap_or(&empty_page);
                subtree_token_refs(page, &mi_id, &mut tokens_used);
            }
        }
        // Set identity: the shared path (uuid-free). If several distinct paths
        // (unusual), the lexicographically-first keeps it deterministic; if
        // none, degrade to the first variant name rather than the churny id.
        let set = paths
            .iter()
            .next()
            .cloned()
            .or_else(|| variant_names.iter().next().cloned())
            .unwrap_or_default();
        contracts.push(Contract {
            set,
            set_kind: if group.first_class {
                SetKind::FirstClassVariant
            } else {
                SetKind::PathConvention
            },
            variant_names: variant_names.into_iter().collect(),
            exposed_properties: prop_names.into_iter().collect(),
            tokens_used: tokens_used.into_iter().collect(),
            component_count: group.comps.len(),
        });
    }
    contracts.sort_by(|a, b| {
        (a.set_kind.as_str(), &a.set).cmp(&(b.set_kind.as_str(), &b.set))
    });

    // 4. library-level exported surface.
    let mut exported_colors: BTreeSet<String> = BTreeSet::new();
    let mut exported_typographies: BTreeSet<String> = BTreeSet::new();
    let mut exported_tokens: BTreeSet<TokenExport> = BTreeSet::new();
    for (rel, bytes) in files {
        if rel.starts_with(&colprefix) && rel.ends_with(".json") {
            if let Some(v) = parse_json(bytes) {
                let name = s(&v, "name");
                if !name.is_empty() {
                    exported_colors.insert(name);
                }
            }
        } else if rel.starts_with(&typprefix) && rel.ends_with(".json") {
            if let Some(v) = parse_json(bytes) {
                let name = s(&v, "name");
                if !name.is_empty() {
                    exported_typographies.insert(name);
                }
            }
        }
    }
    if let Some(bytes) = files.get(&tokens_rel) {
        if let Some(v) = parse_json(bytes) {
            collect_tokens(&v, &mut exported_tokens);
        }
    }

    LibraryContract {
        file_id,
        contracts,
        exported_colors: exported_colors.into_iter().collect(),
        exported_typographies: exported_typographies.into_iter().collect(),
        exported_tokens: exported_tokens.into_iter().collect(),
    }
}

// ---------------------------------------------------------------------------
// Classifier
// ---------------------------------------------------------------------------

/// A version bump severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bump {
    /// Implementation only — the contract is byte-identical.
    Patch,
    /// The legacy→first-class variant-model migration (caveat 3). Ranks with
    /// `Patch` (no consumer break) but is labelled distinctly so it is never
    /// mistaken for a spurious `minor`.
    Migration,
    /// The contract grew — an element was added, none removed.
    Minor,
    /// An element was removed, renamed, or `$type`-changed.
    Major,
}

impl Bump {
    pub fn as_str(self) -> &'static str {
        match self {
            Bump::Patch => "patch",
            Bump::Migration => "migration",
            Bump::Minor => "minor",
            Bump::Major => "major",
        }
    }
    fn rank(self) -> u8 {
        match self {
            Bump::Patch | Bump::Migration => 0,
            Bump::Minor => 1,
            Bump::Major => 2,
        }
    }
}

/// How one set (or the library surface) changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDelta {
    pub field: String,
    pub bump: Bump,
    pub removed: Vec<String>,
    pub added: Vec<String>,
}

/// The per-set verdict with its evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetDelta {
    pub set: String,
    pub bump: Bump,
    pub reason: String,
    pub fields: Vec<FieldDelta>,
}

/// The whole-file classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub overall: Bump,
    pub sets: Vec<SetDelta>,
    pub library: Vec<FieldDelta>,
}

impl Classification {
    pub fn to_json(&self) -> Value {
        let field_json = |f: &FieldDelta| {
            json!({"field": f.field, "bump": f.bump.as_str(),
                   "removed": f.removed, "added": f.added})
        };
        json!({
            "overall": self.overall.as_str(),
            "sets": self.sets.iter().map(|d| json!({
                "set": d.set, "bump": d.bump.as_str(), "reason": d.reason,
                "fields": d.fields.iter().map(field_json).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "library": self.library.iter().map(field_json).collect::<Vec<_>>(),
        })
    }
}

/// The core rule: removed → major, else added → minor, else patch.
fn classify_field(field: &str, before: &[String], after: &[String]) -> FieldDelta {
    let b: BTreeSet<&String> = before.iter().collect();
    let a: BTreeSet<&String> = after.iter().collect();
    let removed: Vec<String> = b.difference(&a).map(|s| (*s).clone()).collect();
    let added: Vec<String> = a.difference(&b).map(|s| (*s).clone()).collect();
    let bump = if !removed.is_empty() {
        Bump::Major
    } else if !added.is_empty() {
        Bump::Minor
    } else {
        Bump::Patch
    };
    FieldDelta { field: field.to_string(), bump, removed, added }
}

fn max_bump(a: Bump, b: Bump) -> Bump {
    if b.rank() > a.rank() {
        b
    } else {
        a
    }
}

fn token_strings(tokens: &[TokenExport]) -> Vec<String> {
    // (path, $type) pair, so a $value edit is invisible (patch) but a $type
    // change reads as a rename (old removed + new added → major).
    tokens.iter().map(|t| format!("{}\u{1f}{}", t.path, t.ty)).collect()
}

/// Set identity for diffing is `(set_kind, set-path)` — matching the identity
/// `extract_contracts` sorts by — so a first-class set and a legacy set that
/// happen to share a `path` (a partially-migrated file) never collide and mask
/// each other's changes. Path alone was WRONG: it silently shadowed one set.
type SetKey<'a> = (&'a str, &'a str);

fn contract_index(lib: &LibraryContract) -> BTreeMap<SetKey<'_>, &Contract> {
    lib.contracts
        .iter()
        .map(|c| ((c.set_kind.as_str(), c.set.as_str()), c))
        .collect()
}

/// Normal (same-kind) per-set field diff.
fn diff_normal(bc: &Contract, ac: &Contract) -> (Bump, Vec<FieldDelta>) {
    let mut fields: Vec<FieldDelta> = Vec::new();
    let mut setbump = Bump::Patch;
    for f in [
        classify_field("variantNames", &bc.variant_names, &ac.variant_names),
        classify_field("exposedProperties", &bc.exposed_properties, &ac.exposed_properties),
        classify_field("tokensUsed", &bc.tokens_used, &ac.tokens_used),
    ] {
        if f.bump != Bump::Patch {
            setbump = max_bump(setbump, f.bump);
            fields.push(f);
        }
    }
    (setbump, fields)
}

/// Legacy→first-class migration diff (caveat 3): the exposedProperties growth
/// is the model switch, not contract growth — neutralise it. variantNames /
/// token changes still bump normally; an otherwise-clean switch is Migration.
fn diff_migration(bc: &Contract, ac: &Contract) -> (Bump, Vec<FieldDelta>) {
    let mut fields: Vec<FieldDelta> = Vec::new();
    let mut setbump = Bump::Patch;
    for f in [
        classify_field("variantNames", &bc.variant_names, &ac.variant_names),
        classify_field("tokensUsed", &bc.tokens_used, &ac.tokens_used),
    ] {
        if f.bump != Bump::Patch {
            setbump = max_bump(setbump, f.bump);
            fields.push(f);
        }
    }
    if setbump == Bump::Patch {
        setbump = Bump::Migration;
    }
    (setbump, fields)
}

/// Classify a contract change patch / minor / major, with the
/// legacy→first-class migration special-case (caveat 3).
pub fn diff_contracts(before: &LibraryContract, after: &LibraryContract) -> Classification {
    let b = contract_index(before);
    let a = contract_index(after);
    let mut overall = Bump::Patch;
    let mut sets: Vec<SetDelta> = Vec::new();
    let mut consumed: BTreeSet<SetKey> = BTreeSet::new();

    let legacy = SetKind::PathConvention.as_str();
    let firstclass = SetKind::FirstClassVariant.as_str();

    // Migration pass: a legacy set at path P (empty exposed, no first-class twin
    // in EITHER snapshot's competing kind) that becomes a first-class set at the
    // same P is a model switch, not remove+add. Keyed by (kind, set) so two sets
    // sharing a path in one snapshot are never conflated with a migration.
    for (&(kind, path), &bc) in &b {
        if kind != legacy {
            continue;
        }
        let lkey = (legacy, path);
        let fkey = (firstclass, path);
        if !bc.exposed_properties.is_empty() { continue; } // legacy sets have none; guard anyway
        if a.contains_key(&lkey) { continue; }              // legacy survived → not a migration
        if b.contains_key(&fkey) { continue; }              // first-class already existed before → coexistence
        let Some(&ac) = a.get(&fkey) else { continue };      // no first-class after → plain removal (main pass)
        let (setbump, fields) = diff_migration(bc, ac);
        overall = max_bump(overall, setbump);
        if setbump != Bump::Patch {
            sets.push(SetDelta {
                set: path.to_string(),
                bump: setbump,
                reason: "legacy->first-class variant migration".into(),
                fields,
            });
        }
        consumed.insert(lkey);
        consumed.insert(fkey);
    }

    let keys: BTreeSet<SetKey> = b.keys().chain(a.keys()).copied().collect();
    for key in keys {
        if consumed.contains(&key) {
            continue;
        }
        match (b.get(&key), a.get(&key)) {
            (Some(_), None) => {
                overall = max_bump(overall, Bump::Major);
                sets.push(SetDelta {
                    set: key.1.to_string(),
                    bump: Bump::Major,
                    reason: "set removed".into(),
                    fields: Vec::new(),
                });
            }
            (None, Some(_)) => {
                overall = max_bump(overall, Bump::Minor);
                sets.push(SetDelta {
                    set: key.1.to_string(),
                    bump: Bump::Minor,
                    reason: "set added".into(),
                    fields: Vec::new(),
                });
            }
            (Some(bc), Some(ac)) => {
                // Same (kind, set) in both snapshots — never a migration.
                let (setbump, fields) = diff_normal(bc, ac);
                overall = max_bump(overall, setbump);
                if setbump != Bump::Patch {
                    sets.push(SetDelta {
                        set: key.1.to_string(),
                        bump: setbump,
                        reason: String::new(),
                        fields,
                    });
                }
            }
            (None, None) => unreachable!(),
        }
    }

    // Library-level exported surface.
    let mut library: Vec<FieldDelta> = Vec::new();
    for f in [
        classify_field("exportedColors", &before.exported_colors, &after.exported_colors),
        classify_field(
            "exportedTypographies",
            &before.exported_typographies,
            &after.exported_typographies,
        ),
        classify_field(
            "exportedTokens",
            &token_strings(&before.exported_tokens),
            &token_strings(&after.exported_tokens),
        ),
    ] {
        if f.bump != Bump::Patch {
            overall = max_bump(overall, f.bump);
            library.push(f);
        }
    }

    // A no-op with any migration present surfaces as Migration, not Patch.
    if overall == Bump::Patch && sets.iter().any(|d| d.bump == Bump::Migration) {
        overall = Bump::Migration;
    }

    Classification { overall, sets, library }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FID: &str = "3a4be581-6d37-8010-8008-51f0c6eb307f";
    const PID: &str = "3a4be581-6d37-8010-8008-51f0c6eb3080";

    fn tree(entries: Vec<(String, Value)>) -> BTreeMap<String, Vec<u8>> {
        let mut m: BTreeMap<String, Vec<u8>> = entries
            .into_iter()
            .map(|(rel, v)| (rel, serde_json::to_vec(&v).unwrap()))
            .collect();
        m.entry("manifest.json".to_string())
            .or_insert_with(|| serde_json::to_vec(&json!({"files": [{"id": FID}]})).unwrap());
        m
    }

    fn comp(rel: &str, v: Value) -> (String, Value) {
        (format!("files/{FID}/components/{rel}.json"), v)
    }
    fn shape(sid: &str, v: Value) -> (String, Value) {
        (format!("files/{FID}/pages/{PID}/{sid}.json"), v)
    }

    /// A first-class variant set: grouped by variantId, keyed by path,
    /// exposedProperties from variantProperties[].name, tokensUsed unioned
    /// over the main-instance subtree.
    #[test]
    fn extracts_first_class_variant_set() {
        let files = tree(vec![
            comp("c1", json!({
                "id": "c1", "name": "Default", "path": "Controls / Button",
                "variantId": "v-1",
                "variantProperties": [{"name": "Size", "value": "Small"},
                                      {"name": "State", "value": "Default"}],
                "mainInstancePage": PID, "mainInstanceId": "mi1",
            })),
            comp("c2", json!({
                "id": "c2", "name": "Large", "path": "Controls / Button",
                "variantId": "v-1",
                "variantProperties": [{"name": "Size", "value": "Large"},
                                      {"name": "State", "value": "Default"}],
                "mainInstancePage": PID, "mainInstanceId": "mi2",
            })),
            shape("mi1", json!({"id": "mi1", "type": "frame", "mainInstance": true,
                "appliedTokens": {"fill": "layerBase.text"}, "shapes": ["ch1"]})),
            shape("ch1", json!({"id": "ch1", "type": "text",
                "appliedTokens": {"columnGap": "spacing.sm"}})),
            shape("mi2", json!({"id": "mi2", "type": "frame", "mainInstance": true,
                "appliedTokens": {"fill": "layerBase.text"}})),
        ]);
        let lib = extract_contracts(&files);
        assert_eq!(lib.contracts.len(), 1);
        let c = &lib.contracts[0];
        assert_eq!(c.set, "Controls / Button");
        assert_eq!(c.set_kind, SetKind::FirstClassVariant);
        assert_eq!(c.variant_names, vec!["Default", "Large"]);
        assert_eq!(c.exposed_properties, vec!["Size", "State"]);
        assert_eq!(c.tokens_used, vec!["layerBase.text", "spacing.sm"]);
        assert_eq!(c.component_count, 2);
    }

    /// A legacy set: grouped + keyed by shared path, names are the variants,
    /// exposedProperties empty.
    #[test]
    fn extracts_legacy_path_convention_set() {
        let files = tree(vec![
            comp("c1", json!({"id": "c1", "name": "Active", "path": "Legacy / Combobox",
                "mainInstancePage": PID, "mainInstanceId": "x1"})),
            comp("c2", json!({"id": "c2", "name": "Default", "path": "Legacy / Combobox",
                "mainInstancePage": PID, "mainInstanceId": "x2"})),
            comp("c3", json!({"id": "c3", "name": "Disabled", "path": "Legacy / Combobox",
                "mainInstancePage": PID, "mainInstanceId": "x3"})),
        ]);
        let lib = extract_contracts(&files);
        assert_eq!(lib.contracts.len(), 1);
        let c = &lib.contracts[0];
        assert_eq!(c.set, "Legacy / Combobox");
        assert_eq!(c.set_kind, SetKind::PathConvention);
        assert_eq!(c.variant_names, vec!["Active", "Default", "Disabled"]);
        assert!(c.exposed_properties.is_empty());
        assert!(c.tokens_used.is_empty());
    }

    #[test]
    fn deleted_components_are_skipped() {
        let files = tree(vec![
            comp("c1", json!({"id": "c1", "name": "Live", "path": "P"})),
            comp("c2", json!({"id": "c2", "name": "Dead", "path": "P", "deleted": true})),
        ]);
        let lib = extract_contracts(&files);
        assert_eq!(lib.contracts.len(), 1);
        assert_eq!(lib.contracts[0].variant_names, vec!["Live"]);
    }

    #[test]
    fn extracts_library_exported_surface() {
        let files = tree(vec![
            (format!("files/{FID}/colors/col1.json"),
                json!({"id": "col1", "name": "Brand Teal", "color": "#12b886"})),
            (format!("files/{FID}/colors/col2.json"),
                json!({"id": "col2", "name": "Accent", "color": "#ff0000"})),
            (format!("files/{FID}/typographies/t1.json"),
                json!({"id": "t1", "name": "Heading XL", "fontFamily": "Inter"})),
            (format!("files/{FID}/tokens.json"), json!({
                "$metadata": {"tokenSetOrder": ["Base"]},
                "$themes": [],
                "Base": {
                    "layerBase": {"text": {"$type": "color", "$value": "#000"}},
                    "spacing": {"sm": {"$type": "spacing", "$value": "4"}},
                },
            })),
        ]);
        let lib = extract_contracts(&files);
        assert_eq!(lib.exported_colors, vec!["Accent", "Brand Teal"]);
        assert_eq!(lib.exported_typographies, vec!["Heading XL"]);
        assert_eq!(
            lib.exported_token_paths(),
            vec!["layerBase.text".to_string(), "spacing.sm".to_string()]
        );
    }

    fn base_first_class() -> BTreeMap<String, Vec<u8>> {
        tree(vec![
            comp("c1", json!({
                "id": "c1", "name": "Default", "path": "Controls / Button",
                "variantId": "v-1",
                "variantProperties": [{"name": "Size", "value": "Small"},
                                      {"name": "State", "value": "Default"}],
                "mainInstancePage": PID, "mainInstanceId": "mi1",
            })),
            shape("mi1", json!({"id": "mi1", "type": "frame", "mainInstance": true,
                "x": 0, "y": 0, "fills": [{"fillColor": "#abcdef"}],
                "appliedTokens": {"fill": "layerBase.text"}})),
        ])
    }

    #[test]
    fn diff_impl_only_is_patch() {
        let before = extract_contracts(&base_first_class());
        // move + inline fill change; contract untouched.
        let mut files = base_first_class();
        files.insert(
            format!("files/{FID}/pages/{PID}/mi1.json"),
            serde_json::to_vec(&json!({"id": "mi1", "type": "frame", "mainInstance": true,
                "x": 40, "y": 40, "fills": [{"fillColor": "#123456"}],
                "appliedTokens": {"fill": "layerBase.text"}})).unwrap(),
        );
        let after = extract_contracts(&files);
        assert_eq!(diff_contracts(&before, &after).overall, Bump::Patch);
    }

    #[test]
    fn diff_added_property_is_minor() {
        let before = extract_contracts(&base_first_class());
        let mut files = base_first_class();
        files.insert(
            comp("c1", json!({
                "id": "c1", "name": "Default", "path": "Controls / Button",
                "variantId": "v-1",
                "variantProperties": [{"name": "Size", "value": "Small"},
                                      {"name": "State", "value": "Default"},
                                      {"name": "Theme", "value": "Dark"}],
                "mainInstancePage": PID, "mainInstanceId": "mi1",
            })).0,
            serde_json::to_vec(&json!({
                "id": "c1", "name": "Default", "path": "Controls / Button",
                "variantId": "v-1",
                "variantProperties": [{"name": "Size", "value": "Small"},
                                      {"name": "State", "value": "Default"},
                                      {"name": "Theme", "value": "Dark"}],
                "mainInstancePage": PID, "mainInstanceId": "mi1",
            })).unwrap(),
        );
        let after = extract_contracts(&files);
        let cls = diff_contracts(&before, &after);
        assert_eq!(cls.overall, Bump::Minor);
        assert!(cls.sets[0].fields.iter().any(|f| f.field == "exposedProperties"
            && f.added == vec!["Theme".to_string()]));
    }

    #[test]
    fn diff_removed_property_is_major() {
        let before = extract_contracts(&base_first_class());
        let mut files = base_first_class();
        files.insert(
            comp("c1", json!({})).0,
            serde_json::to_vec(&json!({
                "id": "c1", "name": "Default", "path": "Controls / Button",
                "variantId": "v-1",
                "variantProperties": [{"name": "Size", "value": "Small"}],
                "mainInstancePage": PID, "mainInstanceId": "mi1",
            })).unwrap(),
        );
        let after = extract_contracts(&files);
        assert_eq!(diff_contracts(&before, &after).overall, Bump::Major);
    }

    #[test]
    fn diff_renamed_variant_is_major() {
        let before = extract_contracts(&base_first_class());
        let mut files = base_first_class();
        files.insert(
            comp("c1", json!({})).0,
            serde_json::to_vec(&json!({
                "id": "c1", "name": "Primary", "path": "Controls / Button",
                "variantId": "v-1",
                "variantProperties": [{"name": "Size", "value": "Small"},
                                      {"name": "State", "value": "Default"}],
                "mainInstancePage": PID, "mainInstanceId": "mi1",
            })).unwrap(),
        );
        let after = extract_contracts(&files);
        let cls = diff_contracts(&before, &after);
        assert_eq!(cls.overall, Bump::Major);
        assert!(cls.sets[0].fields.iter().any(|f| f.field == "variantNames"
            && f.removed == vec!["Default".to_string()]));
    }

    #[test]
    fn diff_token_type_change_is_major_value_change_is_patch() {
        let base = |ty: &str, val: &str| tree(vec![(
            format!("files/{FID}/tokens.json"),
            json!({"Base": {"radius": {"lg": {"$type": ty, "$value": val}}}}),
        )]);
        let a = extract_contracts(&base("borderRadius", "8"));
        // $value change only -> patch
        let v = extract_contracts(&base("borderRadius", "12"));
        assert_eq!(diff_contracts(&a, &v).overall, Bump::Patch);
        // $type change -> major
        let t = extract_contracts(&base("dimension", "8"));
        assert_eq!(diff_contracts(&a, &t).overall, Bump::Major);
    }

    #[test]
    fn diff_exported_color_added_is_minor_removed_is_major() {
        let with = |names: &[&str]| {
            tree(names.iter().enumerate().map(|(i, n)| (
                format!("files/{FID}/colors/c{i}.json"),
                json!({"id": format!("c{i}"), "name": n}),
            )).collect())
        };
        let a = extract_contracts(&with(&["Brand"]));
        let b = extract_contracts(&with(&["Brand", "Accent"]));
        assert_eq!(diff_contracts(&a, &b).overall, Bump::Minor);
        assert_eq!(diff_contracts(&b, &a).overall, Bump::Major);
    }

    /// Caveat 3: a legacy set migrating to first-class must NOT read as a
    /// spurious minor — its exposedProperties growth is the model switch.
    #[test]
    fn diff_legacy_to_first_class_migration_is_not_minor() {
        // before: legacy set at "Controls / Button" (two variants, no props).
        let before = extract_contracts(&tree(vec![
            comp("c1", json!({"id": "c1", "name": "Default", "path": "Controls / Button"})),
            comp("c2", json!({"id": "c2", "name": "Large", "path": "Controls / Button"})),
        ]));
        // after: same path, same variant names, now first-class with props.
        let after = extract_contracts(&tree(vec![
            comp("c1", json!({"id": "c1", "name": "Default", "path": "Controls / Button",
                "variantId": "v-1",
                "variantProperties": [{"name": "Size", "value": "Small"}]})),
            comp("c2", json!({"id": "c2", "name": "Large", "path": "Controls / Button",
                "variantId": "v-1",
                "variantProperties": [{"name": "Size", "value": "Large"}]})),
        ]));
        let cls = diff_contracts(&before, &after);
        assert_eq!(cls.overall, Bump::Migration, "{:?}", cls);
        assert_ne!(cls.overall, Bump::Minor);
    }

    /// Even during a migration, a genuinely removed variant still bumps major.
    #[test]
    fn diff_migration_with_dropped_variant_is_major() {
        let before = extract_contracts(&tree(vec![
            comp("c1", json!({"id": "c1", "name": "Default", "path": "Controls / Button"})),
            comp("c2", json!({"id": "c2", "name": "Large", "path": "Controls / Button"})),
        ]));
        let after = extract_contracts(&tree(vec![
            comp("c1", json!({"id": "c1", "name": "Default", "path": "Controls / Button",
                "variantId": "v-1",
                "variantProperties": [{"name": "Size", "value": "Small"}]})),
        ]));
        assert_eq!(diff_contracts(&before, &after).overall, Bump::Major);
    }

    /// Caveat 2: the contract is uuid-free, so remapping every uuid (what
    /// import-as-new does) leaves the extracted contract byte-identical.
    #[test]
    fn contract_is_uuid_invariant() {
        let a = base_first_class();
        let before = extract_contracts(&a);
        // Rewrite every uuid-ish token consistently (simulates import churn).
        let remap = |s: &str| -> String {
            s.replace(FID, "9999ffff-0000-0000-0000-000000000000")
                .replace(PID, "8888eeee-0000-0000-0000-000000000000")
                .replace("v-1", "zzzz-remapped")
                .replace("mi1", "newmain")
                .replace("c1", "newcomp")
        };
        let mut churned: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for (rel, bytes) in &a {
            let text = String::from_utf8(bytes.clone()).unwrap();
            churned.insert(remap(rel), remap(&text).into_bytes());
        }
        let after = extract_contracts(&churned);
        // fileId churns; the contract body does not.
        assert_ne!(before.file_id, after.file_id);
        assert_eq!(before.contracts, after.contracts);
        assert_eq!(diff_contracts(&before, &after).overall, Bump::Patch);
    }

    // A partially-migrated file where a first-class set and a legacy set share
    // the same `path`. Dropping a property from the first-class set is MAJOR and
    // must NOT be masked by the unchanged legacy set at the same path (the differ
    // must key by (set_kind, set), not path alone).
    #[test]
    fn diff_same_path_across_kinds_does_not_mask_a_major() {
        fn lib(fc_exposed: &[&str]) -> LibraryContract {
            let mk = |kind: SetKind, exposed: &[&str]| Contract {
                set: "Shared".into(),
                set_kind: kind,
                variant_names: vec!["A".into(), "B".into()],
                exposed_properties: exposed.iter().map(|s| s.to_string()).collect(),
                tokens_used: vec![],
                component_count: 2,
            };
            LibraryContract {
                file_id: "f".into(),
                contracts: vec![
                    mk(SetKind::FirstClassVariant, fc_exposed),
                    mk(SetKind::PathConvention, &[]),
                ],
                exported_colors: vec![],
                exported_typographies: vec![],
                exported_tokens: vec![],
            }
        }
        let before = lib(&["Size", "State"]);
        let after = lib(&["Size"]); // first-class set dropped the "State" property
        assert_eq!(diff_contracts(&before, &after).overall, Bump::Major);
        // Identical snapshots stay patch (the migration pair never false-fires).
        assert_eq!(diff_contracts(&before, &before).overall, Bump::Patch);
    }
}
