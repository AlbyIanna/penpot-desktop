//! D3: the dumb translation of [`model::MenuModel`] into `tauri::menu::*`,
//! plus the app-global menu-event dispatcher. Mirrors `tray/mod.rs`'s job
//! for the tray exactly — `model.rs` owns every branch (which items exist,
//! which are enabled, which command they carry); this module only reads it
//! and turns clicks into calls on the REAL existing implementations: the
//! same `create-file`/`create-project`/`export-binfile` RPCs `manage.rs`
//! calls, the same `installer::import_binfile_and_settle` templates/packages
//! use, the sync manifest + `reveal.rs` for on-disk lookups, and the N5
//! vault-switch callback the tray already uses. It adds no business logic of
//! its own — and, per this task's own core invariant, it never writes to the
//! vault directly: creation/import go through the backend so only the sync
//! daemon (on its own schedule) ever touches the folder tree.
//!
//! **Why `open_file_window`/`navigation_policy` live here and not in
//! `main.rs`** (a deliberate deviation from where the D3 plan originally
//! sketched them): `menubar` is a module of the `penpot_desktop` LIBRARY
//! crate (`pub mod menubar;` in `lib.rs`), while `main.rs` is the separate
//! BINARY crate that only *depends on* the library. This module is the
//! first real caller of window-per-file (via File > Open… / Open Recent),
//! and a library module cannot call a function defined in the binary — so
//! the window-construction code has to live on the library side of that
//! boundary, beside its only caller. `main.rs` still builds the *home*
//! window itself (unchanged) and calls [`navigation_policy`] for it, so the
//! redirect rule is defined exactly once regardless of which file opened
//! the window.
//!
//! **macOS reality that shapes this file:** the menu bar is app-wide
//! (`Window::set_menu` is explicitly unsupported on macOS), so it is
//! installed with `AppHandle::set_menu` and rebuilt on every window-set or
//! key-window change — exactly like the tray rebuilds on every status
//! change. Menu events are app-global too (there is no per-window menu on
//! macOS), so `Builder`/`App`'s `on_menu_event` is registered exactly ONCE,
//! inside [`install`]; [`rebuild`] only recomputes the model and re-sets the
//! menu, it never re-registers a listener (that would fire every prior
//! listener too — Tauri's API is push-only, there is no "replace").

pub mod model;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use penpot_rpc::{Auth, PenpotClient};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder, WindowEvent};

use crate::navwatch::{self, Decision, NavWatch};
use crate::recent::{self, RecentEntry};
use crate::windows::{self, OpenWindow, Reuse, WindowRegistry, HOME_LABEL};
use model::{build_menu_model, command_for_id, Command, Entry, MenuModel, Predefined, RECENT_PREFIX};

/// Late-bound facts about the active vault/stack: unknown before boot
/// completes, and refreshed again on every N5 vault switch (`File > Open
/// Vault…`). Bundled behind one lock — rather than three separate slots —
/// because `main.rs` always updates all three together, at exactly the two
/// moments any of them changes; mirrors `overlay::ProxyUrlSlot`'s "late-bound
/// shared value" shape, just with three fields instead of one.
#[derive(Debug, Clone, Default)]
pub struct LiveVault {
    /// The proxy origin, e.g. `http://localhost:8686`. Empty before boot.
    /// RPC calls use THIS (not the backend's direct URL) because the menu
    /// bar is a CLIENT of the local stack, same as the SPA itself — the
    /// proxy forwards `/api/*` to the backend, so this is same-origin RPC,
    /// not a bypass of it.
    pub proxy_url: String,
    /// The active vault's default team id. Empty before boot.
    pub team_id: String,
    /// RPC access token for the single provisioned user. Empty before boot.
    /// Needed ONLY by the commands that must go through the backend rather
    /// than the vault directly — see `access_token`'s doc on `VaultRunner`.
    pub access_token: String,
    /// The active vault's root directory on disk. Used strictly for READS
    /// (resolving a picked folder or the key window's file to its manifest
    /// entry) — this task's core invariant is that nothing here WRITES to
    /// the vault; only the sync daemon does, on its own schedule. Empty
    /// (`PathBuf::new()`) before boot.
    pub vault_root: PathBuf,
}

pub type LiveVaultSlot = Arc<Mutex<LiveVault>>;

fn live_snapshot(slot: &LiveVaultSlot) -> LiveVault {
    slot.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Everything the translation + dispatch layer needs.
///
/// `registry` (already `Arc`-backed) and `live` (`Arc<Mutex<_>>`) make this
/// type CHEAP AND ALWAYS LIVE to clone: every clone shares the same
/// underlying window registry and the same late-bound vault facts, so a
/// clone captured by a closure at any point in time keeps seeing fresh data
/// forever — there is no "stale snapshot" to worry about. `data_dir` and
/// `on_open_vault` never change after construction, so they are plain owned
/// values.
#[derive(Clone)]
pub struct MenuCtx {
    pub registry: WindowRegistry,
    pub data_dir: PathBuf,
    pub live: LiveVaultSlot,
    /// The SAME callback the tray's "Open Vault…" entry uses (see
    /// `tray::OpenVaultCb`) — `None` in demo mode. Sharing this one
    /// implementation, rather than re-deriving the N5 switch flow here, is
    /// what "do not reimplement" means for `Command::OpenVault`.
    pub on_open_vault: Option<crate::tray::OpenVaultCb>,
}

// ---------------------------------------------------------------------------
// Model → tauri::menu::* translation (no branching of its own)
// ---------------------------------------------------------------------------

/// Map a `model::Predefined` kind to its `PredefinedMenuItem` constructor.
/// This match is EXHAUSTIVE (no wildcard arm) — a variant added to
/// `model::Predefined` without a matching arm here fails the BUILD. This
/// used to be a string lookup with a `panic!` fallback arm for an
/// unrecognized name (D3-review MINOR finding 6b): that function ran inside
/// `rebuild`, which callback runs from window-event handlers, so a drift
/// between the two files would have panicked a UI callback instead of
/// failing to compile. The enum in `model.rs` closes that hole entirely —
/// there is no runtime "unknown name" case left.
fn predefined_item<R: Runtime>(app: &AppHandle<R>, kind: Predefined) -> tauri::Result<PredefinedMenuItem<R>> {
    let app_name = model::APP_NAME;
    match kind {
        Predefined::Undo => PredefinedMenuItem::undo(app, None),
        Predefined::Redo => PredefinedMenuItem::redo(app, None),
        Predefined::Cut => PredefinedMenuItem::cut(app, None),
        Predefined::Copy => PredefinedMenuItem::copy(app, None),
        Predefined::Paste => PredefinedMenuItem::paste(app, None),
        Predefined::SelectAll => PredefinedMenuItem::select_all(app, None),
        Predefined::Minimize => PredefinedMenuItem::minimize(app, None),
        // macOS's "Zoom" menu item IS `PredefinedMenuItem::maximize` — there
        // is no separately named "zoom" constructor in tauri::menu.
        Predefined::Zoom => PredefinedMenuItem::maximize(app, None),
        Predefined::CloseWindow => PredefinedMenuItem::close_window(app, None),
        // The application-submenu items (D3-review CRITICAL finding 1).
        //
        // These pass an EXPLICIT product name rather than `None`. With `None`
        // the OS derives the wording from the running executable, which is
        // right in a packaged .app but reads "About penpot-desktop" / "Quit
        // penpot-desktop" in a dev build — verified live via System Events.
        // Naming it here is correct in both, and keeps these labels agreeing
        // with the menu bar title, which already says "Penpot Local".
        Predefined::About => {
            PredefinedMenuItem::about(app, Some(&format!("About {app_name}")), None)
        }
        Predefined::Services => PredefinedMenuItem::services(app, None),
        Predefined::Hide => PredefinedMenuItem::hide(app, Some(&format!("Hide {app_name}"))),
        Predefined::HideOthers => PredefinedMenuItem::hide_others(app, None),
        Predefined::ShowAll => PredefinedMenuItem::show_all(app, None),
        Predefined::Quit => PredefinedMenuItem::quit(app, Some(&format!("Quit {app_name}"))),
    }
}

fn menu_item<R: Runtime>(app: &AppHandle<R>, item: &model::Item) -> tauri::Result<MenuItem<R>> {
    MenuItem::with_id(app, item.id.clone(), &item.label, item.enabled, item.accelerator.as_deref())
}

/// Append `entries` onto `parent`, grouping the contiguous run of Open
/// Recent rows (id prefix [`RECENT_PREFIX`]) into a real `Submenu` titled
/// "Open Recent". The pure model has no submenu concept (see its module
/// doc) — recognizing that one, fixed, model-owned id convention and nesting
/// it visually is exactly the kind of "dumb translation" this file exists
/// for: it does not decide what the recent list contains, only how it nests.
fn append_entries<R: Runtime>(app: &AppHandle<R>, parent: &Submenu<R>, entries: &[Entry]) -> tauri::Result<()> {
    let mut i = 0;
    while i < entries.len() {
        match &entries[i] {
            Entry::Item(first) if first.id.starts_with(RECENT_PREFIX) => {
                let recent_menu = Submenu::new(app, "Open Recent", true)?;
                while let Some(Entry::Item(item)) = entries.get(i) {
                    if !item.id.starts_with(RECENT_PREFIX) {
                        break;
                    }
                    recent_menu.append(&menu_item(app, item)?)?;
                    i += 1;
                }
                parent.append(&recent_menu)?;
            }
            Entry::Item(item) => {
                parent.append(&menu_item(app, item)?)?;
                i += 1;
            }
            Entry::Separator => {
                parent.append(&PredefinedMenuItem::separator(app)?)?;
                i += 1;
            }
            Entry::Predefined(kind) => {
                parent.append(&predefined_item(app, *kind)?)?;
                i += 1;
            }
        }
    }
    Ok(())
}

fn build_tauri_menu<R: Runtime>(app: &AppHandle<R>, model: &MenuModel) -> tauri::Result<Menu<R>> {
    let menu = Menu::new(app)?;
    for section in &model.sections {
        let submenu = Submenu::new(app, &section.title, true)?;
        append_entries(app, &submenu, &section.entries)?;
        menu.append(&submenu)?;
    }
    Ok(menu)
}

/// Install the app-wide native menu bar and register the ONE global
/// menu-event listener (macOS has no per-window menu, so per-window
/// listeners do not exist — see the module doc). Call this exactly once,
/// early in setup; every later window-set/key change goes through
/// [`rebuild`] instead.
pub fn install<R: Runtime>(app: &AppHandle<R>, ctx: &MenuCtx) -> tauri::Result<()> {
    rebuild(app, ctx)?;
    let ctx = ctx.clone();
    app.on_menu_event(move |app, event| {
        dispatch(app, &ctx, event.id.as_ref());
    });
    Ok(())
}

/// Recompute the model from the current registry/recent-files/key-window
/// state and re-set the app-wide menu. Cheap (a handful of items), so call
/// this liberally: on every window open/close, every key-window change, and
/// once more after boot completes / after an N5 vault switch.
pub fn rebuild<R: Runtime>(app: &AppHandle<R>, ctx: &MenuCtx) -> tauri::Result<()> {
    let key = ctx.registry.key();
    let key_label = key.as_ref().map(|w| w.label.as_str());
    let open_windows = ctx.registry.list();
    let recents = recent::list_recent(&ctx.data_dir, recent::RECENT_LIMIT);
    let model = build_menu_model(&open_windows, &recents, key_label);
    let menu = build_tauri_menu(app, &model)?;
    app.set_menu(menu)?;
    Ok(())
}

/// The D3 hook point: call whenever the set of open windows changes (a file
/// window opened or closed) or the key window changes. Logs rather than
/// propagates a rebuild failure — a stale menu is recoverable (the next
/// window event rebuilds it again), crashing the window-close path is not.
pub fn on_window_set_changed<R: Runtime>(app: &AppHandle<R>, ctx: &MenuCtx) {
    if let Err(e) = rebuild(app, ctx) {
        tracing::warn!("menu rebuild failed: {e}");
    }
}

// ---------------------------------------------------------------------------
// Window-per-file (moved here from main.rs — see the module doc for why)
// ---------------------------------------------------------------------------

/// Build the shared `on_navigation` policy closure for the window labelled
/// `label`: D1/D2's rule that `#/auth/*` and `#/dashboard` are cancelled,
/// redirecting to `recovery_path` on THIS window. Used for the home window
/// (by `main.rs`, `recovery_path = navwatch::HOME_PATH`) AND every file
/// window (by [`open_file_window`] below, `recovery_path` = that file's own
/// `workspace_deep_link`) so the redirect rule is defined exactly once — a
/// second, hand-copied closure is exactly how `#/dashboard` would quietly
/// become reachable again from a file window.
///
/// **D3-review IMPORTANT fix (finding 2):** `navwatch::decide` always
/// returns `/__home` as the path inside `Decision::CancelAndRedirect` — that
/// constant is right for the home window, but sending a FILE window there
/// left the window showing Home while the registry still recorded its
/// `file_id`, a "ghost" the Window menu, Export/Reveal, and `reuse_or_create`
/// all then acted on incorrectly. The single cancel-or-allow decision in
/// `navwatch::decide` is UNCHANGED (still one policy body, still
/// Tauri-free/unit-tested there); only the recovery DESTINATION is now a
/// parameter of this one translation function, per-window, rather than a
/// second copy of the redirect rule.
pub fn navigation_policy<R: Runtime>(
    app: &AppHandle<R>,
    label: &str,
    watch: NavWatch,
    recovery_path: &str,
) -> impl Fn(&tauri::Url) -> bool + Send + 'static {
    let nav_handle = app.clone();
    let label = label.to_string();
    let recovery_path = recovery_path.to_string();
    move |url| {
        let url_s = url.to_string();
        watch.record("on_navigation", &url_s);
        match navwatch::decide(&url_s, watch.redirect_enabled()) {
            Decision::Allow => true,
            Decision::CancelAndRedirect(_) => {
                let h = nav_handle.clone();
                let label = label.clone();
                let recovery_path = recovery_path.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(w) = h.get_webview_window(&label) {
                        if let Ok(base) = w.url() {
                            if let Ok(dest) = base.join(&recovery_path) {
                                let _ = w.navigate(dest);
                            }
                        }
                    }
                });
                false
            }
        }
    }
}

/// Open `file_id` in its own window (D3: window-per-file), focusing the
/// existing window instead of duplicating it if the file is already open
/// (`windows::reuse_or_create`, unit-tested without a Tauri runtime). A
/// no-op (logged) if called before boot has published `proxy_url`/`team_id`
/// — there is nothing to build a deep link to yet.
pub fn open_file_window<R: Runtime>(
    app: &AppHandle<R>,
    ctx: &MenuCtx,
    file_id: &str,
    page_id: Option<&str>,
    title: &str,
) -> tauri::Result<()> {
    match windows::reuse_or_create(file_id, &ctx.registry) {
        Reuse::Focus(label) => {
            if let Some(w) = app.get_webview_window(&label) {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
            Ok(())
        }
        Reuse::Create(label) => {
            let live = live_snapshot(&ctx.live);
            if live.proxy_url.is_empty() || live.team_id.is_empty() {
                tracing::warn!("open_file_window called before boot completed; ignoring");
                return Ok(());
            }
            let path = vault_index::workspace_deep_link(&live.team_id, file_id, page_id);
            let full = format!("{}{}", live.proxy_url.trim_end_matches('/'), path);
            let url: tauri::Url = full.parse().map_err(tauri::Error::InvalidUrl)?;

            let watch = NavWatch::from_env();
            // Recovery path is THIS window's own deep link (`path`, computed
            // above), not the home window's `/__home` — see
            // `navigation_policy`'s doc comment (D3-review finding 2). Using
            // the shared `/__home` here would be the ghost-window bug: this
            // window would show Home while the registry still says it shows
            // `file_id`.
            let window = WebviewWindowBuilder::new(app, &label, WebviewUrl::External(url))
                .title(title)
                .inner_size(1280.0, 800.0)
                .resizable(true)
                .on_navigation(navigation_policy(app, &label, watch, &path))
                .build()?;

            ctx.registry.insert(OpenWindow {
                label: label.clone(),
                file_id: Some(file_id.to_string()),
                title: title.to_string(),
            });
            // A freshly created window is the frontmost one.
            ctx.registry.set_key(&label);

            let ctx_for_events = ctx.clone();
            let app_for_events = app.clone();
            let label_for_events = label.clone();
            window.on_window_event(move |event| match event {
                WindowEvent::Destroyed => {
                    ctx_for_events.registry.remove(&label_for_events);
                    on_window_set_changed(&app_for_events, &ctx_for_events);
                }
                WindowEvent::Focused(true) => {
                    ctx_for_events.registry.set_key(&label_for_events);
                    on_window_set_changed(&app_for_events, &ctx_for_events);
                }
                _ => {}
            });

            on_window_set_changed(app, ctx);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch: id -> Command -> the real implementation
// ---------------------------------------------------------------------------

fn dispatch<R: Runtime>(app: &AppHandle<R>, ctx: &MenuCtx, id: &str) {
    let key = ctx.registry.key();
    let key_label = key.as_ref().map(|w| w.label.as_str());
    let open_windows = ctx.registry.list();
    let recents = recent::list_recent(&ctx.data_dir, recent::RECENT_LIMIT);
    let model = build_menu_model(&open_windows, &recents, key_label);
    let Some(command) = command_for_id(&model, id) else {
        // Not a bug by itself (predefined items fire without going through
        // `command_for_id` at all — they never reach `dispatch`), but any id
        // that DOES reach here and doesn't resolve is worth knowing about.
        tracing::debug!(id, "menubar: menu event with no resolvable command");
        return;
    };
    run_command(app, ctx, command);
}

/// `<basename>.penpot` -> `<basename>`, for a vault-relative path.
fn file_display_name(rel_path: &str) -> &str {
    let base = rel_path.rsplit('/').next().unwrap_or(rel_path);
    base.strip_suffix(".penpot").unwrap_or(base)
}

/// Focus the home window (showing/unminimizing it first) and navigate it to
/// `path` on the proxy origin. Used by every View command, and by New
/// Project (there is no window to open for a newly created EMPTY project —
/// see [`create_new_project`]).
fn navigate_home<R: Runtime>(app: &AppHandle<R>, ctx: &MenuCtx, path: &str) {
    let proxy_url = live_snapshot(&ctx.live).proxy_url;
    if proxy_url.is_empty() {
        tracing::info!(path, "menu navigation requested before boot completed; ignoring");
        return;
    }
    let Some(window) = app.get_webview_window(HOME_LABEL) else { return };
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
    let full = format!("{}{}", proxy_url.trim_end_matches('/'), path);
    match full.parse() {
        Ok(url) => {
            let _ = window.navigate(url);
        }
        Err(e) => tracing::error!(full, "bad menu navigation url: {e}"),
    }
}

/// File > Open… (and Open Recent) resolve to a `file_id` via the on-disk
/// sync manifest (`.penpot-sync.json`) — a pure filesystem read, no backend
/// RPC and no access token needed, matching the folder-is-truth architecture.
fn load_manifest(vault_root: &Path) -> Option<sync_core::Manifest> {
    match sync_core::Manifest::load(vault_root) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("menubar: failed to read the sync manifest: {e}");
            None
        }
    }
}

fn open_picked_folder<R: Runtime>(app: &AppHandle<R>, ctx: &MenuCtx, path: &Path) {
    let live = live_snapshot(&ctx.live);
    let Ok(rel) = path.strip_prefix(&live.vault_root) else {
        crate::dialog::native_error_dialog(
            "Penpot Local — Open",
            &format!(
                "{} is outside the active vault ({}) and cannot be opened this way.",
                path.display(),
                live.vault_root.display()
            ),
        );
        return;
    };
    let rel = rel.to_string_lossy().replace('\\', "/");
    let Some(manifest) = load_manifest(&live.vault_root) else {
        crate::dialog::native_error_dialog(
            "Penpot Local — Open",
            "This vault has not synced yet, so nothing is known about that file. Wait a moment and try again.",
        );
        return;
    };
    let Some((file_id, _entry)) = manifest.entry_by_path(&rel) else {
        crate::dialog::native_error_dialog(
            "Penpot Local — Open",
            &format!("{rel} is not a recognized Penpot file in this vault."),
        );
        return;
    };
    let file_id = file_id.to_string();
    let title = file_display_name(&rel).to_string();
    if let Err(e) = open_file_window(app, ctx, &file_id, None, &title) {
        tracing::error!("open_file_window failed: {e}");
        return;
    }
    let _ = recent::record_open(
        &ctx.data_dir,
        RecentEntry {
            file_id,
            title,
            page_id: None,
            opened_at: chrono::Utc::now().to_rfc3339(),
        },
    );
}

fn open_recent<R: Runtime>(app: &AppHandle<R>, ctx: &MenuCtx, file_id: &str) {
    let entries = recent::list_recent(&ctx.data_dir, recent::RECENT_LIMIT);
    let Some(mut entry) = entries.into_iter().find(|e| e.file_id == file_id) else {
        // The store changed under us between the menu being built and the
        // click landing (e.g. RECENT_LIMIT eviction) — nothing to open.
        tracing::warn!(file_id, "Open Recent: id no longer in the recent store");
        return;
    };
    if let Err(e) = open_file_window(app, ctx, &entry.file_id, entry.page_id.as_deref(), &entry.title) {
        tracing::error!("open_file_window failed: {e}");
        return;
    }
    entry.opened_at = chrono::Utc::now().to_rfc3339();
    let _ = recent::record_open(&ctx.data_dir, entry);
}

/// Build an authenticated RPC client from the live vault facts, or `None`
/// before boot / before a token exists. Uses the PROXY origin as the RPC
/// base, not the backend's direct URL — the menu bar is a CLIENT of the
/// local stack, exactly like the SPA itself, and the proxy forwards
/// `/api/*` to the backend, so this is same-origin RPC, not a bypass.
fn rpc_client(live: &LiveVault) -> Option<PenpotClient> {
    if live.proxy_url.is_empty() || live.access_token.is_empty() {
        return None;
    }
    Some(PenpotClient::new(&live.proxy_url).with_auth(Auth::Token(live.access_token.clone())))
}

/// File > New File: create-file over RPC (the SAME RPC `manage.rs`'s
/// `/__api/vault/manage/file` route calls), landing in the team's default
/// ("Drafts") project via `installer::default_project_id` — the same
/// resolver `templates.rs`/`packages.rs` use, so this is not a new policy,
/// just a new caller of an existing one. This goes through the backend
/// (not the filesystem) DELIBERATELY: this task's core invariant is that
/// nothing here writes to the vault directly — only the sync daemon does,
/// once the new file lands in the DB and the daemon exports it on its own
/// schedule. The freshly created file (which already has a default page,
/// just no board yet — see `home.html`'s own "D2 gap fix" note) opens
/// immediately in its own window, exactly like opening any other file.
async fn create_new_file<R: Runtime>(app: &AppHandle<R>, ctx: &MenuCtx) {
    let live = live_snapshot(&ctx.live);
    let Some(client) = rpc_client(&live) else {
        tracing::info!("New File requested before boot completed; ignoring");
        return;
    };
    let project_id = match crate::installer::default_project_id(&client, &live.team_id).await {
        Ok(id) => id,
        Err(e) => {
            crate::dialog::native_error_dialog("Penpot Local — New File failed", &format!("{e:#}"));
            return;
        }
    };
    let name = "New File".to_string();
    match client.create_file(&project_id, &name).await {
        Ok(f) => {
            let page_id = crate::installer::first_page_id(&client, &f.id).await;
            if let Err(e) = open_file_window(app, ctx, &f.id, page_id.as_deref(), &name) {
                tracing::error!("open_file_window failed: {e}");
            }
            let _ = recent::record_open(
                &ctx.data_dir,
                RecentEntry { file_id: f.id, title: name, page_id, opened_at: chrono::Utc::now().to_rfc3339() },
            );
        }
        Err(e) => crate::dialog::native_error_dialog("Penpot Local — New File failed", &format!("{e}")),
    }
}

/// File > New Project: create-project over RPC (the SAME RPC `manage.rs`'s
/// `/__api/vault/manage/project` route calls). There is no window to open
/// for an empty project, so this navigates Home instead — where the new
/// project shows up immediately (manage.rs's `list_projects` reads the DB
/// directly, "includes empty projects the instant they exist") and the user
/// can New File into it.
async fn create_new_project<R: Runtime>(app: &AppHandle<R>, ctx: &MenuCtx) {
    let live = live_snapshot(&ctx.live);
    let Some(client) = rpc_client(&live) else {
        tracing::info!("New Project requested before boot completed; ignoring");
        return;
    };
    match client.create_project(&live.team_id, "New Project").await {
        Ok(_) => navigate_home(app, ctx, "/__home"),
        Err(e) => crate::dialog::native_error_dialog("Penpot Local — New Project failed", &format!("{e}")),
    }
}

/// File > Export…: the SAME `export-binfile` → download RPC pair
/// `manage.rs`'s `duplicate_file` uses (`(false, true)` is the
/// server-accepted flag pair there too) — reads the CURRENT DB state
/// directly rather than whatever the sync daemon last wrote to disk, so a
/// very recent in-app edit the daemon hasn't caught up to yet still exports
/// correctly. Writes only to wherever the user picks (outside the vault);
/// nothing here touches the vault itself.
async fn export_key_file(ctx: &MenuCtx) {
    let live = live_snapshot(&ctx.live);
    let Some(client) = rpc_client(&live) else {
        tracing::info!("Export requested before boot completed; ignoring");
        return;
    };
    let Some(file_id) = ctx.registry.key().and_then(|w| w.file_id) else { return };
    // Best-effort default filename from the sync manifest (a pure disk
    // READ) -- a file that hasn't synced to disk yet can still be exported,
    // it just falls back to a generic default name.
    let default_name = load_manifest(&live.vault_root)
        .and_then(|m| m.files.get(&file_id).map(|e| file_display_name(&e.path).to_string()))
        .unwrap_or_else(|| "export".to_string());
    let exported = match client.export_binfile(&file_id, false, true).await {
        Ok(e) => e,
        Err(e) => {
            crate::dialog::native_error_dialog("Penpot Local — Export failed", &format!("{e}"));
            return;
        }
    };
    let bytes = match client.download_exported_binfile(&exported.uri).await {
        Ok(b) => b,
        Err(e) => {
            crate::dialog::native_error_dialog("Penpot Local — Export failed", &format!("{e}"));
            return;
        }
    };
    let picker_name = format!("{default_name}.zip");
    let Some(dest) = tauri::async_runtime::spawn_blocking(move || {
        crate::dialog::save_file("Export a Penpot file", &picker_name)
    })
    .await
    .ok()
    .flatten() else {
        return;
    };
    // D3-review MINOR fix (finding 5): refuse a destination inside the
    // vault — the folder tree is the core invariant's source of truth, and a
    // stray exported `.zip` dropped into it is not something the sync daemon
    // understands (it is not a `.penpot` binfile directory), so it would
    // just sit there as vault clutter the daemon and the Files list both
    // have no idea what to do with. Mirrors `open_picked_folder`'s existing
    // `strip_prefix(&live.vault_root)` check above, just checking the
    // opposite direction (destination must be OUTSIDE, not inside).
    if dest.strip_prefix(&live.vault_root).is_ok() {
        crate::dialog::native_error_dialog(
            "Penpot Local — Export failed",
            &format!(
                "{} is inside the active vault ({}). The vault's folder tree is the \
                 source of truth for your projects — exporting into it would drop a \
                 stray file the sync daemon doesn't understand. Choose a location \
                 outside the vault.",
                dest.display(),
                live.vault_root.display()
            ),
        );
        return;
    }
    match tauri::async_runtime::spawn_blocking(move || std::fs::write(&dest, bytes)).await {
        Ok(Ok(())) => crate::dialog::native_info_dialog("Penpot Local — Export", "Export complete."),
        Ok(Err(e)) => crate::dialog::native_error_dialog("Penpot Local — Export failed", &format!("{e}")),
        Err(e) => crate::dialog::native_error_dialog("Penpot Local — Export failed", &format!("{e}")),
    }
}

/// File > Import…: the SAME import-as-new-and-settle mechanism
/// `templates.rs`/`packages.rs` use (`installer::import_binfile_and_settle`)
/// — the picked `.zip`'s bytes go straight to the backend over RPC, landing
/// in the team's default project via `installer::default_project_id`. This
/// goes through the backend rather than unzipping into the vault directly
/// for the SAME reason as New File: nothing in this module may write to the
/// vault — only the sync daemon does, once the import lands in the DB. The
/// newly imported file opens immediately, exactly like opening any other.
async fn import_into_vault<R: Runtime>(app: &AppHandle<R>, ctx: &MenuCtx) {
    let live = live_snapshot(&ctx.live);
    let Some(client) = rpc_client(&live) else {
        tracing::info!("Import requested before boot completed; ignoring");
        return;
    };
    let Some(src) = tauri::async_runtime::spawn_blocking(|| {
        crate::dialog::choose_file("Import a Penpot export (.zip)", &["zip"])
    })
    .await
    .ok()
    .flatten() else {
        return;
    };
    let project_id = match crate::installer::default_project_id(&client, &live.team_id).await {
        Ok(id) => id,
        Err(e) => {
            crate::dialog::native_error_dialog("Penpot Local — Import failed", &format!("{e:#}"));
            return;
        }
    };
    let name = src.file_stem().and_then(|s| s.to_str()).unwrap_or("Imported file").to_string();
    let bytes = match tauri::async_runtime::spawn_blocking(move || std::fs::read(&src)).await {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            crate::dialog::native_error_dialog("Penpot Local — Import failed", &format!("could not read the file: {e}"));
            return;
        }
        Err(e) => {
            crate::dialog::native_error_dialog("Penpot Local — Import failed", &format!("{e}"));
            return;
        }
    };
    match crate::installer::import_binfile_and_settle(&client, &project_id, &name, bytes, None).await {
        Ok((file_id, _settle_cycles)) => {
            let page_id = crate::installer::first_page_id(&client, &file_id).await;
            if let Err(e) = open_file_window(app, ctx, &file_id, page_id.as_deref(), &name) {
                tracing::error!("open_file_window failed: {e}");
            }
            let _ = recent::record_open(
                &ctx.data_dir,
                RecentEntry { file_id, title: name, page_id, opened_at: chrono::Utc::now().to_rfc3339() },
            );
        }
        Err(e) => crate::dialog::native_error_dialog("Penpot Local — Import failed", &format!("{e:#}")),
    }
}

/// File > Reveal in Finder: resolve the key window's file to its on-disk
/// path via the sync manifest, then hand off to the existing `reveal.rs`
/// machinery (same one the tray's per-file rows use) — no new OS-integration
/// code here.
fn reveal_key_file(ctx: &MenuCtx) {
    let live = live_snapshot(&ctx.live);
    let Some(file_id) = ctx.registry.key().and_then(|w| w.file_id) else { return };
    let Some(entry) = load_manifest(&live.vault_root).and_then(|m| m.files.get(&file_id).cloned()) else {
        crate::dialog::native_error_dialog(
            "Penpot Local — Reveal",
            "This file has not finished its first sync yet; wait a moment and try again.",
        );
        return;
    };
    crate::reveal::reveal(&live.vault_root.join(&entry.path));
}

/// Exhaustive match over every `Command` variant, deliberately WITHOUT a
/// wildcard `_` arm: adding a new `Command` in `model.rs` without wiring it
/// here fails the BUILD, not a test run later. Do not "simplify" this into a
/// `match command { … , _ => {} }` — that is precisely the silent-orphan
/// failure mode this design exists to prevent.
fn run_command<R: Runtime>(app: &AppHandle<R>, ctx: &MenuCtx, command: Command) {
    match command {
        Command::NewFile => {
            let app = app.clone();
            let ctx = ctx.clone();
            tauri::async_runtime::spawn(async move { create_new_file(&app, &ctx).await });
        }
        Command::NewProject => {
            let app = app.clone();
            let ctx = ctx.clone();
            tauri::async_runtime::spawn(async move { create_new_project(&app, &ctx).await });
        }
        Command::OpenFile => {
            let app = app.clone();
            let ctx = ctx.clone();
            tauri::async_runtime::spawn(async move {
                // `.penpot` is a DIRECTORY on disk (an unzipped binfile), not
                // a file — a file picker cannot select one. `choose_folder`
                // blocks on osascript, so it runs off the async runtime.
                let picked = tauri::async_runtime::spawn_blocking(|| {
                    crate::dialog::choose_folder("Open a Penpot file")
                })
                .await
                .ok()
                .flatten();
                if let Some(path) = picked {
                    open_picked_folder(&app, &ctx, &path);
                }
            });
        }
        Command::OpenRecent(file_id) => {
            let app = app.clone();
            let ctx = ctx.clone();
            tauri::async_runtime::spawn(async move {
                open_recent(&app, &ctx, &file_id);
            });
        }
        Command::OpenVault => match &ctx.on_open_vault {
            Some(cb) => cb(),
            None => tracing::info!("Open Vault requested but unavailable (demo mode, or before boot)"),
        },
        Command::Import => {
            let app = app.clone();
            let ctx = ctx.clone();
            tauri::async_runtime::spawn(async move { import_into_vault(&app, &ctx).await });
        }
        Command::Export => {
            let ctx = ctx.clone();
            tauri::async_runtime::spawn(async move { export_key_file(&ctx).await });
        }
        Command::RevealInFinder => {
            let ctx = ctx.clone();
            tauri::async_runtime::spawn_blocking(move || reveal_key_file(&ctx));
        }
        Command::ShowHome => navigate_home(app, ctx, "/__home"),
        Command::ShowSearch => navigate_home(app, ctx, "/__search"),
        Command::ShowPalette => {
            let proxy_url = live_snapshot(&ctx.live).proxy_url;
            if proxy_url.is_empty() {
                tracing::info!("Show Palette requested before boot completed; ignoring");
            } else {
                // `toggle_palette` wants the app's late-bound `ProxyUrlSlot`
                // type; wrapping the value we already resolved is cheaper
                // than plumbing the actual app-wide slot through MenuCtx
                // just for this one command, and behaves identically since
                // the slot is only ever read, not written, inside it.
                let slot: crate::overlay::ProxyUrlSlot = Arc::new(Mutex::new(Some(proxy_url)));
                crate::overlay::toggle_palette(app, &slot);
            }
        }
        Command::ShowPackages => navigate_home(app, ctx, "/__packages"),
        Command::ShowTemplates => navigate_home(app, ctx, "/__templates"),
        Command::FocusWindow(label) => {
            if let Some(w) = app.get_webview_window(&label) {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }
        Command::About => crate::dialog::native_info_dialog(
            "About Penpot Local",
            &format!(
                "Penpot Local {}\n\nA local-first desktop wrapper around Penpot {}.\n\
                 The folder tree is the source of truth; the app database is a\n\
                 disposable cache that can be deleted and rebuilt at any time.",
                env!("CARGO_PKG_VERSION"),
                "2.16.2"
            ),
        ),
        Command::KnownLimits => crate::dialog::native_info_dialog(
            "Penpot Local — Known Limits",
            "\u{2022} Preferences changes to plugins, the Content Security Policy, or\n  re-enabling renders need a restart of the local stack — the\n  Preferences window offers an explicit \"Apply & Restart\" for those.\n\
             \u{2022} Open…, Import… and Export… use native pickers on macOS only.\n\
             \u{2022} The menu bar is app-wide (a macOS constraint): it always reflects\n  whichever window is currently key, not a per-window menu.\n\
             \u{2022} New Project has no file to open yet, so it opens Home instead,\n  where the new (empty) project appears immediately.",
        ),
        Command::Preferences => {
            let proxy_url = live_snapshot(&ctx.live).proxy_url;
            if proxy_url.is_empty() {
                tracing::info!("Preferences requested before boot completed; ignoring");
            } else {
                // Same "wrap the already-known proxy url into a fresh
                // ProxyUrlSlot" shortcut `Command::ShowPalette` uses above —
                // cheaper than plumbing the app-wide slot through `MenuCtx`
                // just for this one command, and behaves identically since
                // the slot is only ever read here, never written.
                let slot: crate::overlay::ProxyUrlSlot = Arc::new(Mutex::new(Some(proxy_url)));
                crate::overlay::open_preferences(app, &slot);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{build_menu_model, Entry};

    /// The compiler enforces the other half of "no orphaned items" for us:
    /// `run_command` above matches every `Command` variant with NO wildcard
    /// arm, so a variant added to `model.rs` without a matching arm here
    /// fails to compile. This test covers the half the compiler can't: every
    /// `Entry::Item` the model can produce must carry a real (non-empty) id,
    /// since `command_for_id`/`dispatch` key off it.
    #[test]
    fn every_item_in_the_model_has_a_non_empty_id() {
        let recent = [RecentEntry {
            file_id: "f1".into(),
            title: "Alpha".into(),
            page_id: None,
            opened_at: "x".into(),
        }];
        let windows = [
            OpenWindow { label: HOME_LABEL.into(), file_id: None, title: "Penpot Local".into() },
            OpenWindow { label: "file-f1".into(), file_id: Some("f1".into()), title: "Alpha".into() },
        ];
        for key_label in [None, Some("file-f1")] {
            let model = build_menu_model(&windows, &recent, key_label);
            for section in &model.sections {
                for entry in &section.entries {
                    if let Entry::Item(item) = entry {
                        assert!(!item.id.is_empty(), "empty id in section {}", section.title);
                    }
                }
            }
        }
    }

    #[test]
    fn file_display_name_strips_dir_and_extension() {
        assert_eq!(file_display_name("Client A/homepage.penpot"), "homepage");
        assert_eq!(file_display_name("bare.penpot"), "bare");
        assert_eq!(file_display_name("no-extension"), "no-extension");
    }
}
