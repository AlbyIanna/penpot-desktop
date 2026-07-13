//! `watchdog-fake-parent` — test harness for the orphan-watchdog integration
//! tests (`tests/watchdog_integration.rs`). NOT part of the product.
//!
//! Plays the app's role without the penpot stack: spawns dummy `sleep`
//! children in their own process groups (proving the watchdog kills by pid,
//! not by process group), optionally a "fake postmaster" whose command line
//! contains a pgdata path but whose pid is deliberately NOT fed to the
//! watchdog (exercising the ps-cmdline fallback), spawns the real watchdog,
//! feeds it the protocol, writes all pids to an outfile, then either:
//!
//! - mode `kill`: sleeps forever — the test SIGKILLs this process, which is
//!   the parent-death (pipe EOF) trigger;
//! - mode `bye`: sends `bye` and exits cleanly — the watchdog must exit
//!   without killing the still-running dummies.
//!
//! Usage: `watchdog-fake-parent <watchdog-bin> <outfile> <kill|bye> [pgdata-dir]`

#[cfg(unix)]
fn main() {
    unix_main::run();
}

#[cfg(not(unix))]
fn main() {
    eprintln!("watchdog-fake-parent is unix-only");
    std::process::exit(1);
}

#[cfg(unix)]
mod unix_main {
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;

    use supervisor::watchdog::WatchdogHandle;

    const GRACE: Duration = Duration::from_millis(2000);

    fn spawn_sleep() -> Child {
        Command::new("/bin/sleep")
            .arg("300")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Own process group: the watchdog must kill it by PID.
            .process_group(0)
            .spawn()
            .expect("spawn sleep dummy")
    }

    /// A long-running process whose COMMAND LINE contains the pgdata path
    /// (like a detached postmaster's `-D <pgdata>`): `bash <pgdata>/fake-postmaster.sh`.
    /// The script body is a loop so bash cannot tail-exec it away.
    fn spawn_pgdata_dummy(pgdata: &Path) -> Child {
        std::fs::create_dir_all(pgdata).expect("create pgdata dir");
        let script = pgdata.join("fake-postmaster.sh");
        std::fs::write(&script, "while true; do sleep 1; done\n").expect("write script");
        Command::new("/bin/bash")
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn pgdata dummy")
    }

    pub fn run() {
        let args: Vec<String> = std::env::args().collect();
        if args.len() < 4 {
            eprintln!("usage: {} <watchdog-bin> <outfile> <kill|bye> [pgdata-dir]", args[0]);
            std::process::exit(2);
        }
        let watchdog_bin = PathBuf::from(&args[1]);
        let outfile = PathBuf::from(&args[2]);
        let mode = args[3].as_str();
        let pgdata = args.get(4).map(PathBuf::from);

        let s1 = spawn_sleep();
        let s2 = spawn_sleep();
        let pg = pgdata.as_deref().map(spawn_pgdata_dummy);

        let mut watchdog = WatchdogHandle::spawn(&watchdog_bin, GRACE, pgdata.as_deref())
            .expect("spawn watchdog");
        // NOTE: the pgdata dummy's pid is deliberately NOT in this set — it
        // must die via the ps-cmdline fallback, not pid tracking.
        watchdog.send_pids(&[s1.id(), s2.id()]).expect("send pids");

        // Atomic outfile write (tmp + rename) so the test never reads a torn file.
        let tmp = outfile.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp).expect("create outfile");
            writeln!(f, "parent {}", std::process::id()).unwrap();
            writeln!(f, "watchdog {}", watchdog.pid()).unwrap();
            writeln!(f, "s1 {}", s1.id()).unwrap();
            writeln!(f, "s2 {}", s2.id()).unwrap();
            if let Some(pg) = &pg {
                writeln!(f, "pg {}", pg.id()).unwrap();
            }
        }
        std::fs::rename(&tmp, &outfile).expect("rename outfile");

        match mode {
            "bye" => {
                // Clean-shutdown path: in the real app the children are
                // stopped BEFORE bye; here we leave them running to prove
                // the watchdog does not touch them on bye.
                std::thread::sleep(Duration::from_millis(300));
                watchdog.bye(Duration::from_secs(5));
                // Exit without killing s1/s2 — the test asserts they survive
                // and cleans them up itself.
            }
            "kill" => {
                // Wait to be SIGKILLed by the test.
                std::thread::sleep(Duration::from_secs(600));
            }
            other => {
                eprintln!("unknown mode {other:?}");
                std::process::exit(2);
            }
        }
    }
}
