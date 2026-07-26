mod attach;
mod verbs;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "penpot", about = "Penpot Local (internal CLI)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the known vaults and mark the active one (Tier 1).
    VaultList,
    /// Commit the active vault's current state (Tier 1).
    Checkpoint,
    /// Resolve what a path would open, without opening it (Tier 1).
    Open { path: String },
    /// List the built-in template catalog (Tier 1).
    TplList,
    /// Print the preferences file (Tier 1).
    PrefsGet,
    /// List installed packages from lock.json (Tier 1, partial).
    PkgList,
    /// Print per-file sync state from the manifest (Tier 1, partial).
    SyncStatus,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    let cli = Cli::parse();
    let config = app_core::AppConfig::resolve()?;
    let out = match cli.command {
        Command::VaultList => verbs::tier1::vault_list(&config)?,
        Command::Checkpoint => verbs::tier1::checkpoint(&config)?,
        Command::Open { path } => verbs::tier1::open(&config, &path)?,
        Command::TplList => verbs::tier1::tpl_list(&config)?,
        Command::PrefsGet => verbs::tier1::prefs_get(&config)?,
        Command::PkgList => verbs::tier1::pkg_list(&config)?,
        Command::SyncStatus => verbs::tier1::sync_status(&config)?,
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
