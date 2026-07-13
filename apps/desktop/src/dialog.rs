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
