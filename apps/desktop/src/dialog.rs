//! Minimal native dialogs (M5). Fire-and-forget `osascript` on macOS — no
//! extra Tauri plugin, no blocking of the calling thread; on other platforms
//! the message goes to the log only (the window title carries it too).

#[cfg(target_os = "macos")]
use std::process::Command;

/// AppleScript string literal escaping (backslash first, then quotes).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn show(title: &str, message: &str, icon: &str) {
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display dialog \"{}\" with title \"{}\" buttons {{\"OK\"}} default button 1 with icon {icon}",
            applescript_escape(message),
            applescript_escape(title),
        );
        match Command::new("osascript").arg("-e").arg(script).spawn() {
            // Reap off-thread: osascript blocks until the user clicks OK,
            // and an unwaited child would stay a zombie after that.
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(e) => tracing::warn!("native dialog unavailable (osascript spawn failed: {e})"),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        tracing::info!(%title, %message, %icon, "native dialogs not implemented on this platform");
    }
}

/// Error dialog (stop icon). Never blocks; failure to show is only logged.
pub fn native_error_dialog(title: &str, message: &str) {
    show(title, message, "stop");
}

/// Informational dialog (note icon).
pub fn native_info_dialog(title: &str, message: &str) {
    show(title, message, "note");
}

/// N5: native "choose folder" picker (blocks until the user picks or cancels).
/// Returns the chosen POSIX path, or `None` on cancel / any error. macOS only
/// (via `osascript`); other platforms return `None` — the GUI picker is a
/// macOS surface, the switch mechanism itself is headless-driven.
#[cfg(target_os = "macos")]
pub fn choose_folder(prompt: &str) -> Option<std::path::PathBuf> {
    let script = format!(
        "POSIX path of (choose folder with prompt \"{}\")",
        applescript_escape(prompt)
    );
    let output = Command::new("osascript").arg("-e").arg(script).output().ok()?;
    if !output.status.success() {
        // Non-zero = user cancelled (osascript error -128) or the dialog
        // failed; either way there is nothing to open.
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(path))
}

/// Non-macOS stub: no native picker available.
#[cfg(not(target_os = "macos"))]
pub fn choose_folder(_prompt: &str) -> Option<std::path::PathBuf> {
    tracing::info!("choose_folder: native folder picker only implemented on macOS");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applescript_escaping_handles_quotes_and_backslashes() {
        assert_eq!(applescript_escape(r#"path "with" quotes"#), r#"path \"with\" quotes"#);
        assert_eq!(applescript_escape(r"C:\x"), r"C:\\x");
        // Escape order matters: a quote must not end up double-escaped.
        assert_eq!(applescript_escape(r#"\""#), r#"\\\""#);
        // Emoji pass through untouched (they appear in preflight messages).
        assert_eq!(applescript_escape("dati 🎨"), "dati 🎨");
    }
}
