//! Board-name → filename sanitization and per-file deduplication.
//!
//! Mirrors the rules of the sync daemon's path sanitizer (path separators /
//! control chars / Windows-reserved chars become `-`, leading dots and
//! trailing dots/spaces stripped, length capped, empty → fallback), plus a
//! **case-insensitive** dedup pass: two boards named "Board" and "board"
//! must not collide on a case-insensitive filesystem (macOS APFS default).

const MAX_STEM_CHARS: usize = 100;

/// Sanitize an arbitrary Penpot board name into a safe filename stem.
pub fn sanitize_stem(raw: &str) -> String {
    let mut s: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .take(MAX_STEM_CHARS)
        .collect();
    while s.starts_with(['.', ' ']) {
        s.remove(0);
    }
    while s.ends_with(['.', ' ']) {
        s.pop();
    }
    if s.is_empty() {
        "board".to_string()
    } else {
        s
    }
}

/// Deterministic unique filename stems for an ordered list of board names:
/// sanitize each, then suffix case-insensitive duplicates with `-2`, `-3`, …
/// (first occurrence keeps the bare name). Suffixed forms are themselves
/// checked against the used set, so a literal "Board-2" board never collides.
pub fn unique_stems(names: &[String]) -> Vec<String> {
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let base = sanitize_stem(name);
        let mut candidate = base.clone();
        let mut n = 1u32;
        while !used.insert(candidate.to_lowercase()) {
            n += 1;
            candidate = format!("{base}-{n}");
        }
        out.push(candidate);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sanitize_basics() {
        assert_eq!(sanitize_stem("Board 1"), "Board 1");
        assert_eq!(sanitize_stem("a/b\\c:d"), "a-b-c-d");
        assert_eq!(sanitize_stem("..hidden"), "hidden");
        assert_eq!(sanitize_stem("trailing. . "), "trailing");
        assert_eq!(sanitize_stem(""), "board");
        assert_eq!(sanitize_stem("  .  "), "board");
        assert_eq!(sanitize_stem("emoji 🎨 ok"), "emoji 🎨 ok");
        assert_eq!(sanitize_stem("win<>:\"|?*"), "win-------");
        assert_eq!(sanitize_stem("tab\there"), "tab-here");
        let long = "x".repeat(500);
        assert_eq!(sanitize_stem(&long).chars().count(), MAX_STEM_CHARS);
    }

    #[test]
    fn dedup_appends_counters_in_order() {
        assert_eq!(
            unique_stems(&v(&["Board", "Board", "Board"])),
            vec!["Board", "Board-2", "Board-3"]
        );
    }

    #[test]
    fn dedup_is_case_insensitive_for_apfs() {
        assert_eq!(unique_stems(&v(&["Board", "board"])), vec!["Board", "board-2"]);
    }

    #[test]
    fn dedup_collides_with_sanitized_forms() {
        // Two different raw names that sanitize identically still dedup.
        assert_eq!(unique_stems(&v(&["a/b", "a:b"])), vec!["a-b", "a-b-2"]);
    }

    #[test]
    fn dedup_avoids_existing_literal_suffix_names() {
        // A literal "Board-2" board occupies its name; the duplicate walks on.
        assert_eq!(
            unique_stems(&v(&["Board", "Board-2", "Board"])),
            vec!["Board", "Board-2", "Board-3"]
        );
    }

    #[test]
    fn distinct_names_pass_through() {
        assert_eq!(unique_stems(&v(&["Cover", "Detail"])), vec!["Cover", "Detail"]);
    }
}
