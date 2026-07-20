//! N4 palette overlay window + global shortcut plumbing.
//!
//! The palette is a proxy-served page (`/__palette`) shown in its own small
//! always-on-top Tauri window. It is reachable two ways (PLAN2.md risk 7:
//! keep it usable/testable even where the global shortcut collides):
//!   - the configurable global shortcut (default Cmd+K / Ctrl+K), and
//!   - the tray "Quick open…" entry.
//!
//! The overlay only exists once the proxy is up (it needs the proxy origin for
//! `/__palette`), so both callers read the origin from a late-bound slot that
//! boot fills in. Before boot the toggle is a logged no-op.
//!
//! These OS-integration pieces (global-shortcut firing, overlay focus) are NOT
//! a local exit criterion — tauri-driver has no macOS support — so they ride a
//! manual-QA checklist (docs/milestones/n4.md); everything they *reach* (the
//! ranked `/__api/vault/palette`, the `/__palette` page) is HTTP-gateable.
//!
//! D4 adds [`open_preferences`], the same "reuse-if-already-open by label"
//! shape as [`toggle_palette`], for the native Preferences window
//! (`/__preferences`) — see that function's doc for the one behavioral
//! difference (it never hides on a second call).

use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder};

/// The palette overlay window label.
pub const PALETTE_LABEL: &str = "palette";

/// D4 — the Preferences window label.
pub const PREFERENCES_LABEL: &str = "preferences";

/// A late-bound holder for the proxy origin (e.g. `http://localhost:8686`),
/// shared by the shortcut handler and the tray. Filled once boot completes.
pub type ProxyUrlSlot = Arc<Mutex<Option<String>>>;

/// The configured palette shortcut, parsed from `PENPOT_LOCAL_PALETTE_SHORTCUT`
/// (e.g. `cmd+k`, `ctrl+shift+p`), defaulting to the platform Cmd/Ctrl+K.
#[cfg(not(any(test)))]
pub fn configured_shortcut() -> tauri_plugin_global_shortcut::Shortcut {
    let raw = std::env::var("PENPOT_LOCAL_PALETTE_SHORTCUT").unwrap_or_default();
    parse_shortcut(&raw).unwrap_or_else(default_shortcut)
}

#[cfg(not(any(test)))]
fn default_shortcut() -> tauri_plugin_global_shortcut::Shortcut {
    use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut};
    #[cfg(target_os = "macos")]
    let mods = Modifiers::SUPER;
    #[cfg(not(target_os = "macos"))]
    let mods = Modifiers::CONTROL;
    Shortcut::new(Some(mods), Code::KeyK)
}

/// Parse a `mod+mod+key` accelerator into a `Shortcut`. Recognizes
/// cmd/super/meta, ctrl/control, alt/option, shift and single letter/`k`-style
/// keys. Returns `None` on anything it cannot map (caller falls back).
#[cfg(not(any(test)))]
fn parse_shortcut(s: &str) -> Option<tauri_plugin_global_shortcut::Shortcut> {
    use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut};
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut mods = Modifiers::empty();
    let mut code: Option<Code> = None;
    for part in s.split('+') {
        let p = part.trim().to_ascii_lowercase();
        match p.as_str() {
            "cmd" | "command" | "super" | "meta" | "win" => mods |= Modifiers::SUPER,
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "alt" | "option" | "opt" => mods |= Modifiers::ALT,
            "shift" => mods |= Modifiers::SHIFT,
            other => {
                code = letter_code(other);
                code?;
            }
        }
    }
    let code = code?;
    let mods = if mods.is_empty() { None } else { Some(mods) };
    Some(Shortcut::new(mods, code))
}

#[cfg(not(any(test)))]
fn letter_code(s: &str) -> Option<tauri_plugin_global_shortcut::Code> {
    use tauri_plugin_global_shortcut::Code;
    let mut ch = s.chars();
    let c = ch.next()?;
    if ch.next().is_some() {
        return None; // multi-char (e.g. "space") not supported here
    }
    Some(match c.to_ascii_uppercase() {
        'A' => Code::KeyA, 'B' => Code::KeyB, 'C' => Code::KeyC, 'D' => Code::KeyD,
        'E' => Code::KeyE, 'F' => Code::KeyF, 'G' => Code::KeyG, 'H' => Code::KeyH,
        'I' => Code::KeyI, 'J' => Code::KeyJ, 'K' => Code::KeyK, 'L' => Code::KeyL,
        'M' => Code::KeyM, 'N' => Code::KeyN, 'O' => Code::KeyO, 'P' => Code::KeyP,
        'Q' => Code::KeyQ, 'R' => Code::KeyR, 'S' => Code::KeyS, 'T' => Code::KeyT,
        'U' => Code::KeyU, 'V' => Code::KeyV, 'W' => Code::KeyW, 'X' => Code::KeyX,
        'Y' => Code::KeyY, 'Z' => Code::KeyZ,
        _ => return None,
    })
}

/// Toggle the palette overlay: create it if absent, else hide/show it. A no-op
/// (logged) if the proxy origin is not known yet (boot still in progress).
pub fn toggle_palette<R: Runtime>(app: &AppHandle<R>, proxy_slot: &ProxyUrlSlot) {
    let origin = proxy_slot.lock().ok().and_then(|g| g.clone());
    let Some(origin) = origin else {
        tracing::info!("palette toggle before boot completed; ignoring");
        return;
    };
    if let Some(win) = app.get_webview_window(PALETTE_LABEL) {
        match win.is_visible() {
            Ok(true) => {
                let _ = win.hide();
            }
            _ => {
                let _ = win.show();
                let _ = win.set_focus();
            }
        }
        return;
    }
    let url = format!("{}/__palette", origin.trim_end_matches('/'));
    match url.parse() {
        Ok(parsed) => {
            match WebviewWindowBuilder::new(app, PALETTE_LABEL, WebviewUrl::External(parsed))
                .title("Quick open")
                .inner_size(680.0, 460.0)
                .min_inner_size(480.0, 300.0)
                .resizable(true)
                .always_on_top(true)
                .decorations(true)
                .focused(true)
                .build()
            {
                Ok(_) => tracing::info!(url = %url, "palette overlay opened"),
                Err(e) => tracing::error!("failed to open palette overlay: {e}"),
            }
        }
        Err(e) => tracing::error!(url = %url, "bad palette url: {e}"),
    }
}

/// D4 — open (or focus, if already open) the Preferences window at
/// `/__preferences`. Reuse-if-already-open BY LABEL, the same mechanism
/// [`toggle_palette`] uses to avoid duplicating the palette window. Unlike
/// the palette this always SHOWS + FOCUSES rather than hiding on a second
/// call: Preferences is opened from a plain menu command
/// (`File > Preferences…` / `CmdOrCtrl+,`), not a toggle shortcut, so
/// invoking it again while the window is already open should bring it
/// forward, never hide it. A no-op (logged) if the proxy origin is not known
/// yet (boot still in progress) — same posture as `toggle_palette`.
pub fn open_preferences<R: Runtime>(app: &AppHandle<R>, proxy_slot: &ProxyUrlSlot) {
    let origin = proxy_slot.lock().ok().and_then(|g| g.clone());
    let Some(origin) = origin else {
        tracing::info!("preferences requested before boot completed; ignoring");
        return;
    };
    if let Some(win) = app.get_webview_window(PREFERENCES_LABEL) {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        return;
    }
    let url = format!("{}/__preferences", origin.trim_end_matches('/'));
    match url.parse() {
        Ok(parsed) => {
            match WebviewWindowBuilder::new(app, PREFERENCES_LABEL, WebviewUrl::External(parsed))
                .title("Preferences")
                .inner_size(600.0, 640.0)
                .min_inner_size(440.0, 420.0)
                .resizable(true)
                .decorations(true)
                .focused(true)
                .build()
            {
                Ok(_) => tracing::info!(url = %url, "preferences window opened"),
                Err(e) => tracing::error!("failed to open preferences window: {e}"),
            }
        }
        Err(e) => tracing::error!(url = %url, "bad preferences url: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_slot_defaults_empty_then_fills() {
        let slot: ProxyUrlSlot = Arc::new(Mutex::new(None));
        assert!(slot.lock().unwrap().is_none());
        *slot.lock().unwrap() = Some("http://localhost:8686".to_string());
        assert_eq!(slot.lock().unwrap().as_deref(), Some("http://localhost:8686"));
    }
}
