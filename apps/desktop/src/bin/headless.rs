//! Headless runner: the same boot sequence as the Tauri app, no window.
//!
//! Prints `READY <proxy-url>` on stdout once the stack is up, then runs until
//! SIGTERM/SIGINT and shuts the supervised children down cleanly. Used by
//! `scripts/m1-smoke.sh` (`just smoke`), `scripts/m2-invariant.sh`
//! (`just invariant`) and `scripts/m3-sync.sh` (`just m3`).
//!
//! **Test-only control hook**: SIGUSR1 toggles the sync daemon's
//! pause/resume (the same `SyncControl` surface the tray uses). This exists
//! so shell-driven tests (the simultaneous-edit conflict test in
//! `scripts/m3-sync.sh`) can pause the daemon from outside the process; the
//! GUI app does not install this handler — the tray menu is its control
//! surface. Each toggle logs `SIGUSR1: sync paused/resumed` on stderr.

use std::io::Write;

use penpot_desktop::{boot, AppConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let config = AppConfig::resolve()?;
    eprintln!(
        "booting Penpot Local (data dir: {}, runtime: {})",
        config.data_dir.display(),
        config.runtime_dir.display()
    );

    let app = boot(config).await?;

    // stdout is block-buffered when piped — flush so waiters see READY now.
    println!("READY {}", app.proxy_url);
    std::io::stdout().flush().ok();

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigusr1 = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
    loop {
        tokio::select! {
            _ = sigterm.recv() => { eprintln!("SIGTERM received, shutting down"); break },
            r = tokio::signal::ctrl_c() => { r?; eprintln!("SIGINT received, shutting down"); break },
            _ = sigusr1.recv() => {
                // Test-only hook (see module docs): toggle sync pause.
                match app.sync_control() {
                    Some(control) => {
                        if control.is_paused() {
                            control.resume();
                            eprintln!("SIGUSR1: sync resumed");
                        } else {
                            control.pause();
                            eprintln!("SIGUSR1: sync paused");
                        }
                    }
                    None => eprintln!("SIGUSR1 ignored: sync daemon not running"),
                }
            }
        }
    }

    app.shutdown().await;
    println!("STOPPED");
    std::io::stdout().flush().ok();
    Ok(())
}
