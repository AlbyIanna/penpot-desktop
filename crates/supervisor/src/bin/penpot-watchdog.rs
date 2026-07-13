//! `penpot-watchdog` — SIGKILL orphan watchdog (see `supervisor::watchdog`
//! module docs for the full mechanism).
//!
//! Tiny, std-only at runtime: reads the line protocol from stdin (the pipe
//! whose write end lives in the app process). On `bye` it exits without
//! touching anything; on EOF (parent died for ANY reason, including SIGKILL)
//! it SIGTERMs the last-known child pids, waits the grace period, SIGKILLs
//! survivors, SIGKILLs anything whose command line contains the pgdata path,
//! and exits.

use std::io::BufRead;

use supervisor::watchdog::{parse_line, Line, WatchdogState};

fn main() {
    // Only `bye` or pipe EOF may terminate the watchdog: it runs in its own
    // process group (the parent spawns it that way) AND ignores the polite
    // signals, so a terminal Ctrl+C or a broad `kill` sweep cannot disarm it.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }

    let mut state = WatchdogState::default();
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        match parse_line(&line) {
            Some(Line::Bye) => {
                eprintln!("[penpot-watchdog] bye — clean shutdown, exiting without killing");
                return;
            }
            Some(update) => state.apply(update),
            None => {
                if !line.trim().is_empty() {
                    eprintln!("[penpot-watchdog] ignoring malformed line: {line:?}");
                }
            }
        }
    }

    // EOF without bye: the parent is gone (SIGKILL, panic, abort, …).
    #[cfg(unix)]
    supervisor::watchdog::run_kill_sequence(&state);
    #[cfg(not(unix))]
    eprintln!("[penpot-watchdog] parent died, but no unix kill support on this platform");
}
