//! Deterministic zip / safe unzip for binfile-v3 directories.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use sync_core::{read_tree, unzip_to, zip_dir};

fn write_tree(dir: &Path, files: &[(&str, &[u8])]) {
    for (rel, content) in files {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

#[test]
fn zip_unzip_round_trip_preserves_tree_exactly() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write_tree(
        &src,
        &[
            ("manifest.json", br#"{"v": 3}"# as &[u8]),
            ("files/abc.json", b"{}"),
            ("files/abc/pages/p1.json", b"[1, 2]"),
            ("objects/blob.bin", &[0u8, 1, 2, 255, 254]),
            ("objects/empty.bin", b""),
        ],
    );
    let zipped = zip_dir(&src).unwrap();
    let dest = tmp.path().join("dest");
    unzip_to(&zipped, &dest).unwrap();
    assert_eq!(read_tree(&src).unwrap(), read_tree(&dest).unwrap());
}

#[test]
fn zip_is_deterministic_regardless_of_write_order_and_mtime() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    // Same content, different creation order and different mtimes.
    write_tree(
        &a,
        &[
            ("z/last.json", b"{}" as &[u8]),
            ("a/first.json", b"[]"),
            ("bin.dat", &[7u8; 100]),
        ],
    );
    write_tree(
        &b,
        &[
            ("bin.dat", &[7u8; 100] as &[u8]),
            ("a/first.json", b"[]"),
            ("z/last.json", b"{}"),
        ],
    );
    // Nudge mtimes apart.
    let f = fs::OpenOptions::new()
        .append(true)
        .open(b.join("bin.dat"))
        .unwrap();
    drop(f);
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(b.join("z/last.json"), b"{}").unwrap();

    let zip_a = zip_dir(&a).unwrap();
    let zip_b = zip_dir(&b).unwrap();
    assert_eq!(zip_a, zip_b, "zip bytes must be identical");
    // And stable across repeated calls.
    assert_eq!(zip_dir(&a).unwrap(), zip_a);
}

#[test]
fn unzip_rejects_zip_slip_entries() {
    // Hand-build a zip with a path-traversal entry.
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    writer.start_file("../evil.txt", options).unwrap();
    writer.write_all(b"pwned").unwrap();
    let bytes = writer.finish().unwrap().into_inner();

    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("inner").join("dest");
    let err = unzip_to(&bytes, &dest).unwrap_err();
    assert!(
        matches!(err, sync_core::Error::UnsafeZipPath(ref p) if p == "../evil.txt"),
        "got: {err}"
    );
    assert!(!tmp.path().join("inner/evil.txt").exists());
    assert!(!tmp.path().join("evil.txt").exists());
}

#[test]
fn unzip_handles_real_penpot_export() {
    let zip_bytes = fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/export-a.zip"),
    )
    .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    unzip_to(&zip_bytes, tmp.path()).unwrap();
    let files = read_tree(tmp.path()).unwrap();
    assert_eq!(files.len(), 6);
    assert!(files.contains_key("manifest.json"));
    // Re-zip deterministically and round-trip again: extracted trees equal.
    let rezipped = zip_dir(tmp.path()).unwrap();
    let tmp2 = tempfile::tempdir().unwrap();
    unzip_to(&rezipped, tmp2.path()).unwrap();
    assert_eq!(files, read_tree(tmp2.path()).unwrap());
}
