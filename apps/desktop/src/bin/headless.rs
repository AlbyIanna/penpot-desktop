//! Headless runner: the same boot sequence as the Tauri app, no window.
//!
//! Prints `READY <proxy-url>` on stdout once the stack is up, then runs until
//! SIGTERM/SIGINT and shuts the supervised children down cleanly. Used by
//! `scripts/m1-smoke.sh` (`just smoke`), `scripts/m2-invariant.sh`
//! (`just invariant`), `scripts/m3-sync.sh` (`just m3`) and the N-gates.
//!
//! **N5 vaults**: boot goes through [`control::boot_active_vault`], which
//! resolves the active vault (registry + interrupted-switch recovery) before
//! booting. A localhost control server always comes up so `scripts/n5-vaults.sh`
//! can drive `File > Open Vault` switches headlessly (`POST /open {path}`,
//! `GET /active`, `GET /list`) and, from chapter C, so the CLI can attach;
//! the GUI uses the same [`control::VaultRunner`] behind a native dialog.
//! `PENPOT_LOCAL_CONTROL_PORT` only pins the port. Every request must carry
//! the per-boot token from `<data_dir>/runtime.json` — see
//! [`app_core::runtime_record`].
//!
//! **Test-only control hook**: SIGUSR1 toggles the sync daemon's
//! pause/resume (the same `SyncControl` surface the tray uses). This exists
//! so shell-driven tests (the simultaneous-edit conflict test in
//! `scripts/m3-sync.sh`) can pause the daemon from outside the process; the
//! GUI app does not install this handler — the tray menu is its control
//! surface. Each toggle logs `SIGUSR1: sync paused/resumed` on stderr.

use std::io::Write;

use app_core::{control, AppConfig};

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

    // N5: resolve the active vault (registry + interrupted-switch recovery),
    // then boot it. The runner owns the live stack across vault switches.
    let runner = control::boot_active_vault(config).await?;

    // N5: the localhost control server, driving `File > Open Vault` switches
    // without the GUI dialog. D5 Task 2 adds `GET /windows`; the headless
    // runner has no Tauri, so no window ever opens here — an empty,
    // never-populated `WindowRegistry` is honest: `/windows` answers
    // `{"windows":[]}`, which is the truth for this binary (the GUI shell
    // wires its REAL registry through the same call).
    //
    // Chapter C: unconditional, because the CLI attaches through it. The env
    // var only pins the port for the gates; unset ⇒ 0 ⇒ the OS picks one and
    // `serve_control` publishes the BOUND port (with the pid, the proxy port
    // and this token) into `<data_dir>/runtime.json`, mode 0600.
    let control_port = control::control_port_from_env().unwrap_or(0);
    let control_token = app_core::runtime_record::new_token();
    {
        let runner_ctl = runner.clone();
        let windows = app_core::windows::WindowRegistry::new();
        let data_dir = runner.data_dir().to_path_buf();
        let proxy_port = runner.proxy_port();
        tokio::spawn(async move {
            if let Err(e) = control::serve_control(
                runner_ctl,
                windows,
                control_port,
                control_token,
                &data_dir,
                proxy_port,
            )
            .await
            {
                tracing::error!("vault control server exited: {e:#}");
            }
        });
    }

    // stdout is block-buffered when piped — flush so waiters see READY now.
    println!("READY {}", runner.proxy_url());
    std::io::stdout().flush().ok();

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigusr1 = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
    loop {
        tokio::select! {
            _ = sigterm.recv() => { eprintln!("SIGTERM received, shutting down"); break },
            r = tokio::signal::ctrl_c() => { r?; eprintln!("SIGINT received, shutting down"); break },
            _ = sigusr1.recv() => {
                // Test-only hook (see module docs): toggle sync pause on the
                // CURRENT vault's daemon.
                match runner.sync_control().await {
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

    // Chapter C: retract the discovery record BEFORE the stack goes down, so a
    // CLI never attaches to a control port that is already on its way out.
    // Idempotent and never fatal (see `remove_record`).
    app_core::runtime_record::remove_record(runner.data_dir());
    runner.shutdown().await;
    println!("STOPPED");
    std::io::stdout().flush().ok();
    Ok(())
}
