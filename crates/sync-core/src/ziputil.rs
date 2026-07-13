//! Zip/unzip helpers for binfile-v3 directories.
//!
//! [`zip_dir`] is deterministic: entries sorted by relative path, fixed
//! timestamps (1980-01-01), fixed permissions, stable compression. The Penpot
//! backend doesn't care, but our tests (and any content-addressed cache) do.
//! Remember the invariant: zips are transport only — never hash or compare
//! the container, only extracted trees.

use std::io::{Cursor, Read, Write};
use std::path::Path;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::{Error, Result};

/// Deterministically zip every file under `root` (relative paths with `/`,
/// sorted; directories are implicit — no dir entries, matching what the
/// Python spike produced and what Penpot exports look like).
pub fn zip_dir(root: &Path) -> Result<Vec<u8>> {
    let files = crate::hash::read_tree(root)?; // BTreeMap => sorted
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default()) // 1980-01-01 00:00:00
        .unix_permissions(0o644);
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for (rel, content) in &files {
        writer.start_file(rel, options)?;
        writer
            .write_all(content)
            .map_err(|e| Error::io(root.join(rel), e))?;
    }
    let cursor = writer.finish()?;
    Ok(cursor.into_inner())
}

/// Unzip `bytes` into `dest` (created if missing). Entry paths are validated
/// against zip-slip: any entry that would escape `dest` is a hard error.
pub fn unzip_to(bytes: &[u8], dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).map_err(|e| Error::io(dest, e))?;
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let raw_name = entry.name().to_owned();
        let Some(rel) = entry.enclosed_name() else {
            return Err(Error::UnsafeZipPath(raw_name));
        };
        let out_path = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path).map_err(|e| Error::io(&out_path, e))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let mut content = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut content)
            .map_err(|e| Error::io(&out_path, e))?;
        std::fs::write(&out_path, &content).map_err(|e| Error::io(&out_path, e))?;
    }
    Ok(())
}
