//! N4b "Checkpoint now" — the manual, git-native vault checkpoint verb.
//!
//! This is the concrete instance of the ecosystem values *surface, don't
//! apply* and *git repos, not a registry* (docs/ecosystem-concept.md): it
//! makes **one labeled commit** of the vault, **only on explicit user action**
//! (the palette/tray verb POSTs `/__api/vault/checkpoint`), and it obeys the
//! git-coexistence rule from PLAN2.md risk 5:
//!
//! - **Manual-only.** Never on a timer or daemon — the only caller is the
//!   HTTP verb, fired by a click. There is no auto-chronicle.
//! - **Never rewrites history.** It only ever *adds* a commit (or no-ops); it
//!   never amends, rebases, resets, or force-updates a ref.
//! - **Refuses loudly on a dirty/in-progress repo state** — mid-rebase, a
//!   pending merge/cherry-pick/revert, a detached HEAD, or unmerged conflict
//!   paths — with a clear message, so it can never fight the user's own git
//!   operation.
//! - **Clean no-op when nothing changed** since the last checkpoint.
//! - On a **fresh (no-repo) vault** it initializes the repo (via the shipped
//!   `designs-git-init.sh` machinery — same `.gitignore`/README) and makes
//!   exactly one commit containing the manifest + `.penpot` dirs.
//!
//! The decision is a pure function of the probed repo state ([`decide`]), so
//! the whole decision table — including the dirty-repo refusal — is unit
//! testable without a running stack; the git execution is exercised end-to-end
//! against real repos in tempdirs.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use http::StatusCode;
use serde_json::json;

/// The probed state of the vault's git repo — the sole input to [`decide`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoState {
    /// No `.git` at the vault root: a fresh vault.
    NoRepo,
    /// A normal repo on a branch with no in-progress operation. `has_changes`
    /// is true iff `git status --porcelain` is non-empty.
    Clean { has_changes: bool },
    /// An in-progress or conflicted state we must not touch. Carries a
    /// human-readable reason for the loud refusal.
    InProgress(String),
}

/// What [`checkpoint`] will do, decided purely from a [`RepoState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Fresh vault → init + exactly one commit.
    Init,
    /// Clean repo with staged/unstaged changes → one labeled commit.
    Commit,
    /// Clean repo, nothing changed since the last checkpoint → no-op.
    NoOp,
    /// Dirty/in-progress → refuse loudly (never touch the repo).
    Refuse(String),
}

impl Decision {
    /// The stable string the HTTP payload reports (`decision` field).
    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::Init => "init",
            Decision::Commit => "commit",
            Decision::NoOp => "noop",
            Decision::Refuse(_) => "refused",
        }
    }
}

/// THE decision table (pure): map a probed repo state to the action. This is
/// the whole of the "surface, don't apply / never fight the user's git" policy
/// in one total function.
pub fn decide(state: &RepoState) -> Decision {
    match state {
        RepoState::NoRepo => Decision::Init,
        RepoState::InProgress(reason) => Decision::Refuse(reason.clone()),
        RepoState::Clean { has_changes: true } => Decision::Commit,
        RepoState::Clean { has_changes: false } => Decision::NoOp,
    }
}

/// Run `git` in `dir`, returning (success, stdout-trimmed, stderr-trimmed).
fn git(dir: &Path, args: &[&str]) -> std::io::Result<(bool, String, String)> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ))
}

/// Locate the repo's git dir (handles a `.git` file for worktrees) — used to
/// look for in-progress operation markers. Falls back to `<dir>/.git`.
fn git_dir(dir: &Path) -> PathBuf {
    if let Ok((true, out, _)) = git(dir, &["rev-parse", "--absolute-git-dir"]) {
        if !out.is_empty() {
            return PathBuf::from(out);
        }
    }
    dir.join(".git")
}

/// True iff `git status --porcelain` reports any unmerged (conflict) path.
/// Conflict codes: DD, AU, UD, UA, DU, AA, UU (either side U, or AA/DD).
fn has_unmerged_paths(dir: &Path) -> bool {
    match git(dir, &["status", "--porcelain"]) {
        Ok((true, out, _)) => out.lines().any(|l| {
            let code = l.get(0..2).unwrap_or("");
            let (x, y) = (code.chars().next().unwrap_or(' '), code.chars().nth(1).unwrap_or(' '));
            x == 'U' || y == 'U' || code == "AA" || code == "DD"
        }),
        _ => false,
    }
}

/// Probe the vault's git state. Never mutates anything.
pub fn probe(vault: &Path) -> RepoState {
    // A fresh vault has no `.git` entry at its root (matches the gitinit
    // machinery's "fresh repo" definition).
    if !vault.join(".git").exists() {
        return RepoState::NoRepo;
    }
    let gd = git_dir(vault);
    // In-progress operations we must not disturb (PLAN2.md risk 5).
    let in_progress = [
        ("rebase-merge", "a rebase is in progress"),
        ("rebase-apply", "a rebase/am is in progress"),
        ("MERGE_HEAD", "a merge is in progress"),
        ("CHERRY_PICK_HEAD", "a cherry-pick is in progress"),
        ("REVERT_HEAD", "a revert is in progress"),
        ("BISECT_LOG", "a bisect is in progress"),
    ];
    for (marker, reason) in in_progress {
        if gd.join(marker).exists() {
            return RepoState::InProgress(reason.to_string());
        }
    }
    // Detached HEAD: symbolic-ref fails on a detached HEAD but SUCCEEDS on an
    // unborn branch (fresh `git init`, no commits yet), which is fine to
    // commit onto.
    let unborn = matches!(git(vault, &["rev-parse", "--verify", "HEAD"]), Ok((false, _, _)));
    if !unborn {
        let detached = matches!(git(vault, &["symbolic-ref", "-q", "HEAD"]), Ok((false, _, _)));
        if detached {
            return RepoState::InProgress("HEAD is detached".to_string());
        }
    }
    // Unmerged conflict paths in the working tree.
    if has_unmerged_paths(vault) {
        return RepoState::InProgress("unresolved merge conflicts in the working tree".to_string());
    }
    let has_changes = match git(vault, &["status", "--porcelain"]) {
        Ok((true, out, _)) => !out.is_empty(),
        // If status itself fails, treat as no changes (nothing to commit) —
        // but this path is not expected for a valid repo.
        _ => false,
    };
    RepoState::Clean { has_changes }
}

/// Identity fallback args so a commit works on a machine with no global git
/// identity — never overrides an existing one.
fn identity_args(dir: &Path) -> Vec<String> {
    let mut args = Vec::new();
    if !matches!(git(dir, &["config", "user.email"]), Ok((true, s, _)) if !s.is_empty()) {
        args.push("-c".into());
        args.push("user.email=penpot-local@localhost".into());
    }
    if !matches!(git(dir, &["config", "user.name"]), Ok((true, s, _)) if !s.is_empty()) {
        args.push("-c".into());
        args.push("user.name=Penpot Local".into());
    }
    args
}

/// The outcome of a checkpoint run (serialized into the HTTP payload).
#[derive(Debug, Clone)]
pub struct Outcome {
    pub decision: Decision,
    /// The new commit's short hash, when one was made.
    pub commit: Option<String>,
    /// A human-readable message for the surface.
    pub message: String,
}

/// Run a checkpoint against `vault` with commit `label`. Returns the outcome;
/// only ever *adds* a commit (Init/Commit) or does nothing (NoOp/Refuse).
pub fn checkpoint(vault: &Path, label: &str) -> anyhow::Result<Outcome> {
    let state = probe(vault);
    let decision = decide(&state);
    match &decision {
        Decision::Refuse(reason) => Ok(Outcome {
            message: format!("Checkpoint refused: {reason}. Resolve it in git, then try again."),
            decision,
            commit: None,
        }),
        Decision::NoOp => Ok(Outcome {
            decision,
            commit: head_short(vault),
            message: "Nothing changed since the last checkpoint.".to_string(),
        }),
        Decision::Init => {
            // Fresh vault: the shipped gitinit machinery inits + writes the
            // .gitignore/README + makes exactly one initial commit (its
            // `git add -A` captures the manifest + .penpot dirs).
            crate::gitinit::run_git_init(vault)?;
            let commit = head_short(vault);
            Ok(Outcome {
                decision,
                message: format!(
                    "Initialized git in the vault and made the first checkpoint{}.",
                    commit.as_deref().map(|c| format!(" ({c})")).unwrap_or_default()
                ),
                commit,
            })
        }
        Decision::Commit => {
            let (ok, _out, err) = git(vault, &["add", "-A"])?;
            if !ok {
                anyhow::bail!("git add failed: {err}");
            }
            let ident = identity_args(vault);
            let mut args: Vec<&str> = ident.iter().map(String::as_str).collect();
            args.extend_from_slice(&["commit", "--quiet", "-m", label]);
            let (ok, _out, err) = git(vault, &args)?;
            if !ok {
                anyhow::bail!("git commit failed: {err}");
            }
            let commit = head_short(vault);
            Ok(Outcome {
                decision,
                message: format!(
                    "Checkpoint committed{}.",
                    commit.as_deref().map(|c| format!(" ({c})")).unwrap_or_default()
                ),
                commit,
            })
        }
    }
}

/// The short hash of HEAD, if any (None on an unborn branch).
fn head_short(vault: &Path) -> Option<String> {
    match git(vault, &["rev-parse", "--short", "HEAD"]) {
        Ok((true, s, _)) if !s.is_empty() => Some(s),
        _ => None,
    }
}

/// A default checkpoint label with an RFC 3339 UTC timestamp.
pub fn default_label() -> String {
    format!("Checkpoint {}", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"))
}

// ---------------------------------------------------------------------------
// HTTP surface: POST /__api/vault/checkpoint
// ---------------------------------------------------------------------------

struct CheckpointState {
    vault_root: PathBuf,
}

/// The `/__api/vault/checkpoint` route (merged into the proxy's extra router).
pub fn router(vault_root: impl Into<PathBuf>) -> Router {
    let state = Arc::new(CheckpointState { vault_root: vault_root.into() });
    Router::new()
        .route("/__api/vault/checkpoint", post(checkpoint_action))
        .with_state(state)
}

async fn checkpoint_action(State(state): State<Arc<CheckpointState>>) -> Response {
    let vault = state.vault_root.clone();
    let label = default_label();
    let result = tokio::task::spawn_blocking(move || checkpoint(&vault, &label)).await;
    match result {
        Ok(Ok(outcome)) => {
            let refused = matches!(outcome.decision, Decision::Refuse(_));
            let body = json!({
                "ok": !refused,
                "decision": outcome.decision.as_str(),
                "commit": outcome.commit,
                "message": outcome.message,
            });
            if refused {
                // Loud refusal: a 409 the surface renders as an error.
                (StatusCode::CONFLICT, Json(body)).into_response()
            } else {
                Json(body).into_response()
            }
        }
        Ok(Err(e)) => {
            tracing::error!(error = format!("{e:#}"), "checkpoint failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "decision": "error", "message": format!("checkpoint failed: {e}")})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "checkpoint task panicked");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "decision": "error", "message": "checkpoint task panicked"})),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- pure decision table ----------------

    #[test]
    fn decision_table_is_total_and_correct() {
        assert_eq!(decide(&RepoState::NoRepo), Decision::Init);
        assert_eq!(decide(&RepoState::Clean { has_changes: true }), Decision::Commit);
        assert_eq!(decide(&RepoState::Clean { has_changes: false }), Decision::NoOp);
        let r = decide(&RepoState::InProgress("a merge is in progress".into()));
        assert_eq!(r, Decision::Refuse("a merge is in progress".into()));
        assert_eq!(r.as_str(), "refused");
    }

    // ---------------- end-to-end against real git repos ----------------

    fn git_available() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    fn run(dir: &Path, args: &[&str]) {
        let ident: Vec<&str> = vec![
            "-c", "user.email=t@t", "-c", "user.name=T",
        ];
        let mut full = ident.clone();
        full.extend_from_slice(args);
        let ok = Command::new("git").arg("-C").arg(dir).args(&full).status().unwrap().success();
        assert!(ok, "git {args:?} failed");
    }

    fn commit_count(dir: &Path) -> u32 {
        let out = Command::new("git")
            .args(["-C", &dir.to_string_lossy(), "rev-list", "--count", "HEAD"])
            .output()
            .unwrap();
        if !out.status.success() {
            return 0;
        }
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
    }

    fn seed_vault(dir: &Path) {
        std::fs::create_dir_all(dir.join("Client/home.penpot/files")).unwrap();
        std::fs::write(dir.join("Client/home.penpot/manifest.json"), "{}").unwrap();
        std::fs::write(dir.join(".penpot-sync.json"), "{\"files\":{}}").unwrap();
    }

    #[test]
    fn fresh_vault_inits_and_makes_exactly_one_commit_with_penpot_dirs() {
        if !git_available() {
            eprintln!("git unavailable; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        seed_vault(&vault);

        assert_eq!(probe(&vault), RepoState::NoRepo);
        let out = checkpoint(&vault, "Checkpoint one").unwrap();
        assert_eq!(out.decision, Decision::Init);
        assert_eq!(commit_count(&vault), 1, "fresh vault → exactly one commit");
        assert!(out.commit.is_some());
        // The single commit contains the manifest + the .penpot dir.
        let tracked = Command::new("git")
            .args(["-C", &vault.to_string_lossy(), "ls-files"])
            .output()
            .unwrap();
        let files = String::from_utf8_lossy(&tracked.stdout);
        assert!(files.contains("Client/home.penpot/manifest.json"), "penpot dir committed: {files}");
        assert!(files.contains(".penpot-sync.json"), "manifest committed: {files}");
    }

    #[test]
    fn pre_existing_repo_adds_one_commit_rewriting_no_history() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        seed_vault(&vault);
        run(&vault, &["init", "-q"]);
        run(&vault, &["add", "-A"]);
        run(&vault, &["commit", "-q", "-m", "user's own initial commit"]);
        let base_count = commit_count(&vault);
        let base_head = head_short(&vault).unwrap();

        // A new edit appears (a synced board changed).
        std::fs::write(vault.join("Client/home.penpot/files/data.json"), "{\"x\":1}").unwrap();
        assert_eq!(probe(&vault), RepoState::Clean { has_changes: true });

        let out = checkpoint(&vault, "Checkpoint two").unwrap();
        assert_eq!(out.decision, Decision::Commit);
        assert_eq!(commit_count(&vault), base_count + 1, "exactly one new commit");
        // History not rewritten: the previous HEAD is still the parent.
        let parent = Command::new("git")
            .args(["-C", &vault.to_string_lossy(), "rev-parse", "--short", "HEAD~1"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&parent.stdout).trim(), base_head);
    }

    #[test]
    fn no_change_since_last_checkpoint_is_a_clean_noop() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        seed_vault(&vault);
        checkpoint(&vault, "Checkpoint one").unwrap();
        let count = commit_count(&vault);

        assert_eq!(probe(&vault), RepoState::Clean { has_changes: false });
        let out = checkpoint(&vault, "Checkpoint two").unwrap();
        assert_eq!(out.decision, Decision::NoOp);
        assert_eq!(commit_count(&vault), count, "no-op adds no commit");
    }

    #[test]
    fn dirty_repo_mid_merge_is_refused_loudly() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        // Build a conflicting merge.
        run(&vault, &["init", "-q"]);
        std::fs::write(vault.join("f.txt"), "base\n").unwrap();
        run(&vault, &["add", "-A"]);
        run(&vault, &["commit", "-q", "-m", "base"]);
        // Capture the default branch name (main/master vary by git config).
        let default_branch = {
            let out = Command::new("git")
                .args(["-C", &vault.to_string_lossy(), "rev-parse", "--abbrev-ref", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&vault, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(vault.join("f.txt"), "feature\n").unwrap();
        run(&vault, &["commit", "-qam", "feature"]);
        run(&vault, &["checkout", "-q", &default_branch]);
        std::fs::write(vault.join("f.txt"), "mainline\n").unwrap();
        run(&vault, &["commit", "-qam", "mainline"]);
        // Attempt the conflicting merge (expected to fail, leaving MERGE state).
        let _ = Command::new("git")
            .args(["-C", &vault.to_string_lossy(), "-c", "user.email=t@t", "-c", "user.name=T", "merge", "feature"])
            .output()
            .unwrap();

        let state = probe(&vault);
        assert!(
            matches!(state, RepoState::InProgress(_)),
            "mid-merge must probe as InProgress, got {state:?}"
        );
        let before = commit_count(&vault);
        let out = checkpoint(&vault, "Checkpoint x").unwrap();
        assert!(matches!(out.decision, Decision::Refuse(_)), "must refuse: {out:?}");
        assert!(out.message.to_lowercase().contains("refused"));
        assert_eq!(commit_count(&vault), before, "refusal touches nothing");
    }

    #[test]
    fn detached_head_is_refused() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        seed_vault(&vault);
        run(&vault, &["init", "-q"]);
        run(&vault, &["add", "-A"]);
        run(&vault, &["commit", "-q", "-m", "c1"]);
        std::fs::write(vault.join("more.json"), "{}").unwrap();
        run(&vault, &["add", "-A"]);
        run(&vault, &["commit", "-q", "-m", "c2"]);
        // Detach onto the first commit.
        run(&vault, &["checkout", "-q", "HEAD~1"]);
        let state = probe(&vault);
        assert_eq!(state, RepoState::InProgress("HEAD is detached".into()));
        assert!(matches!(decide(&state), Decision::Refuse(_)));
    }
}
