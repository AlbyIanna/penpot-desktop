# D6 — Residue Audit, Honest Docs, Packaged Proof: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Account for **every** reachable web route, write the honest known-limits document, and prove the *packaged* app boots into the native home, offline, with no dashboard reachable — closing chapter 4.

**Architecture:** This is an audit, not a feature. It (1) closes the one navigation gap the survey found — reachable `#/debug/*` routes — and pins the full route table with tests; (2) writes `docs/known-limits.md` cataloguing exactly what stays web-shaped inside the canvas and why invariant 3 keeps it; (3) extends `scripts/m4-artifact-test.sh` to prove the packaged dmg boots into `/__home` offline; (4) adds `scripts/d6-residue.sh` tying the route accounting together; (5) updates the top-level README with a chapter-4 section.

**Tech Stack:** Rust (navwatch), bash + the existing `PENPOT_LOCAL_NAVWATCH_LOG` mechanism, the packaged `.app` / dmg build, Markdown + Mermaid.

## Global Constraints

- **Core invariant (P0):** delete the DB, restart, everything rebuilds from the folder tree.
- **Zero cross-vault spill (P0)** and **offline (P0, zero non-loopback):** unchanged; the packaged leg re-proves offline.
- **Invariant 3 is the CEILING, not a bug:** in-canvas affordances (share, comments, upgrade nag, help/feedback) live inside the SPA and CANNOT be removed without patching the frontend, which this chapter refuses. The docs say "the app never takes you there", never "Penpot's account UI has been removed."
- **Decisions already made:** `#/view` is KEPT (present mode; our Present buttons use it; share-links inert offline) — do NOT redirect it. "Updates" is DEFERRED to a later chapter — do NOT add an updater.
- **The route accounting must be complete:** every top-level hash route in the shipped bundle is classified as redirected / product / internal-utility-allowed / dev-inert-cancelled, with a test.
- **D6 dedicated ports** for its own gate: proxy 9058, backend 6520, postgres 5593, valkey 6536. The packaged leg reuses `m4-artifact-test.sh`'s M4 ports (8906/6381/5455/6398/6468).
- Commits end with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`; never a bare `#<number>`.
- `just d6` chained into `just e2e`; green twice. (The packaged-artifact leg is GUI/macOS + heavy build, like M4 — it stays in `m4` / `just m4`, not the headless `e2e` chain; `d6-residue`'s pure-and-headless parts chain.)

## The route table (ground truth from the shipped bundle — the audit's checklist)

| Route family | Reachable offline | Verdict |
|---|---|---|
| `#/auth/*` | yes | **cancelled** (navwatch, done) |
| `#/dashboard/*` | yes | **cancelled** (navwatch, done) |
| `#/settings/*` | yes | **cancelled** (navwatch, done) |
| `#/workspace/*` | yes | **allowed** — the product |
| `#/view/*` | yes | **allowed, kept deliberately** — present mode; our UI uses it |
| `#/frame-preview`, `#/render-sprite/:id` | yes | **allowed** — internal render utilities the SPA navigates to itself; cancelling risks breaking rendering (same lesson as `#/view`) |
| `#/debug/icons-preview`, `#/debug/playground` | yes | **cancel** — inert dev tooling, the app never navigates there; no account/network implication; closing them makes "no dev surface is reachable" a clean claim |
| `#/subscribe-nitrate` | N/A | **absent from our build** (flag not set) — record, no action |

---

### Task 1: Close the route-accounting gap in navwatch

**Files:** Modify `apps/desktop/src/navwatch.rs`.

**Interfaces:**
- `ALWAYS_CANCELLED_PREFIXES` gains `"#/debug"` (→ 4 entries).
- Its doc comment gains the full route-accounting rationale: which families are cancelled, which allowed and WHY (`#/view` kept; `#/frame-preview`/`#/render-sprite` internal-use; `#/workspace` product).

**Why `#/debug` and not the others:** the survey found `*assert*` is not elided from the release build, so `#/debug/icons-preview` and `#/debug/playground` are compiled in and reachable. They are dev tooling with no account/cloud/network implication, and the app never navigates to them — so cancelling is risk-free and closes the accounting. `#/frame-preview` and `#/render-sprite` are internal render utilities the SPA DOES use, so cancelling them could break rendering (the `#/view` lesson) — they stay allowed and are documented.

- [ ] **Step 1: Write the failing tests** — extend `navwatch::tests`:

```rust
    #[test]
    fn debug_routes_are_cancelled() {
        // The shipped bundle does not elide *assert*, so #/debug/* is reachable
        // by URL. It is inert dev tooling with no account/network implication,
        // and the app never navigates there — so we close it.
        for url in [
            "http://localhost:9058/#/debug/icons-preview",
            "http://localhost:9058/#/debug/playground",
        ] {
            match decide(url, false) {
                Decision::CancelAndRedirect(to) => assert!(to.ends_with(HOME_PATH), "{url} -> {to}"),
                other => panic!("{url} not cancelled: {other:?}"),
            }
        }
    }

    #[test]
    fn internal_render_routes_and_the_viewer_stay_allowed() {
        // These the SPA navigates to ITSELF (present mode; frame/sprite render
        // utilities). Cancelling would break rendering — the #/view lesson. The
        // audit KEEPS them, deliberately, and pins that so a future tidy-up
        // can't silently cancel them.
        for url in [
            "http://localhost:9058/#/view?file-id=x&page-id=y",
            "http://localhost:9058/#/frame-preview",
            "http://localhost:9058/#/render-sprite/abc",
            "http://localhost:9058/#/workspace?team-id=t&file-id=f",
        ] {
            assert!(matches!(decide(url, false), Decision::Allow), "{url} should be allowed");
        }
    }

    #[test]
    fn debug_prefix_boundary_holds() {
        // "#/debugger" (hypothetical) must NOT match "#/debug".
        assert!(matches!(decide("http://localhost:9058/#/debugxyz", false), Decision::Allow));
    }
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p penpot-desktop navwatch::` (the first fails: `#/debug` currently allowed).
- [ ] **Step 3: Implement** — add `"#/debug"` to `ALWAYS_CANCELLED_PREFIXES`; rewrite the module doc's route-accounting section to name all eight families and their verdicts.
- [ ] **Step 4: Run tests.** Also re-run the D1/D2 navwatch legs mentally: no existing allowed route regresses.
- [ ] **Step 5: Commit.**

---

### Task 2: The known-limits document

**Files:** Create `docs/known-limits.md`.

This is the honest catalogue PLAN4 demands. It must say "the app never navigates you there", never "Penpot's account UI was removed". Structure:

- [ ] **Section: What the app removed or closed.** The config-removed surfaces (registration link, login password fields, dashboard-templates, google-fonts, onboarding, email-verification) with the mechanism (flags). The redirected routes (`#/auth`, `#/dashboard`, `#/settings`, `#/debug`) with the mechanism (navwatch, unconditional). Link each to the milestone that did it (D1, D2, D4, D6).

- [ ] **Section: What stays web-shaped inside the canvas — and why invariant 3 keeps it.** The audit's core honesty. Enumerate, with the survey's evidence: the workspace **Share/publish** control (inert offline — no server for a share link to reach), **comments** (in-canvas, usable as notes-to-self), the **subscription "Power up"** menu item and version-history subscription warnings, **help-center / feedback** links in the file menu, and the viewer's **go-to-dashboard** icon (visible but neutralised — it fires `#/dashboard`, which navwatch redirects to `/__home`). For each: it lives inside the SPA bundle; invariant 3 forbids patching it out; the only lever is not-navigating-there (already applied where it leads out of the canvas). State plainly that **no avatar/profile menu and no invite affordance were found reachable from inside `#/workspace`** — they live on `#/dashboard`, which is redirected away.

- [ ] **Section: What is kept deliberately.** `#/view` (present mode) and `#/frame-preview`/`#/render-sprite` (internal render utilities) — reachable, invariant-safe, and why cancelling them would break real behaviour.

- [ ] **Section: Deferred threads (named, not buried).** "Updates" — the app never checks for updates (never phones home); releases are manual downloads; deferred to a later chapter by decision. D1's egress caveat — the D1 gate's socket check was a single `lsof` sample; D6's packaged leg strengthens it with the poisoned-proxy `env -i` harness (Task 3), which is why that caveat is now discharged.

- [ ] **Section: Testing honesty.** Native chrome (menu bar, dialogs, window titles, Finder) is not CI-testable — captured manually per PLAN4; the gates test the command layer beneath. Finder double-click needs a signed installed app (D5a). Say this plainly.

- [ ] Commit.

---

### Task 3: Extend the packaged-artifact test — boots into `/__home`, offline

**Files:** Modify `scripts/m4-artifact-test.sh`.

The survey confirmed `m4-artifact-test.sh` already boots the packaged `.app` binary under `env -i` with **poisoned proxies** (a real offline harness — stronger than D1's single sample) and polls the proxy for readiness. It does NOT currently assert anything about which URL the webview lands on. Add that.

- [ ] **Step 1:** Pass `PENPOT_LOCAL_NAVWATCH_LOG=<path>` through the packaged app's `env -i` invocation (it is not in the passed env list today). This is the D0/D2 mechanism: the app appends every `{source,url}` navigation observation as JSONL.

- [ ] **Step 2: Assert the packaged app boots into the native home, not the dashboard.** After readiness, read the navwatch log and assert: the first real navigation the webview made resolves to `/__home` (via `/__bootstrap` → `/__home`), and NO observation is a `#/dashboard`/`#/auth`/`#/settings` fragment. This is the "no dashboard reachable by default" proof — and because the fragment never reaches the server (only the webview sees it), the navwatch log is the ONLY valid evidence; a `curl` check cannot prove it. Prove-you-were-looking: fail loudly if the log is empty/absent (an infra failure, not a pass).

- [ ] **Step 3: Strengthen the offline evidence (discharges D1's caveat).** The `env -i` + poisoned-proxy harness already means a non-loopback connection cannot succeed. Additionally sample `lsof -nP -i` over the packaged stack pids (the script's `stack_pids()` already exists) and assert zero non-loopback peers — reusing `d1_egress.py`'s loopback predicate/parser, not a new one. Reference this in the milestone doc as the promised D6 strengthening of D1's single-sample caveat.

- [ ] **Step 4:** Keep the leg guarded like the rest of M4 (GUI/macOS, run-solo, not headless CI). Verify with `bash -n`; do NOT run the full build here — the controller runs `just m4` live.
- [ ] **Step 5: Commit.**

---

### Task 4: The residue gate

**Files:** Create `scripts/d6-residue.sh`; modify `justfile`.

**Model on `scripts/d1-offline.sh`**: header block, `pass`/`fail`, totals, non-zero exit, SKIPPED distinct from PASS. D6 ports 9058/6520/5593/6536.

- [ ] **(a) Route accounting is complete and pinned.** Run `cargo test -p penpot-desktop navwatch::` and REQUIRE by name the specific tests that pin each verdict: `debug_routes_are_cancelled`, `internal_render_routes_and_the_viewer_stay_allowed`, `dashboard_is_cancelled_even_with_redirect_disabled`, `settings_is_present_and_cancelled_in_d4`, `auth_family_is_cancelled_even_with_redirect_disabled`, `workspace_and_our_surfaces_are_never_redirected` (the D1/D2 precedent: "the suite passed" is not enough). This asserts every web route in the table has a verdict and it's enforced.

- [ ] **(b) A live redirect actually lands on `/__home`.** Reuse the D0/D2 GUI harness (`PENPOT_LOCAL_NAVWATCH_LOG` + `PENPOT_LOCAL_START_URL`) to drive a `#/dashboard` and a `#/debug/playground` navigation in a real webview and assert each lands on `/__home`. Mark this leg GUI-session-required and SKIP-with-reason if no GUI (the D0/D2 precedent), never a silent pass.

- [ ] **(c) The known-limits doc exists and is honest.** Assert `docs/known-limits.md` exists and contains the required sections (removed, in-canvas ceiling, kept-deliberately, deferred threads). A grep-level structural check — cheap, and it stops the doc silently disappearing or being gutted.

- [ ] **(d) The packaged proof is referenced, not duplicated.** Assert the extended `m4-artifact-test.sh` leg exists (grep for the navwatch-log assertion) so `just d6` and `just m4` don't drift; the actual packaged run stays in `just m4`.

- [ ] Chain `d6` into `just e2e`; verify (`bash -n`, `just --list`); commit.

---

### Task 5: The chapter close — README + milestone doc

**Files:** Modify top-level `README.md`; create `docs/milestones/d6/README.md`.

- [ ] **Step 1: `docs/milestones/d6/README.md`** — following the shape of d1..d5. What the audit found (the reachable-debug-route gap, now closed), the route table with verdicts, a Mermaid diagram of the navigation-verdict classification, and **known limits pointing to `docs/known-limits.md`**. Native screenshots are MANUAL (menu bar, Preferences, a file window) per PLAN4 — capture them on a real GUI session; **do not use `screencapture` from an automated session** (it grabs the whole display). If no native captures are taken here, say so explicitly rather than implying they exist.

- [ ] **Step 2: Top-level `README.md`** — the survey found its status table stops at M5 (chapter 1) and never mentions chapters 2–4. Add a **chapter-4 section** summarising "the desktop app" (native home, menu bar, Preferences, documents, offline-proven) with links to each `docs/milestones/d<N>/README.md` and `docs/known-limits.md`. Do not re-narrate the milestones — link them.

- [ ] **Step 3: Commit.**

---

## Self-Review

**1. Spec coverage.** PLAN4's D6 asks for: apply the redirect verdict across every reachable web route ✅ (Task 1 — closes the `#/debug` gap the survey found; the account routes were already done in D1/D2/D4; `#/view` kept by decision); `docs/known-limits.md` naming what survives in-canvas and why invariant 3 keeps it ✅ (Task 2); an extended `m4-artifact-test` leg proving the packaged dmg boots into the native home offline with no dashboard ✅ (Task 3); README updated ✅ (Task 5); the `d6-residue.sh` gate ✅ (Task 4); green twice ✅.

**2. Placeholders.** None. Task 3 and Task 4(b) name the exact env-var mechanism and precedent scripts.

**3. Type consistency.** `ALWAYS_CANCELLED_PREFIXES` (Task 1) is what Task 4(a)'s tests pin. The `PENPOT_LOCAL_NAVWATCH_LOG` mechanism (Task 3, Task 4b) is the same one D0/D2 use. `docs/known-limits.md` (Task 2) is what Task 4(c) checks and Task 5 links.

**Deliberate scope decisions, recorded:** `#/frame-preview` and `#/render-sprite` stay ALLOWED (internal render utilities — cancelling risks breaking rendering, the `#/view` lesson); `#/view` stays allowed (decided); "updates" is out (decided). `#/debug` is cancelled because it's inert and the app never uses it — a technical audit call, justified in Task 1's doc comment and the milestone doc.

**Known risk carried into execution:** the extended M4 leg and `d6-residue.sh`'s live leg both need a real GUI session on macOS and a packaged build — not headless CI. That is the standing native-chrome ceiling (PLAN4 risk 2), not a D6 regression; both must SKIP-with-reason rather than fake a pass when the environment can't run them, and the milestone doc must say which legs are GUI-only.
