//! FTS5 MATCH-expression building and workspace deep-link building — the two
//! pure string layers between user input and the index/webview.

/// Build a safe FTS5 MATCH expression from raw user input.
///
/// Every whitespace-separated token becomes a quoted phrase (internal `"`
/// doubled per FTS5 string syntax), so FTS5 operators (`AND`, `NOT`, `-`,
/// `(`, `:`) in user input are matched literally instead of being parsed.
/// The last token gets a `*` prefix marker for as-you-type search
/// (`"che"*` still matches the full token `checkout`). Returns `None` for
/// empty/whitespace-only input.
pub fn build_match_query(input: &str) -> Option<String> {
    let tokens: Vec<&str> = input.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = tokens
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if let Some(last) = parts.last_mut() {
        last.push('*');
    }
    Some(parts.join(" "))
}

/// The verified workspace deep link (PLAN2.md pillar 1: route string present
/// in the compiled bundle `runtime/frontend/js/main.js`; query-param
/// destructuring in upstream 2.16.2 `app/main/ui.cljs`). Penpot uuids are
/// `[0-9a-f-]` so no URL encoding is needed; anything else is filtered out
/// defensively rather than encoded.
pub fn workspace_deep_link(team_id: &str, file_id: &str, page_id: Option<&str>) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect()
    };
    let mut url = format!(
        "/#/workspace?team-id={}&file-id={}",
        clean(team_id),
        clean(file_id)
    );
    if let Some(p) = page_id.filter(|p| !p.is_empty()) {
        url.push_str("&page-id=");
        url.push_str(&clean(p));
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_quoted_and_last_gets_prefix_star() {
        assert_eq!(build_match_query("checkout"), Some("\"checkout\"*".into()));
        assert_eq!(
            build_match_query("checkout button"),
            Some("\"checkout\" \"button\"*".into())
        );
        assert_eq!(build_match_query("  spaced   out  "), Some("\"spaced\" \"out\"*".into()));
    }

    #[test]
    fn operators_and_special_chars_are_neutralized() {
        // FTS5 operators must be treated as literal text, not syntax.
        assert_eq!(build_match_query("AND"), Some("\"AND\"*".into()));
        assert_eq!(
            build_match_query("a NOT (b OR c)"),
            Some("\"a\" \"NOT\" \"(b\" \"OR\" \"c)\"*".into())
        );
        assert_eq!(build_match_query("col:name"), Some("\"col:name\"*".into()));
        assert_eq!(build_match_query("semi-transparent"), Some("\"semi-transparent\"*".into()));
        // Embedded quotes are doubled (FTS5 string escaping).
        assert_eq!(build_match_query("say \"hi\""), Some("\"say\" \"\"\"hi\"\"\"*".into()));
        assert_eq!(build_match_query("#12b886"), Some("\"#12b886\"*".into()));
    }

    #[test]
    fn empty_input_is_none() {
        assert_eq!(build_match_query(""), None);
        assert_eq!(build_match_query("   \t "), None);
    }

    #[test]
    fn unicode_passes_through() {
        assert_eq!(build_match_query("Diseño 検索"), Some("\"Diseño\" \"検索\"*".into()));
    }

    /// The built expressions must never make SQLite error out, whatever the
    /// user typed (correctness of *matching* is covered in db.rs tests).
    #[test]
    fn built_queries_never_error_against_a_real_index() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("idx.sqlite3");
        let mut db = crate::db::IndexDb::open(&path).unwrap();
        db.replace_file(
            "f1",
            "a.penpot",
            "h1",
            &[crate::extract::DocRow {
                kind: crate::extract::DocKind::Text,
                name: "t".into(),
                body: "checkout button semi-transparent".into(),
                file_id: "f1".into(),
                page_id: "p1".into(),
                object_id: "s1".into(),
                board_id: "b1".into(),
            }],
        )
        .unwrap();
        let search = crate::db::SearchHandle::new(&path);
        for input in [
            "checkout",
            "checkout button",
            "AND OR NOT",
            "a NOT (b OR c)",
            "say \"hi\"",
            "-dash *star* ^caret",
            "col:name near/2",
            "\"\"\"",
            "🎨 emoji",
            "検索 Diseño",
            "$$$ %%%",
        ] {
            let expr = build_match_query(input).unwrap();
            let result = search.search(&expr, None, 10);
            assert!(result.is_ok(), "input {input:?} -> expr {expr:?} errored: {result:?}");
        }
        // Sanity: a real match still works through the builder.
        let expr = build_match_query("semi-transparent").unwrap();
        assert_eq!(search.search(&expr, None, 10).unwrap().len(), 1);
        // Prefix behavior: partial last token matches.
        let expr = build_match_query("checkou").unwrap();
        assert_eq!(search.search(&expr, None, 10).unwrap().len(), 1);
    }

    #[test]
    fn deep_link_shapes() {
        assert_eq!(
            workspace_deep_link("t-1", "f-2", Some("p-3")),
            "/#/workspace?team-id=t-1&file-id=f-2&page-id=p-3"
        );
        assert_eq!(
            workspace_deep_link("t-1", "f-2", None),
            "/#/workspace?team-id=t-1&file-id=f-2"
        );
        assert_eq!(
            workspace_deep_link("t-1", "f-2", Some("")),
            "/#/workspace?team-id=t-1&file-id=f-2"
        );
        // Injection attempts are stripped, not encoded.
        assert_eq!(
            workspace_deep_link("t&x=1", "f#/evil", Some("p 3")),
            "/#/workspace?team-id=tx1&file-id=fevil&page-id=p3"
        );
    }
}
