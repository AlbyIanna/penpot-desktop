//! sync-core — pure library for Penpot Local's sync engine (M2+).
//!
//! No network, no processes: filesystem + bytes only, so everything here is
//! exhaustively testable. Five pieces:
//!
//! - [`normalize`]: deterministic JSON rewriting of an unzipped binfile-v3
//!   tree, byte-compatible with `scripts/roundtrip.py`'s normalizer
//!   (`json.dumps(obj, indent=2, sort_keys=True, ensure_ascii=False)` + `\n`).
//! - [`hash`]: semantic tree hash — strip `createdAt`/`modifiedAt` everywhere,
//!   sha256 over sorted `(relpath, content-sha256)` pairs, binaries raw.
//! - [`manifest`]: the `.penpot-sync.json` model (fileId ↔ path ↔ project ↔
//!   revn ↔ lastSyncedHash ↔ lastSyncedAt), atomic save, versioned schema.
//! - [`swap`]: two-phase directory swap with crash recovery + startup sweep
//!   of orphaned `.tmp-*` / `.old-*` leftovers.
//! - [`ziputil`]: zip/unzip helpers for binfile-v3 dirs (deterministic zip).
//!
//! Invariants (CLAUDE.md, verified in M0): never compare zip containers, only
//! extracted trees; `revn` is advisory; normalization = sorted keys, 2-space
//! indent, non-ASCII preserved, LF, trailing newline.

pub mod binfile;
pub mod consent;
pub mod hash;
pub mod lock;
pub mod manifest;
pub mod normalize;
pub mod swap;
pub mod trash;
mod util;
pub mod ziputil;

pub use binfile::trim_to_single_file;
pub use consent::{ConsentLedger, ConsentRecord, CONSENT_FILE_NAME, CONSENT_SCHEMA_VERSION};
pub use hash::{
    read_tree, semantic_tree_hash, semantic_view, sha256_hex, strip_volatile, tree_hash,
};
pub use lock::{
    plan_reapply, plan_relink, LibraryLink, LockEntry, Lockfile, ReapplyAction, RelinkAction,
    RelinkOp, LOCK_FILE_NAME, LOCK_SCHEMA_VERSION,
};
pub use manifest::{Manifest, ManifestEntry, MANIFEST_FILE_NAME, MANIFEST_SCHEMA_VERSION};
pub use normalize::{dumps, normalize_json_bytes, normalize_tree, VOLATILE_KEYS};
pub use swap::{cleanup_orphans, commit_dir_swap, stage_path_for, CleanupReport};
pub use ziputil::{unzip_to, zip_dir};

/// The in-vault package home (PLAN3 chapter 3 / E2): one git repo per package
/// lives under `<vault>/.penpot-packages/<id>/`. Because the name is
/// dot-prefixed it is invisible to BOTH sync directions — the daemon's event
/// watcher and its full reconcile walk skip `.`-prefixed dirs (the same
/// guarantee `.penpot-vault`/`.penpot-sync.json` rely on). Packages are
/// therefore never auto-imported, hashed, or conflict-swept; install is an
/// explicit verb. Named here so the blindness is a documented, tested
/// invariant rather than an incidental consequence of the dot-prefix rule.
pub const PACKAGES_DIR_NAME: &str = ".penpot-packages";

use std::path::PathBuf;

/// Errors produced by sync-core operations. Every filesystem / JSON error
/// carries the path it happened on.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid JSON in {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("zip entry has unsafe path (zip-slip?): {0:?}")]
    UnsafeZipPath(String),
    #[error("manifest schema version {found} is not supported (expected {expected})")]
    ManifestSchema { found: u32, expected: u32 },
    #[error("lock.json schema version {found} is not supported (expected {expected})")]
    LockSchema { found: u32, expected: u32 },
    #[error("plugin-consent.json schema version {found} is not supported (expected {expected})")]
    ConsentSchema { found: u32, expected: u32 },
    #[error("{0}")]
    Swap(String),
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
