//! `penpot daemon` — the headless runner: the same boot sequence as the Tauri
//! app, no window.
//!
//! This is the body that was `apps/desktop/src/bin/headless.rs` until chapter
//! C task 11, lifted here unchanged so the CLI owns the one process that can be
//! "the app" without a GUI. `bin/headless` is now a three-line shim over
//! [`run`], because **every milestone gate in this repo launches
//! `--bin headless` and waits for `READY <url>` on stdout**: `scripts/m1-smoke.sh`
//! asserts `READY`, clean SIGTERM shutdown with no orphan `java`/`valkey`/
//! `postgres`, and a `STOPPED` line; `scripts/m3-sync.sh` drives the SIGUSR1
//! sync-pause toggle; `m2-invariant`, `m4`, `m5` and the D/E/N gates all start
//! this way. Nothing about that contract may drift — not the flush after
//! `READY`, not the SIGUSR1 handler, not the `remove_record` on the way down.
//!
//! Prints `READY <proxy-url>` on stdout once the stack is up, then runs until
//! SIGTERM/SIGINT and shuts the supervised children down cleanly.
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
//!
//! Note this is the ONE verb that is not a client of a running app — it *is*
//! the running app. Everything in [`crate::verbs::loopback`] attaches to a
//! process started here.

use std::io::Write;

use app_core::{control, AppConfig};

/// Synchronous entry point: build the runtime, then run [`serve`] on it.
///
/// This is what `apps/desktop/src/bin/headless.rs` calls, and it is exactly
/// what the `#[tokio::main]` attribute that used to sit on that binary's `main`
/// expanded to — `Runtime::new()` *is* `Builder::new_multi_thread().enable_all()`.
/// The gates therefore get the same runtime shape they have always had.
///
/// Must not be called from inside an existing runtime (`block_on` would panic).
/// The `penpot daemon` subcommand, whose `main` is already async, calls
/// [`serve`] directly instead.
pub fn run() -> anyhow::Result<()> {
    tokio::runtime::Runtime::new()?.block_on(serve())
}

/// The daemon body. Lifted verbatim from `bin/headless.rs`'s `main`.
pub async fn serve() -> anyhow::Result<()> {
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
    // and this token) into `runtime.json` under the runner's OWN data dir —
    // the same dir the shutdown path below retracts it from.
    let control_port = control::control_port_from_env().unwrap_or(0);
    let control_token = app_core::runtime_record::new_token();
    {
        let runner_ctl = runner.clone();
        let windows = app_core::windows::WindowRegistry::new();
        tokio::spawn(async move {
            if let Err(e) =
                control::serve_control(runner_ctl, windows, control_port, control_token).await
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
