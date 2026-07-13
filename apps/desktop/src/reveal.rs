//! "Reveal in file manager" (M5): open the user's designs folder, or reveal
//! a specific `.penpot` directory selected/highlighted inside its enclosing
//! folder. GUI-only by nature (there is nothing to reveal headless), so all
//! the logic lives in **pure command construction** functions that are
//! unit-tested; the only untestable part is `Command::spawn`.
//!
//! No `tauri-plugin-opener` dependency: the plugin's `reveal_item_in_dir`
//! shells out to the same OS verbs these three lines produce, and a direct
//! `open -R` keeps the dependency surface flat.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Target OS for command construction (parameter instead of `cfg!` so every
/// branch is unit-testable from any host).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Linux,
    Windows,
}

impl Os {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(target_os = "windows") {
            Os::Windows
        } else {
            Os::Linux
        }
    }
}

/// A fully constructed (program, args) pair — the pure, testable output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsCommand {
    pub program: &'static str,
    pub args: Vec<PathBuf>,
    /// Fixed flag arguments that precede the path (kept separate because
    /// on Windows `explorer` wants `/select,` *prepended to* the path).
    pub flag: Option<&'static str>,
}

impl OsCommand {
    fn spawn(&self) {
        let mut cmd = Command::new(self.program);
        if let Some(flag) = self.flag {
            cmd.arg(flag);
        }
        cmd.args(&self.args);
        match cmd.spawn() {
            // Fire-and-forget, but reap: `open` exits immediately and an
            // unwaited child would linger as a zombie for the app's lifetime.
            Ok(mut child) => {
                tracing::info!(program = self.program, args = ?self.args, "opened file manager");
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(e) => tracing::error!(program = self.program, args = ?self.args, "file-manager open failed: {e}"),
        }
    }
}

/// Command that opens `dir` itself in the file manager
/// (tray: "Open Designs Folder").
pub fn open_folder_command(os: Os, dir: &Path) -> OsCommand {
    match os {
        Os::MacOs => OsCommand { program: "open", flag: None, args: vec![dir.to_path_buf()] },
        Os::Linux => OsCommand { program: "xdg-open", flag: None, args: vec![dir.to_path_buf()] },
        Os::Windows => OsCommand { program: "explorer", flag: None, args: vec![dir.to_path_buf()] },
    }
}

/// Command that reveals `path` highlighted inside its *enclosing* folder
/// (tray: click on a per-file row). On Linux `xdg-open` cannot select, so
/// the enclosing directory is opened instead (same folder the user needs).
pub fn reveal_command(os: Os, path: &Path) -> OsCommand {
    match os {
        Os::MacOs => OsCommand { program: "open", flag: Some("-R"), args: vec![path.to_path_buf()] },
        Os::Linux => {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(path);
            OsCommand { program: "xdg-open", flag: None, args: vec![parent.to_path_buf()] }
        }
        Os::Windows => {
            // `explorer /select,<path>` — the comma belongs to the flag and
            // the path follows as its own argument.
            OsCommand { program: "explorer", flag: Some("/select,"), args: vec![path.to_path_buf()] }
        }
    }
}

/// Open `dir` in the platform file manager (fire-and-forget).
pub fn open_folder(dir: &Path) {
    open_folder_command(Os::current(), dir).spawn();
}

/// Reveal `path` in its enclosing folder (fire-and-forget).
pub fn reveal(path: &Path) {
    reveal_command(Os::current(), path).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_reveal_uses_open_dash_r_on_the_item_itself() {
        let cmd = reveal_command(Os::MacOs, Path::new("/designs/Client A/homepage.penpot"));
        assert_eq!(cmd.program, "open");
        assert_eq!(cmd.flag, Some("-R"));
        assert_eq!(cmd.args, vec![PathBuf::from("/designs/Client A/homepage.penpot")]);
    }

    #[test]
    fn macos_open_folder_has_no_flag() {
        let cmd = open_folder_command(Os::MacOs, Path::new("/designs"));
        assert_eq!(cmd.program, "open");
        assert_eq!(cmd.flag, None);
        assert_eq!(cmd.args, vec![PathBuf::from("/designs")]);
    }

    #[test]
    fn linux_reveal_falls_back_to_the_enclosing_dir() {
        let cmd = reveal_command(Os::Linux, Path::new("/designs/Client/home.penpot"));
        assert_eq!(cmd.program, "xdg-open");
        assert_eq!(cmd.flag, None);
        assert_eq!(cmd.args, vec![PathBuf::from("/designs/Client")]);
    }

    #[test]
    fn linux_reveal_of_a_rootless_path_does_not_panic() {
        let cmd = reveal_command(Os::Linux, Path::new("/"));
        assert_eq!(cmd.args, vec![PathBuf::from("/")]);
    }

    #[test]
    fn windows_reveal_uses_explorer_select() {
        let cmd = reveal_command(Os::Windows, Path::new(r"C:\designs\home.penpot"));
        assert_eq!(cmd.program, "explorer");
        assert_eq!(cmd.flag, Some("/select,"));
        assert_eq!(cmd.args, vec![PathBuf::from(r"C:\designs\home.penpot")]);
    }

    #[test]
    fn unicode_and_space_paths_survive_verbatim() {
        // Args go through Command::arg (no shell), so no quoting is needed —
        // the path must be passed through byte-identical.
        let p = Path::new("/designs/Progetti è 🎨/città.penpot");
        let cmd = reveal_command(Os::MacOs, p);
        assert_eq!(cmd.args, vec![p.to_path_buf()]);
    }
}
