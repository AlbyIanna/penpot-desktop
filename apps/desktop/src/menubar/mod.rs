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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use penpot_rpc::{Auth, PenpotClient};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, DragDropEvent, Manager, Runtime, WebviewUrl, WebviewWindowBuilder, WindowEvent};

use crate::docopen;
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
                // D5 Task 4: dragging a `.penpot` onto a file window opens
                // it — see [`handle_drop`], the body shared with the home
                // window's identical arm in `main.rs`.
                WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. }) => {
                    handle_drop(&app_for_events, &ctx_for_events, paths);
                }
                _ => {}
            });

            on_window_set_changed(app, ctx);
            Ok(())
        }
    }
}

/// D5 Task 4 (post-review fix): the shared body of the `DragDrop` window-
/// event arm. A drop delivers every dragged path at once; each goes through
/// the SAME [`open_document`] funnel every other arrival path uses (CLI
/// argv, second launch, `RunEvent::Opened`). Caught NATIVELY by Tauri's
/// window-event loop, never by a script injected into the SPA (invariant 3).
/// A `.penpot` is a DIRECTORY on disk, so `docopen::resolve`'s own
/// `is_dir`/`.penpot`-suffix checks already route anything else to
/// `NotAPenpotDir` — no pre-filtering needed here.
///
/// Both the home window (`main.rs` — the primary and usually ONLY window at
/// launch) and every file window ([`open_file_window`] below) wire this to
/// their own `WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. })` arm.
/// Pulled out once two call sites needed the identical body, rather than
/// hand-copying a loop over `open_document` a second time.
pub fn handle_drop<R: Runtime>(app: &AppHandle<R>, ctx: &MenuCtx, paths: &[PathBuf]) {
    for path in paths {
        open_document(app, ctx, path);
    }
}

// ---------------------------------------------------------------------------
// D5: "open a document" — the one funnel every arrival path uses
// ---------------------------------------------------------------------------

/// D5 Task 3 — what [`open_document`] does for each [`docopen::Resolved`]
/// outcome, pulled out as a PURE decision (no `AppHandle`, no I/O) so it is
/// unit-testable without a Tauri runtime — the same "decision vs. dumb Tauri
/// glue" split as `windows::reuse_or_create`/`Reuse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentAction {
    /// A known in-vault file: open (or focus) its window.
    OpenInVault { file_id: String, title: String },
    /// A `.penpot` outside the vault. Task 5 wires the copy-in + import
    /// offer; for now the caller only logs what was skipped.
    OfferImport { path: PathBuf },
    /// A `.penpot` inside the vault the daemon hasn't imported yet. Task 5
    /// wires the poll-until-id; for now the caller only logs what was
    /// skipped.
    WaitForImport { rel_path: String },
    /// Not a Penpot document at all.
    Reject { reason: String },
}

/// [`docopen::Resolved`] -> [`DocumentAction`]. A 1:1 mapping today, but kept
/// as its own function (rather than matching `Resolved` directly in
/// [`open_document`]) so the ROUTING DECISION — which is what Task 8's gate
/// and this task's own test care about — is exercised without needing a
/// manifest, a vault root, or a Tauri app at all.
pub fn document_action(resolved: docopen::Resolved) -> DocumentAction {
    match resolved {
        docopen::Resolved::InVault { file_id, title } => DocumentAction::OpenInVault { file_id, title },
        docopen::Resolved::External { path } => DocumentAction::OfferImport { path },
        docopen::Resolved::PendingImport { rel_path } => DocumentAction::WaitForImport { rel_path },
        docopen::Resolved::NotAPenpotDir { reason } => DocumentAction::Reject { reason },
    }
}

/// Open `raw_path` as a document: resolve it against the ACTIVE vault's
/// manifest ([`docopen::resolve`] — never another vault, per the zero-spill
/// invariant: `live.vault_root` only comes from `ctx.live`, the same
/// late-bound slot every other menu-bar command reads) and act on the
/// result via [`document_action`].
///
/// This is the ONE funnel every "open a document" entry point calls into —
/// the first-launch CLI argument, a second launch forwarding its argv, and
/// `RunEvent::Opened` (Finder/`open`, macOS) all fed by `main.rs`; drag-drop
/// (Task 4) reuses it too. Getting the routing decision right once here is
/// the whole point of splitting it into [`document_action`] above.
///
/// `InVault` opens exactly like every other "open a file" path —
/// [`open_file_window`], same reuse-or-create, same navigation policy,
/// nothing forked. `OfferImport` confirms with the user then copies the
/// `.penpot` INTO the vault ([`offer_import`]); `WaitForImport` skips
/// straight to polling ([`poll_until_imported_and_open`]) since the dir is
/// already on disk. `Reject` raises a native error dialog — every call site
/// that reaches `open_document` today is itself a direct user gesture (a CLI
/// argument, a second launch, a Finder open), so there is no "quiet
/// background rescan" case here that a dialog would spam.
pub fn open_document<R: Runtime>(app: &AppHandle<R>, ctx: &MenuCtx, raw_path: &Path) {
    let live = live_snapshot(&ctx.live);
    if live.vault_root.as_os_str().is_empty() {
        tracing::info!(
            path = %raw_path.display(),
            "open_document called before boot completed; ignoring"
        );
        return;
    }
    let Some(manifest) = load_manifest(&live.vault_root) else {
        // Mirrors `open_picked_folder`'s same guard: a vault that hasn't
        // synced yet has nothing to resolve against. Not destructive, so a
        // log (not a dialog) is enough — the manifest appears within one
        // sync-daemon poll and the next open attempt will succeed.
        tracing::warn!(
            path = %raw_path.display(),
            "open_document: vault has not synced yet; cannot resolve"
        );
        return;
    };
    let resolved = docopen::resolve(raw_path, &live.vault_root, &manifest);
    match document_action(resolved) {
        DocumentAction::OpenInVault { file_id, title } => {
            if let Err(e) = open_file_window(app, ctx, &file_id, None, &title) {
                tracing::error!("open_file_window failed: {e}");
            }
        }
        DocumentAction::OfferImport { path } => {
            let app = app.clone();
            let ctx = ctx.clone();
            tauri::async_runtime::spawn(async move { offer_import(&app, &ctx, path).await });
        }
        DocumentAction::WaitForImport { rel_path } => {
            let app = app.clone();
            let ctx = ctx.clone();
            let vault_root = live.vault_root.clone();
            tauri::async_runtime::spawn(async move {
                poll_until_imported_and_open(&app, &ctx, &vault_root, &rel_path).await;
            });
        }
        DocumentAction::Reject { reason } => {
            tracing::warn!(path = %raw_path.display(), reason, "open_document: not a Penpot document");
            crate::dialog::native_error_dialog(
                "Penpot Local — Open",
                &format!("{} is not a Penpot document.\n\n{reason}", raw_path.display()),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// D5 Task 5: offer to import an external `.penpot`, and the poll-until-id
// tail shared with `PendingImport`
// ---------------------------------------------------------------------------

/// How long [`poll_until_imported_and_open`] keeps re-reading the manifest
/// before giving up. The daemon's own filesystem debounce is ~2s
/// (`sync_daemon::SyncConfig::fs_debounce`'s default) before Direction B
/// even STARTS the `import-binfile` RPC round trip that assigns the file its
/// id — this timeout has to clear that plus real network/DB time with
/// margin, not just edge past 2s. 20s is several debounce cycles' worth.
const IMPORT_POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// How often [`poll_until_imported_and_open`] re-reads the manifest between
/// [`IMPORT_POLL_TIMEOUT`] checks. Cheap (one small JSON file off disk), so
/// sub-second is fine — this is not the daemon's own poll loop, just this
/// window waiting on it.
const IMPORT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Blocking I/O: recursively copy `src` into a FRESH directory at `dst`
/// (`dst` must not already exist — `create_dir` fails closed if it does).
/// Symlinks are skipped rather than followed, so a symlink inside a
/// `.penpot` tree can never smuggle content from outside `src` into the
/// vault. Always called with `dst` a `.penpot.tmp-*` staging sibling (never
/// the final `.penpot` name directly) — see [`copy_into_vault`].
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else if file_type.is_symlink() {
            tracing::warn!(
                path = %entry.path().display(),
                "skipping a symlink while copying a .penpot into the vault"
            );
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

/// Blocking I/O: copy `source` (named `source_name`) into the active vault
/// under [`docopen::IMPORT_PROJECT_DIR`], returning the vault-relative path
/// it now lives at.
///
/// **Core invariant, not just a nicety:** the copy is built ENTIRELY inside
/// a `.penpot.tmp-*` staging sibling via the SAME two-phase swap primitives
/// (`sync_core::stage_path_for`/`commit_dir_swap`) Direction A's own export
/// path uses — never written straight to the final `.penpot` name. The
/// staging name is already invisible to the daemon's filesystem watcher (see
/// `watcher.rs`'s `.penpot.tmp-`/`.penpot.old-` ignore rules), so a crash or
/// I/O failure partway through the copy leaves only an ignored, orphaned
/// staging dir — never a half-written tree sitting under a REAL `.penpot`
/// name where the daemon could import it half-formed. `commit_dir_swap`'s
/// final rename is what makes the directory visible under its real name at
/// all, and that rename is a single filesystem operation, not a partial one.
/// A leftover staging dir from a failed copy is swept by the daemon's own
/// startup `cleanup_orphans` sweep, same as any other interrupted swap.
fn copy_into_vault(vault_root: &Path, source: &Path, source_name: &str) -> anyhow::Result<String> {
    use anyhow::Context;

    let project_dir = vault_root.join(docopen::IMPORT_PROJECT_DIR);
    std::fs::create_dir_all(&project_dir)
        .with_context(|| format!("creating {}", project_dir.display()))?;

    // What's already there, so `import_target_rel_path` never overwrites an
    // existing import. A plain directory listing (not the manifest): a
    // just-copied sibling from a prior import may not have an id yet.
    let taken: HashSet<String> = std::fs::read_dir(&project_dir)
        .with_context(|| format!("reading {}", project_dir.display()))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .map(|n| format!("{}/{n}", docopen::IMPORT_PROJECT_DIR))
        })
        .collect();
    let rel_path = docopen::import_target_rel_path(source_name, &taken);
    let target = vault_root.join(&rel_path);

    let staged = sync_core::stage_path_for(&target);
    if let Err(e) = copy_dir_recursive(source, &staged) {
        // Best-effort cleanup of a partial staging copy; it would otherwise
        // just sit there until the next startup sweep.
        let _ = std::fs::remove_dir_all(&staged);
        return Err(e).with_context(|| format!("copying {} into the vault", source.display()));
    }
    sync_core::commit_dir_swap(&staged, &target)
        .with_context(|| format!("moving the copy into place at {}", target.display()))?;
    Ok(rel_path)
}

/// `DocumentAction::OfferImport`: confirm with the user (native dialog,
/// fails closed on "no"/dismiss — see [`crate::dialog::native_confirm_dialog`]),
/// then COPY (never move — `source` is the user's own file, left untouched)
/// the `.penpot` into the active vault, then hand off to
/// [`poll_until_imported_and_open`] for the daemon to pick it up on its own
/// schedule.
///
/// The vault used for the copy AND the poll is resolved by
/// [`resolve_post_confirm_vault`] **after** the confirm dialog returns, not
/// before it is shown: `native_confirm_dialog` runs in a separate
/// `osascript` process and does not block the Tauri event loop, so a vault
/// switch (menu action N5, "Open Vault…") is reachable while the dialog is
/// up. There is also an earlier, non-authoritative check before the dialog
/// — it exists only to skip popping a dialog the app can't act on yet (boot
/// not complete); its value is discarded and never reused for the copy or
/// the poll. This is the zero-cross-vault-spill discipline every other
/// command in this module already follows: an N5 switch mid-flow must
/// never make this land in the vault that was active when the user
/// clicked, only the one active when the copy actually happens.
async fn offer_import<R: Runtime>(app: &AppHandle<R>, ctx: &MenuCtx, source: PathBuf) {
    // Fast, NON-authoritative pre-dialog gate: boot not having completed
    // yet is by far the likeliest reason `vault_root` is empty, so bail
    // before even showing the dialog rather than making the user click
    // through a confirm that can't do anything. This snapshot is discarded
    // right after the check — see the doc comment above for why.
    if live_snapshot(&ctx.live).vault_root.as_os_str().is_empty() {
        tracing::info!(path = %source.display(), "import offer requested before boot completed; ignoring");
        return;
    }
    let Some(source_name) = source.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
        crate::dialog::native_error_dialog(
            "Penpot Local — Import",
            &format!("{} has no usable file name.", source.display()),
        );
        return;
    };

    let message = format!(
        "\"{source_name}\" is outside your Penpot Local vault.\n\n\
         Import a COPY into your vault? The original file is left untouched."
    );
    let confirmed = tauri::async_runtime::spawn_blocking(move || {
        crate::dialog::native_confirm_dialog("Penpot Local — Import", &message)
    })
    .await
    .unwrap_or(false); // a panicked confirm task is exactly a "no", not a crash
    if !confirmed {
        return;
    }

    // THE authoritative snapshot: taken fresh right here, after the confirm
    // dialog has closed, so it reflects a vault switch that happened while
    // the dialog was up. Used for BOTH the copy target and the poll below.
    let Some(vault_root) = resolve_post_confirm_vault(&ctx.live) else {
        tracing::info!(
            path = %source.display(),
            "vault facts not ready when import was confirmed; ignoring",
        );
        return;
    };

    let copy_result = tauri::async_runtime::spawn_blocking({
        let source = source.clone();
        let source_name = source_name.clone();
        let vault_root = vault_root.clone();
        move || copy_into_vault(&vault_root, &source, &source_name)
    })
    .await;
    let rel_path = match copy_result {
        Ok(Ok(rel_path)) => rel_path,
        Ok(Err(e)) => {
            crate::dialog::native_error_dialog("Penpot Local — Import failed", &format!("{e:#}"));
            return;
        }
        Err(e) => {
            crate::dialog::native_error_dialog("Penpot Local — Import failed", &format!("{e}"));
            return;
        }
    };

    poll_until_imported_and_open(app, ctx, &vault_root, &rel_path).await;
}

/// The decision half of the post-confirmation re-snapshot used by
/// [`offer_import`]: `None` means the vault facts aren't ready (mirrors the
/// pre-dialog gate there) and the caller must bail rather than copy into an
/// empty path. Split out so the load-bearing property — "the vault used is
/// the one read AFTER confirmation, not the one read before the dialog was
/// shown" — is unit-testable without a Tauri runtime or a real dialog: a
/// test can snapshot a slot, mutate it (standing in for an N5 switch while
/// the dialog is up), then assert this function returns the MUTATED value.
fn resolve_post_confirm_vault(slot: &LiveVaultSlot) -> Option<PathBuf> {
    let live = live_snapshot(slot);
    if live.vault_root.as_os_str().is_empty() {
        None
    } else {
        Some(live.vault_root)
    }
}

/// Poll the vault's manifest for `rel_path` until the sync daemon assigns it
/// a file id (bounded by [`IMPORT_POLL_TIMEOUT`]), then open it — the shared
/// tail of `OfferImport` (called once the copy has landed) and
/// `WaitForImport` (called directly; there is nothing to copy, the dir is
/// already on disk). Never hangs: [`docopen::poll_outcome`] is the pure
/// decision of when to give up, this loop only supplies the read and the
/// sleep. On timeout this surfaces a clear, non-alarming message and
/// returns — the daemon has NOT failed, it just hasn't gotten there yet, and
/// the file will appear on Home (or a later Open attempt will succeed) the
/// moment it does.
async fn poll_until_imported_and_open<R: Runtime>(
    app: &AppHandle<R>,
    ctx: &MenuCtx,
    vault_root: &Path,
    rel_path: &str,
) {
    let title = docopen::display_title(rel_path);
    let started = tokio::time::Instant::now();
    loop {
        let vault_root_owned = vault_root.to_path_buf();
        let rel_path_owned = rel_path.to_string();
        let found_file_id = tauri::async_runtime::spawn_blocking(move || {
            sync_core::Manifest::load(&vault_root_owned)
                .ok()
                .flatten()
                .and_then(|m| m.entry_by_path(&rel_path_owned).map(|(id, _)| id.to_string()))
        })
        .await
        .ok()
        .flatten();

        match docopen::poll_outcome(found_file_id.as_deref(), started.elapsed(), IMPORT_POLL_TIMEOUT) {
            docopen::PollOutcome::Ready(file_id) => {
                if let Err(e) = open_file_window(app, ctx, &file_id, None, &title) {
                    tracing::error!("open_file_window failed: {e}");
                }
                return;
            }
            docopen::PollOutcome::Waiting => {
                tokio::time::sleep(IMPORT_POLL_INTERVAL).await;
            }
            docopen::PollOutcome::TimedOut => {
                crate::dialog::native_info_dialog(
                    "Penpot Local — Import",
                    &format!(
                        "\"{title}\" is still importing — it'll appear on your \
                         home shortly."
                    ),
                );
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// D5 Task 6: the window title tracks a rename
// ---------------------------------------------------------------------------

/// Pure decision: given the currently open windows and a freshly re-read
/// manifest, which windows need `set_title` because their file's on-disk
/// name/location moved since the window opened (or was last retitled)?
/// Reuses [`docopen::display_title`] — the SAME `<project>/<name>.penpot`
/// -> `<name>` rule [`open_document`]/[`open_file_window`] already apply at
/// open time, so a rename can never disagree with what a fresh open of the
/// same file would show. Windows with no `file_id` (the home window) and
/// file ids not (yet) in `manifest` are skipped, not errored — a file id
/// briefly missing from a stale-mid-reload manifest just means the next
/// signal retries with a fresher read.
pub fn title_updates_for_rename(
    open_windows: &[OpenWindow],
    manifest: &sync_core::Manifest,
) -> Vec<(String, String)> {
    open_windows
        .iter()
        .filter_map(|w| {
            let file_id = w.file_id.as_deref()?;
            let entry = manifest.files.get(file_id)?;
            let new_title = docopen::display_title(&entry.path);
            (new_title != w.title).then(|| (w.label.clone(), new_title))
        })
        .collect()
}

/// Apply [`title_updates_for_rename`]'s decisions: `window.set_title` for
/// each affected label, and re-`insert` the registry entry with the new
/// title so the Window menu (built from `ctx.registry.list()`) and the next
/// diff both see it too — mirrors how [`open_file_window`] inserts on
/// create.
fn apply_title_updates<R: Runtime>(app: &AppHandle<R>, ctx: &MenuCtx, updates: Vec<(String, String)>) {
    if updates.is_empty() {
        return;
    }
    let open_windows = ctx.registry.list();
    for (label, new_title) in updates {
        if let Some(w) = app.get_webview_window(&label) {
            if let Err(e) = w.set_title(&new_title) {
                tracing::warn!(label, "failed to retitle window after rename: {e}");
            }
        }
        if let Some(existing) = open_windows.iter().find(|w| w.label == label) {
            ctx.registry.insert(OpenWindow {
                label: label.clone(),
                file_id: existing.file_id.clone(),
                title: new_title,
            });
        }
    }
    on_window_set_changed(app, ctx);
}

/// D5 Task 6 — subscribe to the sync daemon's status watch (the SAME
/// channel already driving the tray icon and the home page's activity
/// strip — see `status.rs`'s module doc) and retitle any open file window
/// whose file changed name/location on disk. This reacts to an EXISTING
/// signal instead of adding a new poll loop, per the task's own
/// constraint: the daemon already ticks this channel
/// (`watch::Sender::send_if_modified`) on every sync cycle that actually
/// changed something, and a rename/move is exactly such a change (D2's
/// relocation logic re-keys the affected file's manifest entry AND its
/// `SyncStatusSnapshot.files` entry in the same pass — see
/// `engine.rs::relocate_file_if_needed`). So "the status channel just
/// ticked" is a cheap, correct trigger to re-check window titles; the
/// manifest read that follows is a plain disk read (the SAME one
/// `open_document`/`open_picked_folder` already do), not a timer of its
/// own.
///
/// Survives an N5 vault switch without resubscribing: `status_rx` is bound
/// to [`crate::status::DaemonStatusBridge`]'s own channel, which a switch's
/// `attach` call re-points at the new daemon (see that module's doc) — so
/// this loop keeps working against whichever vault is active, reading
/// `ctx.live`/`ctx.registry` (both `Arc`-shared) fresh on every iteration.
pub fn watch_rename_titles<R: Runtime>(
    app: AppHandle<R>,
    ctx: MenuCtx,
    mut status_rx: tokio::sync::watch::Receiver<sync_daemon::SyncStatusSnapshot>,
) {
    tauri::async_runtime::spawn(async move {
        loop {
            if status_rx.changed().await.is_err() {
                // The bridge's sender only drops at app shutdown — nothing
                // left to watch.
                break;
            }
            let vault_root = live_snapshot(&ctx.live).vault_root;
            if vault_root.as_os_str().is_empty() {
                continue;
            }
            let Some(manifest) = load_manifest(&vault_root) else { continue };
            let updates = title_updates_for_rename(&ctx.registry.list(), &manifest);
            apply_title_updates(&app, &ctx, updates);
        }
    });
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

    // -----------------------------------------------------------------
    // D5 Task 3 — `open_document`'s routing decision, extracted into
    // `document_action` so it is testable without a Tauri runtime, a
    // manifest, or a vault root (mirrors `windows::reuse_or_create`'s split
    // between the pure decision and the dumb Tauri glue around it).
    // -----------------------------------------------------------------

    #[test]
    fn an_in_vault_resolution_opens_the_file() {
        let action = document_action(docopen::Resolved::InVault {
            file_id: "fid1".into(),
            title: "Home".into(),
        });
        assert_eq!(
            action,
            DocumentAction::OpenInVault { file_id: "fid1".into(), title: "Home".into() }
        );
    }

    #[test]
    fn an_external_resolution_offers_import_not_opens() {
        let path = PathBuf::from("/outside/Loose.penpot");
        let action = document_action(docopen::Resolved::External { path: path.clone() });
        assert_eq!(action, DocumentAction::OfferImport { path });
    }

    #[test]
    fn a_pending_import_resolution_waits_not_opens() {
        let action = document_action(docopen::Resolved::PendingImport {
            rel_path: "Proj/New.penpot".into(),
        });
        assert_eq!(
            action,
            DocumentAction::WaitForImport { rel_path: "Proj/New.penpot".into() }
        );
    }

    #[test]
    fn a_non_penpot_resolution_is_rejected() {
        let action = document_action(docopen::Resolved::NotAPenpotDir {
            reason: "notes is not a directory".into(),
        });
        assert_eq!(
            action,
            DocumentAction::Reject { reason: "notes is not a directory".into() }
        );
    }

    // -----------------------------------------------------------------
    // D5 Task 6 — `title_updates_for_rename`'s pure decision: which open
    // windows need `set_title` because their file's manifest path moved.
    // Mirrors `docopen::tests`' manifest-construction shape.
    // -----------------------------------------------------------------

    fn manifest_with(entries: &[(&str, &str)]) -> sync_core::Manifest {
        let mut m = sync_core::Manifest::default();
        for (id, path) in entries {
            m.files.insert(
                (*id).to_string(),
                sync_core::manifest::ManifestEntry {
                    path: (*path).to_string(),
                    project_id: "p".into(),
                    project_name: "P".into(),
                    revn: 1,
                    db_modified_at: String::new(),
                    last_synced_hash: "h".into(),
                    last_synced_at: "2026-07-20T00:00:00Z".into(),
                },
            );
        }
        m
    }

    #[test]
    fn a_renamed_file_s_window_gets_a_title_update() {
        let manifest = manifest_with(&[("fid1", "Client A/new-name.penpot")]);
        let windows = [OpenWindow {
            label: "file-fid1".into(),
            file_id: Some("fid1".into()),
            title: "old-name".into(),
        }];
        let updates = title_updates_for_rename(&windows, &manifest);
        assert_eq!(updates, vec![("file-fid1".to_string(), "new-name".to_string())]);
    }

    #[test]
    fn an_unchanged_file_s_window_needs_no_update() {
        let manifest = manifest_with(&[("fid1", "Client A/same-name.penpot")]);
        let windows = [OpenWindow {
            label: "file-fid1".into(),
            file_id: Some("fid1".into()),
            title: "same-name".into(),
        }];
        assert!(title_updates_for_rename(&windows, &manifest).is_empty());
    }

    #[test]
    fn the_home_window_and_a_file_missing_from_the_manifest_are_skipped() {
        // The home window has no `file_id` (nothing to look up); a file
        // dropped from the manifest (briefly true mid-vault-switch, before
        // the new manifest loads) has nothing to compare against yet —
        // both are "not my job this round", not an update, and never a
        // panic.
        let manifest = manifest_with(&[]);
        let windows = [
            OpenWindow { label: HOME_LABEL.into(), file_id: None, title: "Penpot Local".into() },
            OpenWindow { label: "file-gone".into(), file_id: Some("gone".into()), title: "Ghost".into() },
        ];
        assert!(title_updates_for_rename(&windows, &manifest).is_empty());
    }

    #[test]
    fn a_move_to_a_different_project_updates_the_title_from_the_new_basename() {
        // A move (not just a rename) changes the whole `path`, but the
        // TITLE only ever reflects the basename (`docopen::display_title`)
        // — moving "Client A/home.penpot" to "Client B/home.penpot" is a
        // no-op for the title even though the manifest path changed.
        let manifest = manifest_with(&[("fid1", "Client B/home.penpot")]);
        let windows = [OpenWindow {
            label: "file-fid1".into(),
            file_id: Some("fid1".into()),
            title: "home".into(),
        }];
        assert!(title_updates_for_rename(&windows, &manifest).is_empty());
    }

    // -----------------------------------------------------------------
    // D5 Task 5 — `copy_dir_recursive`/`copy_into_vault`: real filesystem
    // I/O (no Tauri needed), so these run against a `tempfile::tempdir`
    // rather than mocking anything.
    // -----------------------------------------------------------------

    /// Build a tiny `.penpot` fixture: a couple of files, one nested dir —
    /// enough to prove the copy is recursive and byte-faithful, not a
    /// realistic binfile.
    fn make_fixture_penpot(dir: &Path) {
        std::fs::create_dir_all(dir.join("files")).unwrap();
        std::fs::write(dir.join("manifest.json"), b"{\"ok\":true}").unwrap();
        std::fs::write(dir.join("files/page1.json"), b"{\"shapes\":[]}").unwrap();
    }

    #[test]
    fn copy_dir_recursive_copies_nested_files_byte_for_byte() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("Loose.penpot");
        make_fixture_penpot(&src);
        let dst = tmp.path().join("staged");

        copy_dir_recursive(&src, &dst).unwrap();

        assert_eq!(std::fs::read(dst.join("manifest.json")).unwrap(), b"{\"ok\":true}");
        assert_eq!(std::fs::read(dst.join("files/page1.json")).unwrap(), b"{\"shapes\":[]}");
        // The source is untouched — this is a COPY, never a move.
        assert!(src.join("manifest.json").exists());
    }

    #[test]
    fn copy_dir_recursive_skips_symlinks_rather_than_following_them() {
        #[cfg(unix)]
        {
            let tmp = tempfile::tempdir().unwrap();
            let src = tmp.path().join("Loose.penpot");
            make_fixture_penpot(&src);
            let outside_secret = tmp.path().join("outside-secret.json");
            std::fs::write(&outside_secret, b"not part of the vault").unwrap();
            std::os::unix::fs::symlink(&outside_secret, src.join("sneaky-link.json")).unwrap();

            let dst = tmp.path().join("staged");
            copy_dir_recursive(&src, &dst).unwrap();

            assert!(!dst.join("sneaky-link.json").exists(), "a symlink must not be followed into the vault");
            assert!(dst.join("manifest.json").exists(), "real files still copy");
        }
    }

    #[test]
    fn copy_into_vault_lands_under_the_imported_project_folder() {
        let vault = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let src = outside.path().join("Loose Ideas.penpot");
        make_fixture_penpot(&src);

        let rel = copy_into_vault(vault.path(), &src, "Loose Ideas.penpot").unwrap();

        assert_eq!(rel, "Imported/Loose Ideas.penpot");
        let dest = vault.path().join(&rel);
        assert!(dest.is_dir(), "the copy must land at the returned rel path");
        assert_eq!(std::fs::read(dest.join("manifest.json")).unwrap(), b"{\"ok\":true}");
        // The copy landed via a rename into the FINAL name — no `.tmp-*`
        // staging sibling left behind in the project folder.
        let leftovers: Vec<_> = std::fs::read_dir(vault.path().join(docopen::IMPORT_PROJECT_DIR))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["Loose Ideas.penpot".to_string()]);
        // Source untouched (copy, not move).
        assert!(src.join("manifest.json").exists());
    }

    #[test]
    fn copy_into_vault_deconflicts_a_second_import_of_the_same_name() {
        let vault = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let src = outside.path().join("dup.penpot");
        make_fixture_penpot(&src);

        let first = copy_into_vault(vault.path(), &src, "dup.penpot").unwrap();
        let second = copy_into_vault(vault.path(), &src, "dup.penpot").unwrap();

        assert_eq!(first, "Imported/dup.penpot");
        assert_eq!(second, "Imported/dup-2.penpot");
        assert!(vault.path().join(&first).is_dir());
        assert!(vault.path().join(&second).is_dir());
    }

    // -----------------------------------------------------------------
    // `resolve_post_confirm_vault` — the fix for the cross-vault-spill
    // window in `offer_import`: the native confirm dialog runs in a
    // separate `osascript` process and does not block the Tauri event
    // loop, so a `File > Open Vault` switch (N5) is reachable while it is
    // up. These tests can't drive a real dialog or event loop, but they
    // pin down the actual decision: the vault used for the copy+poll must
    // be the one read from the slot AFTER confirmation, reflecting any
    // switch that happened in between — not a value captured earlier.
    // -----------------------------------------------------------------

    #[test]
    fn resolve_post_confirm_vault_reflects_a_switch_that_happened_after_an_earlier_read() {
        let slot: LiveVaultSlot = Arc::new(Mutex::new(LiveVault {
            vault_root: PathBuf::from("/vaults/A"),
            ..Default::default()
        }));

        // Stand-in for the OLD, buggy pre-dialog snapshot: what the old
        // code would have used for the copy+poll target.
        let pre_confirm = live_snapshot(&slot);
        assert_eq!(pre_confirm.vault_root, PathBuf::from("/vaults/A"));

        // Stand-in for an N5 `File > Open Vault` switch happening while the
        // (non-blocking) confirm dialog is still up.
        *slot.lock().unwrap() = LiveVault { vault_root: PathBuf::from("/vaults/B"), ..Default::default() };

        // The fix: resolving AFTER confirmation must see vault B, the one
        // active now — never vault A, the one active when the user clicked.
        let resolved = resolve_post_confirm_vault(&slot);
        assert_eq!(
            resolved,
            Some(PathBuf::from("/vaults/B")),
            "must use the vault active after confirmation, not the one read before the dialog"
        );
    }

    #[test]
    fn resolve_post_confirm_vault_bails_cleanly_when_vault_facts_are_not_ready() {
        let slot: LiveVaultSlot = Arc::new(Mutex::new(LiveVault::default()));
        assert_eq!(resolve_post_confirm_vault(&slot), None);
    }
}
