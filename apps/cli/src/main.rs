//! The `penpot` binary. Thin: it parses arguments and calls into
//! [`penpot_cli`], which is where every verb actually lives.
//!
//! It reaches the verbs through the LIBRARY rather than re-declaring
//! `mod attach; mod verbs;`, so these sources are compiled once. Task 10's
//! duplicate module tree compiled them twice and made "unused in the binary"
//! a warning about code the library used perfectly well.

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::Value;

use penpot_cli::verbs;

#[derive(Parser)]
#[command(name = "penpot", about = "Penpot Local (internal CLI)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the local stack headlessly until SIGTERM/SIGINT.
    Daemon,

    // -- Tier 1: no app, no backend --------------------------------------
    /// List the known vaults and mark the active one (Tier 1).
    VaultList,
    /// Commit the active vault's current state (Tier 1).
    Checkpoint,
    /// Resolve what a path would open, without opening it (Tier 1).
    Open { path: String },
    /// List the built-in template catalog (Tier 1).
    TplList,
    /// List installed packages from lock.json (Tier 1, partial).
    PkgList,

    // -- Tier 1 by default, Tier 2 with `--live` --------------------------
    /// Print the preferences file; `--live` asks the running app instead,
    /// which adds the live facts the file cannot carry (Tier 1 / Tier 2).
    PrefsGet {
        #[arg(long)]
        live: bool,
    },
    /// Print per-file sync state from the manifest; `--live` asks the running
    /// app for `syncPaused`/`lastOp`, which exist only in the daemon
    /// (Tier 1 / Tier 2).
    SyncStatus {
        #[arg(long)]
        live: bool,
    },

    // -- Tier 2: needs the running app ------------------------------------
    /// Pause the sync daemon (Tier 2). Persisted, and NOT an idle barrier —
    /// see `verbs::loopback`'s module doc.
    SyncPause,
    /// Resume the sync daemon (Tier 2).
    SyncResume,
    /// Set one preference field, preserving the rest (Tier 2).
    PrefsSet {
        /// Field name, camelCase as stored (e.g. `pluginsEnabled`).
        field: String,
        /// Value, parsed as JSON when it parses (`true`, `false`, a number),
        /// otherwise taken as a string.
        value: String,
    },
    /// `git clone` a package repo into the vault (Tier 2). Prints the DERIVED
    /// id, which is what `pkg-install` takes.
    PkgAdd {
        url: String,
        #[arg(long)]
        id: Option<String>,
    },
    /// List the vault's boards (Tier 2).
    Boards {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        sort: Option<String>,
    },
    /// Search the vault index (Tier 2).
    Search {
        q: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Reveal a vault-relative path in the file manager (Tier 2).
    Reveal { path: String },

    // -- Tier 3: needs the supervised backend too --------------------------
    /// Install a fetched package (Tier 3).
    PkgInstall {
        id: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Create a file from a built-in template (Tier 3).
    New {
        #[arg(long = "from-template")]
        from_template: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Switch the active vault (Tier 3). Tears the stack down and boots the
    /// target; the only correct way for a second process to switch.
    VaultSwitch { path: String },
    /// Reboot the supervised stack in place (Tier 3). Ack-then-detach: the
    /// outcome shows up in `prefs-get --live`'s `lastOp`.
    Reboot,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        // `penpot daemon` is not a client of a running app — it IS one. It
        // installs its own `EnvFilter` subscriber (the one every gate reads on
        // stderr) and owns the runtime for the whole process life, so it is
        // dispatched here, before either is set up, through exactly the call
        // `apps/desktop/src/bin/headless.rs` makes.
        Command::Daemon => verbs::daemon::run(),
        command => {
            tracing_subscriber::fmt().with_writer(std::io::stderr).init();
            let out = tokio::runtime::Runtime::new()?.block_on(dispatch(command))?;
            println!("{}", serde_json::to_string_pretty(&out)?);
            Ok(())
        }
    }
}

/// Parse a preference value the way a shell hands it over: JSON when it is
/// JSON (`true`, `false`, `3`), a plain string otherwise — so `prefs-set
/// syncEnabled false` sets a boolean rather than the string `"false"`, which
/// the route would reject.
fn preference_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

async fn dispatch(command: Command) -> Result<Value> {
    let config = app_core::AppConfig::resolve()?;
    let dir = config.data_dir.clone();
    match command {
        Command::Daemon => unreachable!("`daemon` is dispatched before the runtime is built"),

        Command::VaultList => verbs::tier1::vault_list(&config),
        Command::Checkpoint => verbs::tier1::checkpoint(&config),
        Command::Open { path } => verbs::tier1::open(&config, &path),
        Command::TplList => verbs::tier1::tpl_list(&config),
        Command::PkgList => verbs::tier1::pkg_list(&config),

        Command::PrefsGet { live } => match live {
            true => verbs::loopback::prefs_get(&dir).await,
            false => verbs::tier1::prefs_get(&config),
        },
        Command::SyncStatus { live } => match live {
            true => verbs::loopback::sync_status(&dir).await,
            false => verbs::tier1::sync_status(&config),
        },

        Command::SyncPause => verbs::loopback::sync_set_paused(&dir, true).await,
        Command::SyncResume => verbs::loopback::sync_set_paused(&dir, false).await,
        Command::PrefsSet { field, value } => {
            verbs::loopback::prefs_set(&dir, &field, preference_value(&value)).await
        }
        Command::PkgAdd { url, id } => verbs::loopback::pkg_add(&dir, &url, id.as_deref()).await,
        Command::Boards { project, sort } => {
            verbs::loopback::boards(&dir, project.as_deref(), sort.as_deref()).await
        }
        Command::Search { q, kind, limit } => {
            verbs::loopback::search(&dir, &q, kind.as_deref(), limit).await
        }
        Command::Reveal { path } => verbs::loopback::reveal(&dir, &path).await,

        Command::PkgInstall { id, name } => {
            verbs::loopback::pkg_install(&dir, &id, name.as_deref()).await
        }
        Command::New { from_template, name } => {
            verbs::loopback::new_from_template(&dir, &from_template, name.as_deref()).await
        }
        Command::VaultSwitch { path } => verbs::loopback::vault_switch(&dir, &path).await,
        Command::Reboot => verbs::loopback::reboot(&dir).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `prefs-set syncEnabled false` must send a JSON `false`, not the string
    /// `"false"` — `Preferences` deserializes strictly, so the string form is
    /// a 422 from the route rather than a saved preference.
    #[test]
    fn a_preference_value_parses_as_json_when_it_can() {
        assert_eq!(preference_value("false"), Value::Bool(false));
        assert_eq!(preference_value("true"), Value::Bool(true));
        assert_eq!(preference_value("7"), serde_json::json!(7));
        assert_eq!(preference_value("dark"), Value::String("dark".into()));
    }

    /// Clap's own consistency check over the whole command tree: duplicate
    /// subcommand or flag names, a positional after an optional one, and
    /// several other build-time mistakes only surface here.
    #[test]
    fn the_command_tree_is_well_formed() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
