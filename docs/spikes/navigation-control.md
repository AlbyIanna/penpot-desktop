# Spike: can the webview observe and redirect Penpot's hash navigation?

Answers the open question PLAN4 flags as chapter 4's ceiling-setter (`PLAN4.md`, risk 1):
Penpot routes on the URL **fragment**, which never reaches the server, so our usual
interception point (the proxy) is powerless there. Only the webview itself can see a
hash change. D0 exists to find out, before D2 (the home-as-front-door redirect) and D6
(residue cleanup across every web route) build on the answer, whether that's even
possible **without touching Penpot's SPA** — invariant 3 forbids injecting JS to watch
`location`.

**Verdict: GO** — a same-document hash navigation IS observed by Tauri's
`on_navigation` callback, with no SPA involvement required.

Everything below traces to `findings.json`, written by `scripts/d0-navigation-spike.sh`
(`just d0`, PLAN4 milestone D0). The controller ran the finished gate twice; both runs
were identical: exit code 0, `D0 NAVIGATION: ALL PASS (8 passed)`, 16 PASS / 0 FAIL
across the two runs combined, all four dedicated ports (proxy 9034, backend 6496,
postgres 5569, valkey 6512; control 9037 reserved) freed on exit.

## Method: three isolated probes, plus a read-only reality check

The mechanism question — "does `on_navigation` fire on a hash change" — has nothing to
do with Penpot. Answering it *on Penpot's own SPA* would mean either injecting an
observer into the SPA (forbidden by invariant 3) or trusting a fragile heuristic read
off Penpot's real router. Instead the gate isolates the question onto **our own page**:

1. **`/__navprobe`** (`apps/desktop/src/navprobe.{rs,html}`) — a page we serve, with
   three buttons/cases: `hash` (`location.hash = …`, the same mechanism Penpot's router
   uses), `pushstate` (`history.pushState`, a plausible fallback), and `full`
   (`location.assign`, a full document navigation used as the control). Because this is
   our page, not Penpot's, the SPA is never touched — invariant 3 is out of the picture
   entirely for this leg.
2. **`navwatch::decide()`** (`apps/desktop/src/navwatch.rs`) — the observer. It hooks
   Tauri 2.11.5's `WebviewWindowBuilder::on_navigation<F: Fn(&Url) -> bool>` on the
   window (now built in Rust rather than declared in `tauri.conf.json`, so the handler
   can attach), and optionally cancels + redirects. Both switches are env-gated off by
   default (`PENPOT_LOCAL_NAVWATCH_LOG`, `PENPOT_LOCAL_NAVWATCH_REDIRECT`) so merging
   this changes nothing in a normal run.
3. **`scripts/d0_navprobe.py`** — reads the JSONL the observer wrote and reports,
   per case, whether a navigation was actually observed.
4. **`scripts/d0_penpot_nav.cjs`** — a separate, strictly **read-only** DOM inspection
   of Penpot's real dashboard (`page.$$eval` reading anchor `href` attributes only, no
   mutation, no injection) run against the bundled offline Chromium. This leg answers a
   *different* question — "how does Penpot itself navigate, in reality?" — and is kept
   apart from the mechanism probe so a false reading in one can never contaminate the
   other.

## Results (from `findings.json`, identical both runs)

| Leg | Field | Value | What it means |
|---|---|---|---|
| (a) Control — full document nav | `fullObserved` | `true` | Baseline: `on_navigation` fires on an ordinary navigation. If this were `false`, the harness itself would be broken and nothing else would mean anything. |
| (b) Central — same-document hash change | `hashObserved` | `true` | **The central finding.** `location.hash = …` on our own page IS observed by `on_navigation`. |
| (c) Fallback — `history.pushState` | `pushstateObserved` | `false` | A pushState-based fallback produces no navigation event — confirms hash routing is the mechanism that matters here, not an alternative to fall back on. |
| (d) Reality check — does Penpot use anchor `href`s? | `usesAnchorHref` | `true` (`penpotNavRaw.anchors = [{"href": "/#/dashboard/recent"}]`) | Penpot's real dashboard links via `<a href="#/dashboard/...">`, which is exactly the case the `hash` probe measured. See caveat 2 below. |
| — | `probeRan` | `{full: true, hash: true, pushstate: true, redirect: true}` | Proof-of-life for every leg: each case's app actually booted and produced a baseline observation before its measurement is trusted. |
| (e) Redirect | `redirectWorks` | `true` | With `PENPOT_LOCAL_NAVWATCH_REDIRECT=1`, a navigation to `#/dashboard` was cancelled and the window was sent to `/__home` instead. |
| (f) Integrity | `vaultIntact` | `true` | The **on-disk vault tree** (seeded with a canary `.penpot` dir before any boot) was byte-identical before and after the redirect boot. This is a hash over files on disk, exercised from `/__navprobe` — no workspace was ever open during this gate. See caveat 3 below for exactly what was and was not measured. |
| — | `verdict` | `"GO (hash change IS observed by on_navigation)"` | Computed directly from `hashObserved`. |

### The captured URL trace (leg e, both runs — the single most important piece of evidence)

```json
["tauri://localhost", "http://localhost:9034/__navprobe?run=hash", "http://localhost:9034/__navprobe?run=hash#/dashboard", "http://localhost:9034/__home"]
```

Read left to right: the window starts at `tauri://localhost`, loads our probe page,
the probe page performs its `hash` case (appending `#/dashboard` to the current URL),
`on_navigation` sees that hash change, `navwatch::decide()` classifies the fragment as
a web route (`#/dashboard`, `#/settings`, `#/auth` all match), cancels it, and the
window lands on `/__home` — never reaching a real `#/dashboard` state.

## Mechanism

Tauri 2.11.5's `WebviewWindowBuilder::on_navigation<F: Fn(&Url) -> bool>` — returning
`false` from the closure **cancels** the pending navigation. `navwatch::decide(url,
redirect_enabled)` is a pure function (`apps/desktop/src/navwatch.rs`) that: if
redirect is disabled, always allows; otherwise splits the URL on `#`, and if the
fragment starts with `#/dashboard`, `#/settings`, or `#/auth`, returns
`CancelAndRedirect("/__home")`; everything else (workspace URLs, our own `/__*` pages)
is allowed through untouched. The window itself had to move from being declared in
`tauri.conf.json` to being built in Rust so this handler could be attached at all.

## GO/NO-GO caveats — read before building on this

These are not fine print; each one bounds what D2/D6 may claim.

1. **Engine scope.** The anchor reality-check (leg d, `usesAnchorHref`) runs in the
   bundled **Chromium** used by the offline-headless harness, not in WKWebView (the
   engine the real macOS app uses). That leg answers "how does Penpot navigate?" — it
   does **not** answer "what does the Tauri webview report navigation events as?" Only
   the Rust probe (legs a–c, e, via `on_navigation`) answers that question, and only
   the Rust probe ran inside the actual Tauri/WKWebView stack.
2. **`usesAnchorHref` false-negative risk.** `scripts/d0_penpot_nav.cjs` waits a fixed
   4 seconds after page load before scraping anchor elements. A slower-rendering SPA
   render would read as `usesAnchorHref: false` even if anchors exist — a false
   negative, not a false positive. It found the anchor here (`/#/dashboard/recent`),
   but the method could understate reality on a slower machine or a heavier Penpot
   build.
3. **What the integrity leg (f) actually proves — important.** The redirect exercised
   in leg (e) is pure in-webview navigation with no filesystem code path of its own; it
   never calls into the sync daemon. So leg (f) genuinely shows that *an ordinary boot
   plus reconcile that happens to include a mid-session redirect* leaves the vault
   byte-identical — it does **not** isolate the redirect as the only possible
   disk-toucher, because nothing in this gate exercises the redirect independently of a
   normal boot/reconcile cycle. Do not describe this as "the redirect was proven
   harmless to disk"; describe it as what was actually shown: a boot that includes a
   mid-session hash-route cancellation left the on-disk vault unchanged.

   **Also important — no workspace was open.** The redirect in leg (e) was triggered
   from `/__navprobe`, our own page, never from a live workspace session. `vaultIntact`
   is a hash over a hand-seeded canary `.penpot/` directory sitting on disk, taken
   before and after the redirect boot — it is a filesystem-level check, not a
   live-session check. PLAN4's D0 exit criterion asked for "evidence that redirecting
   mid-session leaves the workspace intact"; that is **not** what this gate measured,
   which is why the finding is named `vaultIntact` — it names the on-disk tree that was
   actually hashed, not a claim about a live workspace session.
   **D2 must re-assert this with a real file actually open** — a live workspace with
   unsaved state present when the redirect fires — before relying on this result for
   anything beyond "the redirect mechanism itself does not touch disk."
4. **GUI-session requirement.** `scripts/d0-navigation-spike.sh` launches the real
   Tauri GUI binary (`penpot-desktop`, not the headless binary) so `on_navigation` can
   attach to a real window. It requires a GUI session and cannot run headlessly in CI —
   this is why PLAN4 keeps D0 out of `just e2e` (see `docs/milestones/d0.md`).
5. **No pixel-level visual confirmation was obtainable in this environment.**
   `screencapture` captured a different macOS Space than the one the app window was on,
   and enumerating windows via System Events needs Accessibility permission this
   environment did not have granted. So the window's existence for this spike is proven
   **functionally**, not visually: a live run recorded
   `{"source":"on_navigation","url":"tauri://localhost"}` in the navwatch log, which can
   only be produced by a real window with a real webview attached. Consequence for D1:
   `scripts/shots.sh`'s native captures will need Screen Recording + Accessibility
   granted in whatever environment runs it; the web-surface captures (bundled offline
   Chromium) are unaffected by this gap.

## What this means for D2 and D6

The GO verdict sets the **ceiling**, not the guarantee, for both downstream milestones:

- **The ceiling is the high one.** Because `hashObserved: true` and `redirectWorks:
  true`, D2's `#/dashboard` → `/__home` redirect and D6's residue cleanup across
  `#/auth`, `#/settings`, `#/dashboard` can be made **genuinely unreachable** — every
  navigation to those fragments gets cancelled before it renders — not merely "not the
  default, one click away," which is the ceiling a NO-GO would have left.
- **D2** should wire `navwatch`'s redirect policy in for real (it is currently
  env-gated off) as part of making `/__home` the front door, reusing
  `navwatch::decide()` and `HOME_PATH` as-is; no new mechanism is needed.
- **Warning for D2 — `#/auth` is a loaded gun.** `WEB_ROUTE_PREFIXES` includes
  `#/auth` (for D6's residue cleanup), but this gate never tested that prefix
  through the `/__bootstrap` auto-login path — the redirect leg here only ever
  ran from `/__navprobe`. If auto-login transits an `#/auth/...` fragment on its
  way to a logged-in session, enabling this redirect policy could cancel login
  itself. **D2 must verify the bootstrap login path before enabling `#/auth`
  redirection**, not assume it is safe because `#/dashboard` and `#/settings`
  measured clean.
- **D6** can apply the same policy to `#/auth` and `#/settings` (already included in
  `WEB_ROUTE_PREFIXES`) and assert, per caveat 1 above, that the assertion is made
  against the Rust-level `on_navigation` observation, not the Chromium anchor scrape.
- **In-canvas web affordances remain a separate, unresolved surface.** This spike only
  covers **navigation** (moving between routes). Affordances that live *inside* the
  workspace canvas — the profile/avatar menu, share, comments, subscription nags — are
  not routes the webview navigates away from; a GO here does not extend to hiding them
  (that would require patching the SPA, which the chapter refuses per invariant 3, per
  `PLAN4.md` risk 3).
