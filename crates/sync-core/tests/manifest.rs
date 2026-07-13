//! `.penpot-sync.json` model: round-trip, atomic save, schema versioning.

use sync_core::{Manifest, ManifestEntry, MANIFEST_FILE_NAME, MANIFEST_SCHEMA_VERSION};

fn sample() -> Manifest {
    let mut m = Manifest::default();
    m.files.insert(
        "3a4be581-6d37-8010-8008-51f0c6eb307f".to_string(),
        ManifestEntry {
            path: "client-x/homepage.penpot".to_string(),
            project_id: "e4ebd8e6-e0d6-8139-8008-51ec9531fcd2".to_string(),
            project_name: "client-x".to_string(),
            revn: 7,
            db_modified_at: "2026-07-13T09:04:40.123Z".to_string(),
            last_synced_hash:
                "b2124a9b263292b7416d44db6f3c0a11328968917dc29987c1c386a9503d31b0".to_string(),
            last_synced_at: "2026-07-13T09:04:42Z".to_string(),
        },
    );
    m
}

#[test]
fn save_load_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let m = sample();
    m.save(tmp.path()).unwrap();
    let loaded = Manifest::load(tmp.path()).unwrap().expect("manifest exists");
    assert_eq!(loaded, m);
    assert_eq!(loaded.schema_version, MANIFEST_SCHEMA_VERSION);
    assert_eq!(
        loaded
            .entry_by_path("client-x/homepage.penpot")
            .map(|(id, _)| id),
        Some("3a4be581-6d37-8010-8008-51f0c6eb307f")
    );
    assert!(loaded.entry_by_path("nope.penpot").is_none());
}

#[test]
fn load_missing_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(Manifest::load(tmp.path()).unwrap().is_none());
}

#[test]
fn on_disk_form_is_camel_case_normalized_and_stable() {
    let tmp = tempfile::tempdir().unwrap();
    sample().save(tmp.path()).unwrap();
    let raw = std::fs::read_to_string(tmp.path().join(MANIFEST_FILE_NAME)).unwrap();
    // camelCase wire names, versioned schema field.
    assert!(raw.contains("\"schemaVersion\": 1"));
    assert!(raw.contains("\"lastSyncedHash\""));
    assert!(raw.contains("\"lastSyncedAt\""));
    assert!(raw.contains("\"projectId\""));
    assert!(!raw.contains("last_synced_hash"));
    // Written with the shared normalizer: LF + trailing newline + idempotent.
    assert!(raw.ends_with('\n'));
    assert!(!raw.contains('\r'));
    let renorm = sync_core::normalize_json_bytes(raw.as_bytes(), std::path::Path::new("m")).unwrap();
    assert_eq!(renorm, raw.as_bytes());
}

#[test]
fn save_is_atomic_no_tmp_left_and_replaces_previous() {
    let tmp = tempfile::tempdir().unwrap();
    let mut m = sample();
    m.save(tmp.path()).unwrap();
    m.files.get_mut("3a4be581-6d37-8010-8008-51f0c6eb307f").unwrap().revn = 8;
    m.save(tmp.path()).unwrap();
    let names: Vec<String> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![MANIFEST_FILE_NAME.to_string()], "tmp file leaked");
    assert_eq!(
        Manifest::load(tmp.path()).unwrap().unwrap().files["3a4be581-6d37-8010-8008-51f0c6eb307f"]
            .revn,
        8
    );
}

#[test]
fn m2_manifest_without_db_modified_at_still_loads() {
    // Manifests written before M3 lack the dbModifiedAt key; the field
    // defaults to "" (= unknown → revn-only DB-moved heuristic).
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join(MANIFEST_FILE_NAME),
        br#"{
  "files": {
    "f1": {
      "lastSyncedAt": "2026-07-13T09:04:42Z",
      "lastSyncedHash": "abc",
      "path": "p/x.penpot",
      "projectId": "p1",
      "projectName": "p",
      "revn": 3
    }
  },
  "schemaVersion": 1
}"#,
    )
    .unwrap();
    let m = Manifest::load(tmp.path()).unwrap().unwrap();
    assert_eq!(m.files["f1"].db_modified_at, "");
    assert_eq!(m.files["f1"].revn, 3);
}

#[test]
fn unknown_schema_version_is_a_hard_error() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join(MANIFEST_FILE_NAME),
        br#"{"schemaVersion": 99, "files": {}}"#,
    )
    .unwrap();
    let err = Manifest::load(tmp.path()).unwrap_err();
    assert!(
        matches!(err, sync_core::Error::ManifestSchema { found: 99, expected: 1 }),
        "got: {err}"
    );
}

#[test]
fn corrupt_manifest_is_an_error_not_a_silent_reset() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join(MANIFEST_FILE_NAME), b"{not json").unwrap();
    assert!(Manifest::load(tmp.path()).is_err());
}

#[test]
fn now_rfc3339_shape() {
    let now = sync_core::manifest::now_rfc3339();
    // e.g. 2026-07-13T12:34:56Z
    assert_eq!(now.len(), 20, "unexpected: {now}");
    assert!(now.starts_with("20"));
    assert!(now.ends_with('Z'));
    assert_eq!(&now[4..5], "-");
    assert_eq!(&now[10..11], "T");
}
