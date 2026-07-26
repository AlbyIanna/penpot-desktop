//! Boot pre-flight (M5, PLAN.md risk 8): JDK 26 cannot load classes from a
//! jar whose path contains a **non-BMP** character (emoji / anything needing
//! a UTF-16 surrogate pair) — verified in M4 (docs/milestones/m4.md,
//! adversarial finding 1): from `/tmp/Applicazioni è🎨/` the backend
//! crash-loops with "Could not find or load main class clojure.main", while
//! BMP unicode + spaces (`/tmp/Applicazioni è/`) work fully.
//!
//! So: BEFORE the supervisor starts anything, every load-bearing path is
//! checked; a hit produces one clear, path-naming error (log + window title
//! + a native dialog on macOS) and a clean exit — never a crash-loop.
//!
//! The data dir and designs dir are checked too (conservatively): the JVM
//! reads/writes the assets storage under the data dir and the whole sync
//! path handles the designs dir; the emoji-in-data-dir case was explicitly
//! never proven to work (m4.md "Implications for M5"), so it is refused
//! with the same clear error instead of half-working.

use std::fmt;
use std::path::{Path, PathBuf};

/// A load-bearing path containing a non-BMP character.
#[derive(Debug, Clone)]
pub struct NonBmpPath {
    /// Which configured path is affected (e.g. "install/runtime dir").
    pub label: &'static str,
    pub path: PathBuf,
    /// The first offending character.
    pub offending: char,
}

impl fmt::Display for NonBmpPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the {} path contains the character '{}' (U+{:X}), which the \
             bundled Java runtime cannot handle (JDK limitation with \
             emoji/non-BMP characters in paths). Please move it to a path \
             without emoji: {}",
            self.label, self.offending, self.offending as u32, self.path.display()
        )
    }
}

impl std::error::Error for NonBmpPath {}

/// First character outside the Basic Multilingual Plane (> U+FFFF), if any.
/// BMP unicode (accents, CJK, …) passes; emoji and other astral-plane
/// characters fail.
pub fn first_non_bmp(s: &str) -> Option<char> {
    s.chars().find(|c| *c as u32 > 0xFFFF)
}

fn check_one(label: &'static str, path: &Path) -> Result<(), NonBmpPath> {
    // Non-UTF-8 path bytes cannot contain a supplementary-plane code point
    // that `to_string_lossy` would hide (lossy replacement is U+FFFD, BMP),
    // so the lossy view is exact for this check.
    match first_non_bmp(&path.to_string_lossy()) {
        Some(offending) => Err(NonBmpPath { label, path: path.to_path_buf(), offending }),
        None => Ok(()),
    }
}

/// Check every load-bearing path of a resolved config: the executable
/// location, the runtime/bundle dir, the java binary, the data dir and the
/// designs dir. Returns the FIRST violation (one clear error beats a list).
pub fn check_app_paths(config: &crate::AppConfig) -> Result<(), NonBmpPath> {
    if let Ok(exe) = std::env::current_exe() {
        check_one("application install", &exe)?;
    }
    check_one("install/runtime", &config.runtime_dir)?;
    check_one("java runtime", &config.java_path)?;
    check_one("data directory", &config.data_dir)?;
    check_one("designs folder", &config.designs_dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_bmp_unicode_pass() {
        for ok in [
            "/Applications/Penpot Local.app",
            "/tmp/Applicazioni è/penpot",          // M4-verified working path
            "/Users/björn/Progetti città/ダザイン", // accents + CJK: all BMP
            "/data/ω/π",
        ] {
            assert_eq!(first_non_bmp(ok), None, "{ok} must pass");
        }
    }

    #[test]
    fn emoji_and_astral_plane_fail() {
        // The exact M4 repro path.
        assert_eq!(first_non_bmp("/tmp/Applicazioni è🎨/x"), Some('🎨'));
        assert_eq!(first_non_bmp("/designs/📁"), Some('📁'));
        // Non-emoji astral plane (Gothic letter U+10330) fails too — the JDK
        // limitation is about surrogate pairs, not emoji specifically.
        assert_eq!(first_non_bmp("/data/\u{10330}"), Some('\u{10330}'));
        // U+FFFF boundary: last BMP char passes.
        assert_eq!(first_non_bmp("/data/\u{FFFF}"), None);
    }

    #[test]
    fn error_message_names_the_path_and_character() {
        let err = check_one("data directory", Path::new("/tmp/dati 🎨/penpot")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("data directory"), "{msg}");
        assert!(msg.contains("/tmp/dati 🎨/penpot"), "{msg}");
        assert!(msg.contains("U+1F3A8"), "{msg}");
        assert!(msg.contains("emoji"), "{msg}");
    }

    #[test]
    fn check_one_accepts_clean_paths() {
        assert!(check_one("designs folder", Path::new("/Users/alby/Designs")).is_ok());
    }
}
