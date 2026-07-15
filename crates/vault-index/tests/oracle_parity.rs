//! E1 oracle parity: the Rust contract classifier must produce the SAME
//! patch/minor/major verdicts as the throwaway spike python oracle
//! (`scripts/ecosystem-spike/{extract_contract,diff_contracts}.py`) on the
//! spike's delta matrix. This drives the REAL python oracle over the authored
//! fixture (`scripts/e1-fixture.py`) and compares it to the built `contract`
//! CLI, so it is genuine parity, not a recording.
//!
//! Guarded on `python3` being present (a hard dependency of the whole test
//! harness, per CLAUDE.md — `roundtrip.py` et al.); if absent the test prints
//! a notice and returns, so a constrained CI never goes red on it.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/crates/vault-index
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

fn have_python3() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `contract diff <a> <b>` -> the bump word (PATCH/MINOR/MAJOR/MIGRATION).
fn rust_bump(a: &Path, b: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_contract"))
        .args(["diff", a.to_str().unwrap(), b.to_str().unwrap()])
        .output()
        .expect("run contract bin");
    assert!(out.status.success(), "contract diff failed: {out:?}");
    parse_bump(&String::from_utf8_lossy(&out.stdout))
}

/// The python oracle's overall bump over the same two trees.
fn oracle_bump(spike: &Path, tmp: &Path, a: &Path, b: &Path) -> String {
    let ea = tmp.join("oa.json");
    let eb = tmp.join("ob.json");
    for (tree, out) in [(a, &ea), (b, &eb)] {
        let s = Command::new("python3")
            .args([
                spike.join("extract_contract.py").to_str().unwrap(),
                tree.to_str().unwrap(),
                "--json",
                out.to_str().unwrap(),
            ])
            .output()
            .expect("run extract_contract.py");
        assert!(s.status.success(), "extract_contract.py failed: {s:?}");
    }
    let d = Command::new("python3")
        .args([
            spike.join("diff_contracts.py").to_str().unwrap(),
            ea.to_str().unwrap(),
            eb.to_str().unwrap(),
        ])
        .output()
        .expect("run diff_contracts.py");
    assert!(d.status.success(), "diff_contracts.py failed: {d:?}");
    parse_bump(&String::from_utf8_lossy(&d.stdout))
}

fn parse_bump(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("OVERALL BUMP: "))
        .unwrap_or("<none>")
        .trim()
        .to_string()
}

#[test]
fn rust_classifier_matches_python_oracle_on_the_delta_matrix() {
    if !have_python3() {
        eprintln!("SKIP oracle parity: python3 not available");
        return;
    }
    let root = repo_root();
    let spike = root.join("scripts/ecosystem-spike");
    let fixture = root.join("scripts/e1-fixture.py");
    if !fixture.exists() || !spike.join("diff_contracts.py").exists() {
        eprintln!("SKIP oracle parity: fixture/oracle scripts absent");
        return;
    }

    let tmp = std::env::temp_dir().join(format!("e1-oracle-parity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let gen = Command::new("python3")
        .arg(&fixture)
        .arg(&tmp)
        .output()
        .expect("generate fixture");
    assert!(gen.status.success(), "fixture gen failed: {gen:?}");

    let base = tmp.join("baseline");
    // The spike's delta matrix — the cases where Rust and the oracle MUST agree.
    let matrix = [
        ("delta-patch", "PATCH"),
        ("delta-minor", "MINOR"),
        ("delta-major-removed", "MAJOR"),
        ("delta-major-renamed", "MAJOR"),
    ];
    for (delta, want) in matrix {
        let d = tmp.join(delta);
        let r = rust_bump(&base, &d);
        let o = oracle_bump(&spike, &tmp, &base, &d);
        assert_eq!(r, want, "rust verdict for {delta}");
        assert_eq!(o, want, "oracle verdict for {delta}");
        assert_eq!(r, o, "rust/oracle parity for {delta}");
    }

    let _ = std::fs::remove_dir_all(&tmp);
}
