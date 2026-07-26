//! Compatibility shim (chapter C): `penpot daemon` under the old name.
//!
//! Every milestone gate launches `--bin headless` and waits for `READY <url>`
//! on stdout. Those suites are the net that protected the whole chapter-C
//! extraction, so the name outlives the refactor. Removing this shim is a
//! one-line change in a later chapter.
//!
//! The body moved verbatim to [`penpot_cli::verbs::daemon`] — the module doc
//! there records the whole contract this binary is on the hook for (`READY`,
//! the stdout flush, SIGUSR1, `remove_record`, `STOPPED`).

fn main() -> anyhow::Result<()> {
    penpot_cli::verbs::daemon::run()
}
