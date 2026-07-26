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

#[cfg(test)]
mod tests {
    //! What this crate can and cannot test without a live stack.
    //!
    //! Three of the four public functions here take a `&PenpotClient` and do
    //! nothing but call it; this workspace has no mocking layer, so a unit test
    //! of `settle_to_fixpoint` would be a test of a fake. They stay covered by
    //! the live gates.
    //!
    //! [`unzip_normalize_hash`] is the exception, and not a trivial one: it is
    //! the only pure logic in the crate, and the settle loop rests on the pair
    //! it returns. `settle_to_fixpoint` compares the HASH across cycles while
    //! re-importing the ZIP. If those two describe different trees, "two
    //! consecutive equal hashes ⇒ the export is a fixpoint" is false, and the
    //! first on-disk export the sync daemon writes is not one — the N6 P0 the
    //! module doc opens with.
    //!
    //! **The two tests below catch disjoint faults, and neither subsumes the
    //! other.** `semantic_tree_hash` re-normalizes as it hashes, so a raw tree
    //! and its normalized form hash IDENTICALLY — which means returning the
    //! un-normalized zip passes the pairing test and fails only the bytes test
    //! (verified by mutation: moving `zip_dir` above `normalize_tree` fails the
    //! second and leaves the first green). Conversely a zip built from the wrong
    //! or a partial tree fails both. Written as one test, half of this would be
    //! untested and nothing would say so.
    //!
    //! This is deliberately NOT whole-crate coverage. `scripts/c1-crates.sh`
    //! prints each crate's test count precisely so a number this small stays
    //! visible instead of reading as "installer is tested".

    use super::*;
    use std::path::Path;

    const BINARY: [u8; 4] = [0x89, 0x50, 0x00, 0xff];

    /// A binfile-shaped tree zipped the way an export actually arrives: one
    /// deliberately UN-normalized `.json` (keys out of order, 4-space indent,
    /// no trailing newline, a volatile field) plus one binary that must survive
    /// byte-identical.
    fn un_normalized_export(dir: &Path) -> Vec<u8> {
        std::fs::create_dir_all(dir.join("files")).unwrap();
        std::fs::create_dir_all(dir.join("objects")).unwrap();
        std::fs::write(
            dir.join("files/a.json"),
            b"{\n    \"zeta\": 2,\n    \"createdAt\": \"2020-01-01\",\n    \"alpha\": 1\n}",
        )
        .unwrap();
        std::fs::write(dir.join("objects/blob.bin"), BINARY).unwrap();
        sync_core::zip_dir(dir).unwrap()
    }

    /// The pairing. Whatever ordering the body uses internally, the hash it
    /// hands back must be the hash OF the zip it hands back — otherwise the
    /// settle loop is comparing one tree and re-importing another.
    #[test]
    fn the_returned_hash_describes_the_returned_zip() {
        let src = tempfile::tempdir().unwrap();
        let zip = un_normalized_export(src.path());

        let (hash, norm_zip) = unzip_normalize_hash(zip).unwrap();

        let landed = tempfile::tempdir().unwrap();
        sync_core::unzip_to(&norm_zip, landed.path()).unwrap();
        assert_eq!(
            sync_core::semantic_tree_hash(landed.path()).unwrap(),
            hash,
            "the hash returned must describe the zip returned"
        );
    }

    /// The other half, which the pairing alone cannot catch: the semantic view
    /// re-normalizes before hashing, so a zip of RAW bytes would still hash
    /// equal above. But these bytes are what gets re-imported (and, downstream,
    /// what the daemon writes), so they must already be normalized on disk —
    /// sorted keys, 2-space indent, trailing newline — with binaries untouched.
    #[test]
    fn the_returned_zip_carries_normalized_bytes_not_only_a_normalized_hash() {
        let src = tempfile::tempdir().unwrap();
        let zip = un_normalized_export(src.path());

        let (_, norm_zip) = unzip_normalize_hash(zip).unwrap();

        let landed = tempfile::tempdir().unwrap();
        sync_core::unzip_to(&norm_zip, landed.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(landed.path().join("files/a.json")).unwrap(),
            "{\n  \"alpha\": 1,\n  \"createdAt\": \"2020-01-01\",\n  \"zeta\": 2\n}\n",
            "normalization must be in the returned BYTES, not only in the hash"
        );
        assert_eq!(
            std::fs::read(landed.path().join("objects/blob.bin")).unwrap(),
            BINARY,
            "a non-JSON asset must round-trip byte-identical"
        );
    }
}
