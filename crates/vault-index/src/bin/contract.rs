//! `contract` — the E1 lint CLI: extract a package's contract from a
//! normalized `.penpot` tree on disk, diff two versions, or churn a tree's
//! uuids to prove id-invariance. Pure/offline; no Penpot stack.
//!
//! Reads the SAME normalized bytes the ledger hashes
//! (`sync_core::read_tree` + `semantic_view`), so the lint answers exactly
//! "did this on-disk edit break my library's contract?".
//!
//! Usage:
//!   contract extract <tree-dir>              # print the contract as JSON
//!   contract diff    <before-dir> <after-dir># print OVERALL BUMP + JSON
//!   contract churn   <src-dir> <dst-dir>     # copy, remapping every uuid
//!
//! `diff` prints a leading `OVERALL BUMP: PATCH|MINOR|MAJOR|MIGRATION` line
//! (ANSI-free) for shell greps, matching the spike oracle's output line.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;

use vault_index::{diff_contracts, extract_contracts, LibraryContract};

fn read_normalized(dir: &Path) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let raw = sync_core::read_tree(dir)?;
    let sem = sync_core::semantic_view(&raw)?;
    Ok(sem)
}

fn extract_dir(dir: &Path) -> anyhow::Result<LibraryContract> {
    Ok(extract_contracts(&read_normalized(dir)?))
}

fn cmd_extract(dir: &str) -> anyhow::Result<()> {
    let lib = extract_dir(Path::new(dir))?;
    println!("{}", serde_json::to_string_pretty(&lib.to_json())?);
    Ok(())
}

fn cmd_diff(before: &str, after: &str) -> anyhow::Result<()> {
    let b = extract_dir(Path::new(before))?;
    let a = extract_dir(Path::new(after))?;
    let cls = diff_contracts(&b, &a);
    // The grep line first (upper-case, like diff_contracts.py's "OVERALL BUMP").
    println!("OVERALL BUMP: {}", cls.overall.as_str().to_uppercase());
    println!("{}", serde_json::to_string_pretty(&cls.to_json())?);
    Ok(())
}

/// Consistently remap every uuid in a tree (and in uuid-named file paths) to a
/// fresh one — the cheap, stack-free simulation of import-as-new's per-DB id
/// churn (PLAN3: "you may simulate uuid churn by remapping all uuids").
fn cmd_churn(src: &str, dst: &str) -> anyhow::Result<()> {
    let files = sync_core::read_tree(Path::new(src))?;
    let mut mapping: BTreeMap<String, String> = BTreeMap::new();

    // First pass: discover every uuid across the whole tree (paths + bodies)
    // so the file names and the JSON that references them remap identically.
    for (rel, bytes) in &files {
        discover_uuids(rel, &mut mapping);
        if let Ok(text) = std::str::from_utf8(bytes) {
            discover_uuids(text, &mut mapping);
        }
    }

    let dst_root = Path::new(dst);
    if dst_root.exists() {
        std::fs::remove_dir_all(dst_root)?;
    }
    for (rel, bytes) in &files {
        let new_rel = remap_all(rel, &mapping);
        let out = dst_root.join(&new_rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let new_bytes = match std::str::from_utf8(bytes) {
            Ok(text) => remap_all(text, &mapping).into_bytes(),
            Err(_) => bytes.clone(),
        };
        std::fs::write(&out, new_bytes)?;
    }
    eprintln!("churned {} files, {} distinct uuids remapped", files.len(), mapping.len());
    Ok(())
}

/// Is `s[i..]` the start of a `8-4-4-4-12` hex-dash uuid? Returns its length.
fn uuid_len_at(bytes: &[u8], i: usize) -> Option<usize> {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let is_hex = |b: u8| b.is_ascii_hexdigit();
    let mut pos = i;
    for (gi, &glen) in GROUPS.iter().enumerate() {
        if gi > 0 {
            if bytes.get(pos) != Some(&b'-') {
                return None;
            }
            pos += 1;
        }
        for _ in 0..glen {
            match bytes.get(pos) {
                Some(&b) if is_hex(b) => pos += 1,
                _ => return None,
            }
        }
    }
    // Reject a longer hex run masquerading as a uuid (e.g. trailing hex digit).
    if bytes.get(pos).is_some_and(|&b| is_hex(b)) {
        return None;
    }
    Some(pos - i)
}

fn discover_uuids(text: &str, mapping: &mut BTreeMap<String, String>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(len) = uuid_len_at(bytes, i) {
            let old = &text[i..i + len];
            if !mapping.contains_key(old) {
                let n = (mapping.len() as u64) + 1;
                mapping.insert(
                    old.to_string(),
                    format!("{:08x}-{:04x}-4000-8000-{:012x}", 0xA11CE ^ n, n & 0xffff, n),
                );
            }
            i += len;
        } else {
            i += 1;
        }
    }
}

fn remap_all(text: &str, mapping: &BTreeMap<String, String>) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(len) = uuid_len_at(bytes, i) {
            let old = &text[i..i + len];
            match mapping.get(old) {
                Some(new) => out.push_str(new),
                None => out.push_str(old),
            }
            i += len;
        } else {
            // Copy one char (handle multi-byte UTF-8 safely).
            let ch_len = utf8_char_len(bytes[i]);
            out.push_str(&text[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [_, "extract", dir] => cmd_extract(dir),
        [_, "diff", before, after] => cmd_diff(before, after),
        [_, "churn", src, dst] => cmd_churn(src, dst),
        _ => {
            eprintln!(
                "usage:\n  contract extract <tree-dir>\n  contract diff <before-dir> <after-dir>\n  contract churn <src-dir> <dst-dir>"
            );
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("contract: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
