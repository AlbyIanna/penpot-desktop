//! Minimal native dialogs (M5). Fire-and-forget `osascript` on macOS — no
//! extra Tauri plugin, no blocking of the calling thread; on other platforms
//! the message goes to the log only (the window title carries it too).

#[cfg(target_os = "macos")]
use std::process::Command;

/// AppleScript string literal escaping: control characters first (mapped to
/// a plain space), then backslash, then quotes.
///
/// D3-review MINOR fix (finding 4): the escaping itself was already
/// injection-safe, but a filename containing `\n`/`\r` (a valid byte in a
/// POSIX filename, however rare) produced an UNTERMINATED AppleScript string
/// literal — the newline breaks the `"..."` onto a second source line —
/// which makes `osascript` fail with a syntax error and the dialog never
/// appears at all, so Export/Import silently aborts with no feedback to the
/// user. Control characters carry no meaning a native dialog can render
/// anyway, so mapping them to a space (rather than stripping them, which
/// could collide two distinct names) is a safe, lossy sanitization done
/// BEFORE the backslash/quote escaping below — it introduces no new
/// backslashes or quotes for that pass to worry about.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn applescript_escape(s: &str) -> String {
    let sanitized: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    sanitized.replace('\\', "\\\\").replace('"', "\\\"")
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

/// D3: AppleScript for a native "choose file" open dialog, filtered to
/// `extensions` (bare, no leading dot — e.g. `"penpot"`, not `".penpot"`).
/// Pure command construction — see `reveal.rs`'s module doc for why this is
/// split out: a dialog cannot be driven headlessly, so everything except the
/// final `Command::spawn`/`::output` call is unit-tested here.
pub fn choose_file_script(prompt: &str, extensions: &[&str]) -> String {
    let types = extensions
        .iter()
        .map(|ext| format!("\"{}\"", applescript_escape(ext)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "POSIX path of (choose file with prompt \"{}\" of type {{{}}})",
        applescript_escape(prompt),
        types
    )
}

/// D3: AppleScript for a native "save file" dialog pre-filled with
/// `default_name`. Pure command construction, same reasoning as
/// [`choose_file_script`].
pub fn save_file_script(prompt: &str, default_name: &str) -> String {
    format!(
        "POSIX path of (choose file name with prompt \"{}\" default name \"{}\")",
        applescript_escape(prompt),
        applescript_escape(default_name)
    )
}

/// D3: native "choose file" open picker, filtered to `extensions`. Blocks
/// until the user picks or cancels. Returns the chosen POSIX path, or `None`
/// on cancel / any error. macOS only (via `osascript`), matching
/// [`choose_folder`]'s convention exactly — other platforms return `None`.
#[cfg(target_os = "macos")]
pub fn choose_file(prompt: &str, extensions: &[&str]) -> Option<std::path::PathBuf> {
    let script = choose_file_script(prompt, extensions);
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
pub fn choose_file(_prompt: &str, _extensions: &[&str]) -> Option<std::path::PathBuf> {
    tracing::info!("choose_file: native file picker only implemented on macOS");
    None
}

/// D3: native "save file" picker pre-filled with `default_name`. Blocks
/// until the user picks a location or cancels. Returns the chosen POSIX
/// path, or `None` on cancel / any error. macOS only, matching
/// [`choose_folder`]'s convention — other platforms return `None`.
#[cfg(target_os = "macos")]
pub fn save_file(prompt: &str, default_name: &str) -> Option<std::path::PathBuf> {
    let script = save_file_script(prompt, default_name);
    let output = Command::new("osascript").arg("-e").arg(script).output().ok()?;
    if !output.status.success() {
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
pub fn save_file(_prompt: &str, _default_name: &str) -> Option<std::path::PathBuf> {
    tracing::info!("save_file: native save picker only implemented on macOS");
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

    #[test]
    fn choose_file_script_escapes_quotes_in_the_prompt() {
        let s = choose_file_script("say \"hi\"", &["penpot"]);
        assert!(!s.contains("say \"hi\""), "unescaped quote breaks out of the AppleScript literal");
        assert!(s.contains("\\\"hi\\\""), "expected escaped quotes in: {s}");
    }

    #[test]
    fn save_file_script_escapes_backslashes_and_quotes_in_the_name() {
        let s = save_file_script("Export", r#"we"ird\name"#);
        assert!(s.contains(r#"\""#));
        assert!(s.contains(r"\\"));
    }

    #[test]
    fn choose_file_script_mentions_every_extension() {
        let s = choose_file_script("Open", &["penpot", "zip"]);
        assert!(s.contains("penpot") && s.contains("zip"), "{s}");
    }

    #[test]
    fn control_characters_are_mapped_to_spaces_not_left_to_break_the_literal() {
        // A raw '\n' or '\r' would break the "..." literal onto a second
        // source line, making osascript fail with a syntax error (finding
        // 4) — the escaped string must contain neither, and no dialog
        // silently vanishing as a result.
        assert_eq!(applescript_escape("line1\nline2"), "line1 line2");
        assert_eq!(applescript_escape("a\rb\tc"), "a b c");
        for s in [applescript_escape("line1\nline2"), applescript_escape("a\rb\tc")] {
            assert!(!s.contains('\n') && !s.contains('\r'), "escaped string still breaks the literal: {s:?}");
        }
    }

    #[test]
    fn a_filename_with_a_newline_still_produces_a_single_line_script() {
        // End-to-end through the two script builders that embed
        // user-controlled strings (the save-name and the open prompt).
        let s = save_file_script("Export", "weird\nname.zip");
        assert_eq!(s.lines().count(), 1, "script must stay a single AppleScript line: {s:?}");
        let s2 = choose_file_script("pick\r\na file", &["penpot"]);
        assert_eq!(s2.lines().count(), 1, "script must stay a single AppleScript line: {s2:?}");
    }
}
