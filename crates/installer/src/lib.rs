//! Shared import-as-new + settle-until-fixpoint mechanism.
//!
//! Generalized from N6's `templates.rs::import_new_from_template` (PLAN3 E2:
//! "factor the shared import-as-new + settle path so templates and packages
//! both use it"). Templates are package-type #0; an E2 design-data package is
//! the same mechanism pointed at a `.penpot` source tree instead of a shipped
//! binfile. Both:
//!
//! 1. import-as-new (no file-id → mints a fresh id, remaps internal ids), and
//! 2. **settle until the DB's export is a round-trip fixpoint** — export →
//!    normalize → in-place re-import → re-export until two consecutive exports
//!    have an equal semantic hash (per `scripts/roundtrip.py`). This is the
//!    N6 P0: the first on-disk export the sync daemon later writes must already
//!    be a fixpoint, or the core invariant (delete the DB, rebuild from disk)
//!    surfaces a spurious change on the FIRST rebuild.
//!
//! The only difference between a template and a package here is the import
//! source: a template is raw binfile bytes with a format-dependent `version`
//! field; a package is a normalized `.penpot` directory tree that we zip into a
//! version-less v3 binfile before importing.

use penpot_rpc::PenpotClient;

/// Hard cap on settle cycles (each an in-place re-import) before we give up on
/// reaching a round-trip fixpoint. A clean file converges after 1 cycle,
/// migration/GC-heavy files after 2; 3 leaves headroom without looping forever.
pub const MAX_SETTLE_CYCLES: usize = 3;

/// Resolve the target project id for a new file: the team's default ("Drafts")
/// project, falling back to the first live project.
pub async fn default_project_id(client: &PenpotClient, team_id: &str) -> anyhow::Result<String> {
    let projects = client.get_projects(team_id).await?;
    let live = || projects.iter().filter(|p| p.deleted_at.is_none());
    if let Some(p) = live().find(|p| p.is_default) {
        return Ok(p.id.clone());
    }
    live()
        .map(|p| p.id.clone())
        .next()
        .ok_or_else(|| anyhow::anyhow!("no live project to import into for team {team_id}"))
}

/// import-as-new a binfile (`bytes`, with an optional legacy `version` field)
/// into `project_id`, then settle to a round-trip fixpoint. Returns the minted
/// `(file_id, settle_cycles)`.
pub async fn import_binfile_and_settle(
    client: &PenpotClient,
    project_id: &str,
    display: &str,
    bytes: Vec<u8>,
    version: Option<u8>,
) -> anyhow::Result<(String, usize)> {
    let ids = client
        .import_binfile_versioned(display, project_id, None, bytes, version)
        .await?;
    let file_id = ids
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("import-binfile returned no file id"))?;
    let settle_cycles = settle_to_fixpoint(client, &file_id, project_id, display).await?;
    Ok((file_id, settle_cycles))
}

/// Result of unzipping + normalizing one export: its semantic tree hash and the
/// deterministically re-zipped normalized tree (the exact bytes the daemon
/// would write to disk, ready to re-import in place). Computed off the async
/// runtime because it walks/parses the whole tree.
fn unzip_normalize_hash(zip: Vec<u8>) -> anyhow::Result<(String, Vec<u8>)> {
    let dir = tempfile::tempdir()?;
    sync_core::unzip_to(&zip, dir.path())?;
    sync_core::normalize_tree(dir.path())?;
    let hash = sync_core::semantic_tree_hash(dir.path())?;
    let norm_zip = sync_core::zip_dir(dir.path())?;
    Ok((hash, norm_zip))
}

/// Run export → normalize → in-place re-import → re-export cycles until two
/// consecutive exports have an equal semantic hash — i.e. the DB's current
/// export is its own round-trip fixpoint (per `scripts/roundtrip.py` semantics).
/// Returns the number of in-place re-import cycles run (>= 1). Errors if it does
/// not converge within [`MAX_SETTLE_CYCLES`].
pub async fn settle_to_fixpoint(
    client: &PenpotClient,
    file_id: &str,
    project_id: &str,
    display: &str,
) -> anyhow::Result<usize> {
    let mut prev_hash: Option<String> = None;
    let mut cycles = 0usize;
    loop {
        // Export the CURRENT DB state (embed_assets=true matches the daemon).
        let exported = client.export_binfile(file_id, false, true).await?;
        let zip = client.download_exported_binfile(&exported.uri).await?;
        let (hash, norm_zip) =
            tokio::task::spawn_blocking(move || unzip_normalize_hash(zip)).await??;

        if prev_hash.as_deref() == Some(hash.as_str()) {
            // Two consecutive exports equal → the current export is a fixpoint.
            return Ok(cycles);
        }
        if cycles >= MAX_SETTLE_CYCLES {
            anyhow::bail!(
                "file {file_id} did not reach a round-trip fixpoint after \
                 {MAX_SETTLE_CYCLES} settle cycles (last semantic hash {hash})"
            );
        }

        // Not yet stable: re-import the normalized export in place and loop.
        client
            .import_binfile(display, project_id, Some(file_id), norm_zip)
            .await?;
        prev_hash = Some(hash);
        cycles += 1;
    }
}

/// Best-effort fetch of a file's first page id (`data.pages[0]`) for a deep
/// link. A missing page id just yields a workspace link without `page-id`.
pub async fn first_page_id(client: &PenpotClient, file_id: &str) -> Option<String> {
    let file = client.get_file(file_id).await.ok()?;
    file.get("data")?
        .get("pages")?
        .as_array()?
        .first()?
        .as_str()
        .map(str::to_string)
}
