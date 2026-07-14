//! The index database: bundled SQLite with an FTS5 table for the docs and a
//! plain `indexed_files` table recording *which semantic tree hash* each
//! file's rows came from.
//!
//! The state lives in the SAME database and is written in the SAME
//! transaction as the rows, so state and outputs can never disagree — and
//! deleting the db deletes the state with it, which is exactly invariant 1
//! (derived state is disposable: a fresh db means a full rebuild from disk).
//!
//! Schema versioning via `PRAGMA user_version`: any mismatch nukes and
//! recreates everything (the index is a cache, never a source of truth).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;

use crate::extract::DocRow;

/// Bumped whenever the schema or the extraction semantics change: old dbs
/// are discarded and rebuilt (cheap, disposable).
pub const SCHEMA_VERSION: i64 = 1;

const CREATE_SQL: &str = "
CREATE TABLE indexed_files (
    owner_id   TEXT PRIMARY KEY,  -- manifest key (penpot file uuid)
    rel_path   TEXT NOT NULL,     -- vault-relative .penpot path
    indexed_hash TEXT NOT NULL,   -- semantic tree hash the rows came from
    indexed_at TEXT NOT NULL
);
CREATE VIRTUAL TABLE docs USING fts5(
    body,
    name      UNINDEXED,
    kind      UNINDEXED,
    owner_id  UNINDEXED,
    file_id   UNINDEXED,
    page_id   UNINDEXED,
    object_id UNINDEXED,
    board_id  UNINDEXED,
    rel_path  UNINDEXED,
    tokenize = 'unicode61 remove_diacritics 2'
);
";

fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

/// The writer handle (one per service; rusqlite connections are not Sync).
pub struct IndexDb {
    conn: Connection,
}

impl IndexDb {
    /// Open (creating parent dirs) and migrate/recreate the schema.
    pub fn open(path: &Path) -> anyhow::Result<IndexDb> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open index db {}", path.display()))?;
        configure(&conn)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            // Disposable cache: any other version (including 0 = fresh file)
            // is rebuilt from scratch.
            conn.execute_batch(
                "DROP TABLE IF EXISTS indexed_files; DROP TABLE IF EXISTS docs;",
            )?;
            conn.execute_batch(CREATE_SQL)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            if version != 0 {
                tracing::info!(
                    found = version,
                    expected = SCHEMA_VERSION,
                    "vault-index: schema version mismatch — index recreated (disposable)"
                );
            }
        }
        Ok(IndexDb { conn })
    }

    /// `{owner_id: (rel_path, indexed_hash)}` — THE needs-reindex input.
    pub fn indexed_files(&self) -> anyhow::Result<BTreeMap<String, (String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT owner_id, rel_path, indexed_hash FROM indexed_files")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, (r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (k, v) = row?;
            out.insert(k, v);
        }
        Ok(out)
    }

    /// Atomically replace every doc of `owner_id` and record the hash the
    /// new rows came from. One transaction: rows and state cannot disagree.
    pub fn replace_file(
        &mut self,
        owner_id: &str,
        rel_path: &str,
        indexed_hash: &str,
        docs: &[DocRow],
    ) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM docs WHERE owner_id = ?1", params![owner_id])?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO docs (body, name, kind, owner_id, file_id, page_id,
                                   object_id, board_id, rel_path)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for d in docs {
                ins.execute(params![
                    d.body,
                    d.name,
                    d.kind.as_str(),
                    owner_id,
                    d.file_id,
                    d.page_id,
                    d.object_id,
                    d.board_id,
                    rel_path,
                ])?;
            }
        }
        tx.execute(
            "INSERT INTO indexed_files (owner_id, rel_path, indexed_hash, indexed_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(owner_id) DO UPDATE SET
               rel_path = excluded.rel_path,
               indexed_hash = excluded.indexed_hash,
               indexed_at = excluded.indexed_at",
            params![owner_id, rel_path, indexed_hash, sync_core::manifest::now_rfc3339()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A file vanished from the manifest: drop its rows + state.
    pub fn remove_file(&mut self, owner_id: &str) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM docs WHERE owner_id = ?1", params![owner_id])?;
        tx.execute("DELETE FROM indexed_files WHERE owner_id = ?1", params![owner_id])?;
        tx.commit()?;
        Ok(())
    }

    /// Manifest re-keyed a file to a new path with the same content hash
    /// (OS rename): update rel_path everywhere without reindexing.
    pub fn update_rel_path(&mut self, owner_id: &str, rel_path: &str) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE docs SET rel_path = ?2 WHERE owner_id = ?1",
            params![owner_id, rel_path],
        )?;
        tx.execute(
            "UPDATE indexed_files SET rel_path = ?2 WHERE owner_id = ?1",
            params![owner_id, rel_path],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn docs_total(&self) -> anyhow::Result<usize> {
        let n: i64 = self.conn.query_row("SELECT count(*) FROM docs", [], |r| r.get(0))?;
        Ok(n as usize)
    }
}

// ---------------------------------------------------------------------------
// Search (read side — separate connections, safe under WAL while the
// service writes)
// ---------------------------------------------------------------------------

/// One search hit, serialized camelCase for the HTTP API.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Hit {
    pub kind: String,
    pub name: String,
    /// FTS5 snippet with the match marked `«…»`.
    pub snippet: String,
    pub file_id: String,
    pub page_id: String,
    pub object_id: String,
    pub board_id: String,
    pub rel_path: String,
    /// bm25 rank (more negative = better). Deterministic for a given corpus.
    pub score: f64,
}

/// Cheap cloneable read handle: opens a fresh read-only connection per query
/// (µs-scale; keeps the type Send+Sync without connection pooling).
#[derive(Debug, Clone)]
pub struct SearchHandle {
    db_path: PathBuf,
}

/// Search failure modes the HTTP layer maps to status codes.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("index database not ready")]
    NotReady,
    #[error("search failed: {0}")]
    Other(#[from] anyhow::Error),
}

impl SearchHandle {
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        SearchHandle { db_path: db_path.into() }
    }

    /// Run an FTS5 MATCH query (build it with [`crate::query::build_match_query`]).
    /// Results are ranked by bm25 with a deterministic tiebreak so identical
    /// corpora yield byte-identical result lists (the rebuild invariant).
    pub fn search(
        &self,
        match_expr: &str,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Hit>, SearchError> {
        let conn = Connection::open_with_flags(
            &self.db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| SearchError::NotReady)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| SearchError::Other(e.into()))?;
        let sql = "
            SELECT kind, name,
                   snippet(docs, 0, '«', '»', '…', 12),
                   file_id, page_id, object_id, board_id, rel_path,
                   bm25(docs)
            FROM docs
            WHERE docs MATCH ?1 AND (?2 IS NULL OR kind = ?2)
            ORDER BY bm25(docs), kind, file_id, object_id
            LIMIT ?3";
        let mut stmt = conn.prepare(sql).map_err(|e| match e {
            // docs table missing = the service has not created the schema yet.
            rusqlite::Error::SqliteFailure(_, Some(ref m)) if m.contains("no such table") => {
                SearchError::NotReady
            }
            e => SearchError::Other(e.into()),
        })?;
        let rows = stmt
            .query_map(params![match_expr, kind, limit as i64], |r| {
                Ok(Hit {
                    kind: r.get(0)?,
                    name: r.get(1)?,
                    snippet: r.get(2)?,
                    file_id: r.get(3)?,
                    page_id: r.get(4)?,
                    object_id: r.get(5)?,
                    board_id: r.get(6)?,
                    rel_path: r.get(7)?,
                    score: r.get(8)?,
                })
            })
            .map_err(|e| SearchError::Other(e.into()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| SearchError::Other(e.into()))?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{DocKind, DocRow};

    fn doc(kind: DocKind, body: &str, object_id: &str) -> DocRow {
        DocRow {
            kind,
            name: body.to_string(),
            body: body.to_string(),
            file_id: "f1".into(),
            page_id: "p1".into(),
            object_id: object_id.into(),
            board_id: "b1".into(),
        }
    }

    #[test]
    fn replace_search_and_remove_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("idx.sqlite3");
        let mut db = IndexDb::open(&path).unwrap();
        db.replace_file(
            "f1",
            "proj/home.penpot",
            "h1",
            &[
                doc(DocKind::Text, "proceed to checkout", "s1"),
                doc(DocKind::Board, "Checkout Flow", "s2"),
            ],
        )
        .unwrap();
        assert_eq!(db.docs_total().unwrap(), 2);
        assert_eq!(
            db.indexed_files().unwrap()["f1"],
            ("proj/home.penpot".to_string(), "h1".to_string())
        );

        let search = SearchHandle::new(&path);
        let hits = search.search("\"checkout\"", None, 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.rel_path == "proj/home.penpot"));
        // kind filter
        let hits = search.search("\"checkout\"", Some("board"), 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Checkout Flow");

        // Replace: the old rows must be GONE (stale-hit rule).
        db.replace_file("f1", "proj/home.penpot", "h2", &[doc(DocKind::Text, "pay now", "s1")])
            .unwrap();
        assert!(search.search("\"checkout\"", None, 10).unwrap().is_empty());
        assert_eq!(search.search("\"pay\"", None, 10).unwrap().len(), 1);

        db.remove_file("f1").unwrap();
        assert_eq!(db.docs_total().unwrap(), 0);
        assert!(db.indexed_files().unwrap().is_empty());
    }

    #[test]
    fn rel_path_update_without_reindex() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("idx.sqlite3");
        let mut db = IndexDb::open(&path).unwrap();
        db.replace_file("f1", "old/a.penpot", "h1", &[doc(DocKind::Text, "hello", "s1")])
            .unwrap();
        db.update_rel_path("f1", "new/b.penpot").unwrap();
        assert_eq!(
            db.indexed_files().unwrap()["f1"].0,
            "new/b.penpot".to_string()
        );
        let hits = SearchHandle::new(&path).search("\"hello\"", None, 10).unwrap();
        assert_eq!(hits[0].rel_path, "new/b.penpot");
    }

    #[test]
    fn schema_version_mismatch_recreates() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("idx.sqlite3");
        {
            let mut db = IndexDb::open(&path).unwrap();
            db.replace_file("f1", "a.penpot", "h1", &[doc(DocKind::Text, "hello", "s1")])
                .unwrap();
        }
        // Simulate a future schema.
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 999).unwrap();
        }
        let db = IndexDb::open(&path).unwrap();
        assert_eq!(db.docs_total().unwrap(), 0, "old rows discarded");
        assert!(db.indexed_files().unwrap().is_empty(), "state discarded with them");
    }

    #[test]
    fn search_on_missing_db_is_not_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let search = SearchHandle::new(tmp.path().join("nope.sqlite3"));
        assert!(matches!(
            search.search("\"x\"", None, 10),
            Err(SearchError::NotReady)
        ));
    }

    #[test]
    fn unicode_and_diacritics_match() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("idx.sqlite3");
        let mut db = IndexDb::open(&path).unwrap();
        db.replace_file(
            "f1",
            "a.penpot",
            "h1",
            &[doc(DocKind::Text, "Diseño 検索 Überschrift", "s1")],
        )
        .unwrap();
        let search = SearchHandle::new(&path);
        assert_eq!(search.search("\"diseño\"", None, 10).unwrap().len(), 1);
        // remove_diacritics 2: plain-ascii query matches the accented token
        assert_eq!(search.search("\"diseno\"", None, 10).unwrap().len(), 1);
        assert_eq!(search.search("\"検索\"", None, 10).unwrap().len(), 1);
        assert_eq!(search.search("\"uberschrift\"", None, 10).unwrap().len(), 1);
        assert!(search.search("\"missing\"", None, 10).unwrap().is_empty());
    }
}
