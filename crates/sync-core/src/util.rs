//! Small shared helpers: recursive file walk with stable relative paths,
//! unique suffixes for tmp/old names, atomic file writes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Error, Result};

/// All regular files under `root`, depth-first, in no particular order.
pub(crate) fn walk_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| Error::io(&dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| Error::io(&dir, e))?;
            let path = entry.path();
            let ft = entry.file_type().map_err(|e| Error::io(&path, e))?;
            if ft.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    Ok(out)
}

/// Relative path of `path` under `root`, with `/` separators (matches the
/// Python side's `relpath.replace(os.sep, "/")`).
pub(crate) fn rel_path(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// 12 lowercase hex chars, unique per call within a process and effectively
/// unique across processes (time ^ pid ^ counter through a splitmix64 round).
pub(crate) fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seed = nanos
        ^ (u64::from(std::process::id())).rotate_left(32)
        ^ COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("{:012x}", splitmix64(seed) & 0xffff_ffff_ffff)
}

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// True iff `s` is exactly 12 lowercase hex chars (the [`unique_suffix`] shape).
pub(crate) fn is_suffix(s: &str) -> bool {
    s.len() == 12 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Write `bytes` to `path` atomically: write a sibling `<name>.tmp-<suffix>`,
/// fsync it, then rename over `path`.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!("{file_name}.tmp-{}", unique_suffix()));
    let mut f = std::fs::File::create(&tmp).map_err(|e| Error::io(&tmp, e))?;
    f.write_all(bytes).map_err(|e| Error::io(&tmp, e))?;
    f.sync_all().map_err(|e| Error::io(&tmp, e))?;
    drop(f);
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::io(path, e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixes_are_unique_and_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            let s = unique_suffix();
            assert!(is_suffix(&s), "bad suffix {s}");
            assert!(seen.insert(s), "duplicate suffix");
        }
        assert!(!is_suffix("abc"));
        assert!(!is_suffix("ABCDEF123456"));
        assert!(!is_suffix("ghijklmnopqr"));
    }
}
