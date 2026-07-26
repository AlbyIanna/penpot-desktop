//! Tray-side runner for the git-versioning helper (M5).
//!
//! `scripts/designs-git-init.sh` is THE implementation (also exposed as
//! `just git-init <designs-dir>` for headless use); the tray action
//! "Enable git versioning" runs the **same script**, embedded at compile
//! time so a packaged app doesn't need the repo checkout. The script is
//! idempotent: init-if-needed, write-once .gitignore + DESIGNS-README.md,
//! initial commit only when the repo was created by this very run.

use std::path::Path;
use std::process::Command;

use anyhow::Context;

/// The embedded helper script (single source of truth: `scripts/`).
pub const GIT_INIT_SCRIPT: &str = include_str!("../../../scripts/designs-git-init.sh");

/// Run the embedded script against `designs_dir`. Blocking (git is fast, but
/// call it off the UI thread); returns the script's stdout on success.
pub fn run_git_init(designs_dir: &Path) -> anyhow::Result<String> {
    // Write the embedded script to a private temp file and hand it to bash.
    let dir = tempdir_for_script()?;
    let script_path = dir.join("designs-git-init.sh");
    std::fs::write(&script_path, GIT_INIT_SCRIPT)
        .with_context(|| format!("cannot write {}", script_path.display()))?;

    let output = Command::new("bash")
        .arg(&script_path)
        .arg(designs_dir)
        .output()
        .context("failed to spawn bash for designs-git-init.sh")?;
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_dir(&dir);

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if output.status.success() {
        Ok(stdout)
    } else {
        anyhow::bail!(
            "designs-git-init.sh failed (status {}): {}",
            output.status,
            if stderr.trim().is_empty() { &stdout } else { &stderr }
        )
    }
}

fn tempdir_for_script() -> anyhow::Result<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "penpot-local-gitinit-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded script must keep teaching the M3-verified git lessons —
    /// these strings are load-bearing documentation, not decoration.
    #[test]
    fn embedded_script_carries_the_load_bearing_content() {
        for needle in [
            "--no-overlay",
            "git restore --source=",
            ".penpot-sync.json.tmp-*",
            "*.penpot.tmp-*/",
            "*.penpot.old-*/",
            ".DS_Store",
            "DESIGNS-README.md",
            "conflict",
        ] {
            assert!(
                GIT_INIT_SCRIPT.contains(needle),
                "designs-git-init.sh lost required content: {needle}"
            );
        }
        // Conflict copies and exports must NOT be ignored: no ignore rule may
        // target them (the only mentions live in comments explaining that).
        for line in GIT_INIT_SCRIPT.lines() {
            let l = line.trim();
            if l.starts_with('#') || l.is_empty() {
                continue;
            }
            assert!(
                !(l.starts_with("*.conflict") || l.starts_with("*.exports")),
                "gitignore template must not ignore conflicts/exports, found: {l}"
            );
        }
    }

    /// End-to-end through the embedded copy: fresh dir → repo + files +
    /// exactly one commit; second run → no changes, no second commit;
    /// never commits into a pre-existing repo.
    #[test]
    fn run_git_init_is_idempotent_and_respects_existing_repos() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("git not available; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();

        // Fresh designs dir.
        let designs = tmp.path().join("designs");
        std::fs::create_dir_all(designs.join("Client/home.penpot")).unwrap();
        std::fs::write(designs.join("Client/home.penpot/manifest.json"), "{}").unwrap();
        run_git_init(&designs).expect("first run");
        let commits = |dir: &Path| {
            Command::new("git")
                .args(["-C", &dir.to_string_lossy(), "rev-list", "--count", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        };
        assert_eq!(commits(&designs).as_deref(), Some("1"), "fresh repo gets 1 commit");
        assert!(designs.join(".gitignore").is_file());
        assert!(designs.join("DESIGNS-README.md").is_file());

        // Second run: still exactly one commit, files untouched.
        let gitignore_before = std::fs::read_to_string(designs.join(".gitignore")).unwrap();
        run_git_init(&designs).expect("second run");
        assert_eq!(commits(&designs).as_deref(), Some("1"), "idempotent: no second commit");
        assert_eq!(
            std::fs::read_to_string(designs.join(".gitignore")).unwrap(),
            gitignore_before
        );

        // Pre-existing (empty) repo: helper files written, but NO commit.
        let pre = tmp.path().join("pre-existing");
        std::fs::create_dir_all(&pre).unwrap();
        assert!(Command::new("git")
            .args(["-C", &pre.to_string_lossy(), "init", "-q"])
            .status()
            .unwrap()
            .success());
        run_git_init(&pre).expect("run on pre-existing repo");
        assert!(pre.join(".gitignore").is_file());
        assert!(
            commits(&pre).is_none(),
            "must never commit into a pre-existing repo (even an empty one)"
        );
    }

    #[test]
    fn missing_designs_dir_is_a_clear_error() {
        let err = run_git_init(Path::new("/nonexistent/designs-dir-xyz")).unwrap_err();
        assert!(err.to_string().contains("designs-git-init.sh failed"));
    }
}
