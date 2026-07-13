//! Headless runner: the same boot sequence as the Tauri app, no window.
//!
//! Prints `READY <proxy-url>` on stdout once the stack is up, then runs until
//! SIGTERM/SIGINT and shuts the supervised children down cleanly. Used by
//! `scripts/m1-smoke.sh` (`just smoke`).

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
    tokio::select! {
        _ = sigterm.recv() => eprintln!("SIGTERM received, shutting down"),
        r = tokio::signal::ctrl_c() => { r?; eprintln!("SIGINT received, shutting down") },
    }

    app.shutdown().await;
    println!("STOPPED");
    std::io::stdout().flush().ok();
    Ok(())
}
