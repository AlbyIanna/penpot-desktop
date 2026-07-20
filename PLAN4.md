# Penpot Local — chapter 4: THE DESKTOP APP

Chapter 1 (PLAN.md, M0–M5) built the engine: a supervised embedded stack, a custom proxy, two-way
sync between a disposable DB and a folder tree of git-diffable `.penpot` dirs, conflict rules, an
offline dmg. Chapter 2 (PLAN2.md, N1–N6) made the folder tree the front door *of our own surfaces*:
an offline FTS5 index, a lighttable, a Cmd+K palette, checkpoints, plural vaults, a template gallery.
Chapter 3 (PLAN3.md, E1–E7) built the package ecosystem and settled its three hardest questions with
live spikes.

All of that is plumbing. **Chapter 4 is about what the user actually sees.** Today the app
auto-logs-in and drops you into Penpot's *web dashboard* — a surface built for logged-in, multi-user,
cloud collaboration. Our own surfaces are secondary pages. The result still feels like a web app in a
window: login machinery, a dashboard, teams, invitations, account settings, onboarding, subscription
UI, feedback forms — none of which mean anything when there is no server and no account.

Chapter 4 removes the web/logged-in experience from the user's path and replaces it with a native
desktop one.

## Vision

> You open Penpot Local and see **your files** — not a login, not a dashboard, not a team picker.
> The menu bar is a real menu bar. ⌘O opens a file. ⌘, opens Preferences, not a web settings page.
> Double-clicking a `.penpot` folder in Finder opens it. Nothing asks who you are, nothing phones
> home, nothing offers you a subscription. It is your machine, your folder, your design tool.

**North-star story:** a designer launches the app offline on a plane. It opens on their lighttable
with their recent work. They hit ⌘N, draw, close the window; the folder on disk is updated. They
never see an account, a team, or a network error — because nothing ever tried to reach the network.

## Invariants

Chapters 1–3 invariants remain sacred and are still the test suites:

> **Core (P0):** delete the entire database, restart, and every project/file is rebuilt from the
> folder tree with no data loss. The folder tree is the source of truth; the DB is a disposable cache.

> **Zero cross-vault spill (P0):** switching vaults never surfaces, imports, or writes a file from
> another vault.

> **The SPA stays byte-untouched (invariant 3):** no serve-time patching of upstream JS/CSS, no
> injected scripts, no plugin as OUR integration mechanism; **only URLs reach the canvas.**

Invariant 3 is *load-bearing for this chapter*, not an obstacle to route around. It is what keeps a
Penpot version bump cheap, and it is the reason chapter 4 works by **configuration, navigation, and
native chrome** rather than by forking the frontend. It also caps what is achievable — see risk 3.

New chapter-4 invariants:

> **Offline is a test, not a claim (P0):** in a normal session — boot, create, edit, export — the app
> makes **zero non-loopback network connection attempts**. Traffic to its own supervised stack on
> `localhost`/`127.0.0.1` is the whole architecture and obviously permitted; what must never happen is
> a connection leaving the machine. Proven by a gate, not asserted in a README.

> **The dashboard is not the front door:** the app boots into our own home surface and opens files
> directly into `/#/workspace/…`. `/dashboard`, `/settings` and `/auth` are never navigated to by us.

## How it works — three layers

| Layer | Mechanism | Invariant-3 status |
|---|---|---|
| **Configuration** | Penpot's OWN flags delete cloud surfaces (`disable-registration`, dashboard templates section, `google-fonts-provider`, plus the existing `disable-onboarding` / `disable-email-verification`) | Config, not patching — clean |
| **Front door** | Boot to `/__home`; open files straight to `/#/workspace/…`; (D0-permitting) redirect `#/dashboard` → home, `#/settings` → native Preferences | "Only URLs reach the canvas" — clean |
| **Native chrome** | Menu bar, shortcuts, Open Recent, native dialogs, Preferences window, window-per-file, Finder integration, drag-and-drop | Outside the webview entirely — clean |

**What flags can and cannot do (verified against the shipped 2.16.2 bundle).** The SPA understands
`enable-registration`, `enable-login-with-password`, `enable-onboarding`, `enable-email-verification`,
`enable-dashboard-templates-section`, `enable-google-fonts-provider`. It has **no flag** for the
dashboard itself, teams/members/invitations, `/settings`, subscription, feedback, or share/view. And
because Penpot routes on the URL **hash**, the proxy cannot block those routes — the fragment never
reaches the server. Only the webview can. Hence D0.

## Milestones

Every milestone lands one `scripts/d<N>-*.sh` gate (run-twice idempotent, dedicated ports) and one
`docs/milestones/d<N>.md`. Spikes additionally write a verdict doc.

**Chaining follows the chapter-3 precedent:** a milestone that lands **product code** is chained into
`just e2e` (as E1–E4 and E7 were); a **pure-verdict spike** that lands no product code is not (as E5
and E6 were) — the ladder has nothing of its to regress. So D1–D6 chain; D0 does not.

### D0 — SPIKE: navigation control (gates the chapter's ambition) — ✅ DONE (verdict GO)
Shipped in the D0 navigation-spike PR. `on_navigation` does observe hash-only changes, a
`#/dashboard` navigation was cancelled and landed on `/__home`, and the vault tree was
byte-identical across the redirect. Caveat carried forward: that integrity check ran with **no
workspace open** (it hashed a seeded canary), so D2 must re-assert it with a real file open.

Goal: determine whether the webview can observe and redirect SPA **hash** navigation without touching
the SPA. Probe Tauri v2 navigation events on hash-only changes and the fallbacks (URL polling, custom
protocol). Hard constraint: the observation mechanism must itself be invariant-clean — injecting JS to
watch `location` is NOT allowed. Also probe whether a mid-session redirect corrupts workspace state.
**Depends on:** none. **Exit:** `scripts/d0-navigation-spike.sh` + `docs/spikes/navigation-control.md`
with a GO/NO-GO. On GO: a live capture showing a `#/dashboard` navigation landing on `/__home`
instead, and evidence that redirecting mid-session leaves the workspace intact. On NO-GO: the ceiling
is documented honestly ("not the default, one click away") and D6 scopes down accordingly. Green twice.

### D1 — Offline & config hardening (+ the "before" baseline) — ✅ DONE
Shipped in the D1 offline-hardening PR; write-up in `docs/milestones/d1/README.md`. Gate
`just d1` green three times (17/0), chained into `just e2e`. Negative control run: re-enabling
the flags flips every surface verdict `gone → present`, so the gate demonstrably can fail.

**Two deviations from the exit criteria below, deliberate and recorded:**

1. **No `env -i` + poisoned-proxy harness.** Egress is measured by two observers instead — the
   SPA's own Chromium request log and an `lsof -nP -i` sample of the supervised process tree,
   with a loopback predicate hardened against prefix-match bypasses (`127.0.0.1.evil.com`).
   This is weaker in one specific way, stated in the gate's output: the socket check is a
   single sample, not a proof of absence. A connection that opens and closes between polls
   could be missed. A poisoned-proxy harness would catch attempts rather than established
   connections; worth revisiting in D6's residue audit.
2. **"The corresponding surfaces are absent" is not true for all four flags.** The registration
   route and the account settings page survive. Registration cannot be closed backend-side —
   our own provisioning calls that RPC on every DB wipe, the core invariant — so it is closed
   by the navigation policy instead, and `#/auth/*` is now cancelled unconditionally (D0 had
   shipped that policy dormant). Settings has no flag at all; it is D2's problem.
   `disable-google-fonts-provider` is served but **not** behaviourally verified, because its
   surface is the workspace font picker and this gate never opens a workspace.

Goal: set every Penpot flag that deletes a cloud surface, then make the offline promise a test. Audit
`login-with-password` against the `/__bootstrap` auto-login path before disabling it. Build
`scripts/shots.sh` (below) and capture the **before** baseline — today's launch-into-dashboard,
settings, onboarding and subscription surfaces — while they still exist.
**Depends on:** none. **Exit:** `scripts/d1-offline.sh` asserts (a) each flag is served in `config.js`
AND actually took effect in the SPA (not merely that we set it), (b) the corresponding surfaces are
absent, (c) **zero outbound network connection attempts** across a full session (boot → create → edit
→ export) under the `env -i` + poisoned-proxy harness plus a connection observer that distinguishes
loopback (permitted) from anything leaving the machine (forbidden). Baseline screenshots committed.
Green twice.

### D2 — The home becomes the front door — ✅ DONE
Goal: boot goes `/__bootstrap` → `/__home`, never `/`. The lighttable graduates from a read-only
surface into what the dashboard used to be for a single user: create file, create project, rename,
duplicate, move, delete — reusing the RPCs M5 already shipped (`rename-file`, `move-files`,
`rename-project`). Opening a file navigates to `/#/workspace/…`. If D0 landed GO, the
`#/dashboard` → `/__home` redirect lands here.
**Depends on:** D0 (for the redirect half), D1 (baseline). **Exit:** `scripts/d2-home.sh` drives a
full file lifecycle (new project → new file → rename → move → open in workspace → delete) entirely
through our surfaces, asserts `/dashboard` is **never loaded** in the session, and asserts the vault on
disk reflects every operation (folder-is-truth holds through the new verbs). Green twice.

### D3 — Native menu bar, shortcuts, Open Recent — ✅ DONE
Goal: a real menu bar wired to real commands. **File** (New File, New Project, Open…, Open Recent,
Open Vault…, Import…, Export…, Reveal in Finder), **Edit** (delegating to the webview), **View** (Home,
Search, Palette, Packages, Templates), **Window**, **Help** (About, Known Limits). Desktop
accelerators (⌘N, ⌘O, ⌘F, ⌘,).
**Depends on:** D2. **Exit:** `scripts/d3-menus.sh` asserts the menu model is constructed with the
expected items and accelerators, and that **every menu action's underlying command works headlessly**
— menus cannot be clicked in CI, so the command layer is tested directly and the menu wiring is
asserted to map onto those commands (no orphaned or dead menu items). Green twice.

### D4 — Native Preferences + native dialogs — ✅ DONE
Goal: a native Preferences window standing in for `/settings`: vault location and switching (through
the proven N5 path), sync on/off + status, thumbnails/exporter toggle, plugin + CSP toggles,
about/updates. Native open/save dialogs for import/export.
**Depends on:** D3. **Exit:** `scripts/d4-preferences.sh` asserts preferences persist across a restart
and **actually take effect** (e.g. toggling the exporter stops renders being produced), and that a
vault switch initiated from Preferences goes through the N5 zero-spill machinery (re-asserting no
cross-vault spill). Green twice.

### D5 — OS integration: documents, windows, drag-and-drop
Goal: behave like a document-based app. Open a `.penpot` folder from Finder (file association / CLI
argument / URL scheme); one window per file with the **filename in the window title**; multi-window;
drag a `.penpot` folder onto the app to open it. Must cooperate with the M5 single-instance guard: a
second launch **forwards the document** to the running app rather than booting a second stack.
**Depends on:** D3. **Exit:** `scripts/d5-documents.sh` launches the binary with a path argument and
asserts that file opens; asserts the window title tracks the open file; asserts a second launch
forwards instead of double-booting (no second supervised stack, ports not doubled). Green twice.

### D6 — Residue audit, honest docs, packaged proof (chapter closer)
Goal: apply D0's verdict across every reachable web route (`#/auth`, `#/settings`, `#/dashboard` →
native equivalents), write the **known-limits** document, and close on the packaged artifact.
**Depends on:** D0–D5. **Exit:** `scripts/d6-residue.sh` asserts each web route redirects to its native
equivalent (GO) or records the documented ceiling (NO-GO); `docs/known-limits.md` names exactly which
web/account affordances survive inside the canvas and why invariant 3 keeps them; an extended
`m4-artifact-test` leg proves the **packaged dmg** boots into the native home, offline, with no
dashboard reachable by default. README updated with the new native screenshots. Green twice.

## Definition of done — EVERY milestone

In addition to its gate, each milestone lands `docs/milestones/d<N>.md` containing:

1. **What changed** — a plain-language narrative, not a changelog.
2. **How it works** — with a **Mermaid** diagram where flow or architecture helps. Mermaid renders
   natively on GitHub and is plain text, so it diffs properly; a binary image of a diagram does not.
3. **Before / after visuals** — paired screenshots.
4. **Known limits** — what is still web-shaped here, and why.

### Screenshots — what is automated and what is not

- **Automated (web surfaces).** `scripts/shots.sh <milestone>` boots a stack on dedicated ports, seeds
  a small demo vault (so shots are not empty voids), drives the **bundled offline Chromium** (the one
  the routes-gate and E7's activation leg already use) over a named list of surfaces at a fixed
  1280px-wide viewport, and writes to `docs/milestones/d<N>/img/`. Fixed viewport + seeded fixture ⇒
  re-runnable and comparable, which is what makes honest before/after possible.
- **Manual (native chrome).** The menu bar, Preferences window, native dialogs, window titles and
  Finder integration are OS-level and outside any browser; they are captured on a real GUI session via
  `screencapture`, guided by a per-milestone checklist in the milestone doc. These change rarely.
  **This is not CI-reproducible and the docs must not imply otherwise.**
- **Hygiene.** Fixed viewport, PNGs optimized before commit. A few images per milestone is fine;
  unbounded 4K screenshots would bloat the repo.

## Known risks (read before coding)

1. **Hash routing means only the webview can control routes.** Penpot routes on the URL fragment,
   which never reaches the server — so the proxy (our usual interception point, as with the CSP header)
   is powerless here. Everything in the "front door" layer depends on webview-level navigation control,
   whose feasibility is genuinely unknown. D0 exists to answer it before D2/D6 build on it. A NO-GO
   does not sink the chapter; it lowers the ceiling from "unreachable" to "not the default".
2. **Native chrome is not CI-testable.** Menus, dialogs and OS windows cannot be clicked headlessly.
   The gates therefore test the **command layer beneath** the menu and assert the wiring maps onto it —
   which catches dead menu items but not "the menu didn't render". Accept the gap explicitly rather
   than fake a test that proves nothing.
3. **Invariant 3 caps how much web experience can be removed — over-promising is the trap.** In-canvas
   affordances (back-to-dashboard, share, comments, the avatar/profile menu, subscription nags) live
   inside the SPA. We can stop *navigating* there and we can redirect *if D0 says GO*, but we cannot
   delete them without patching the frontend, which this chapter deliberately refuses. Documentation
   must say "the app never takes you there", never "Penpot's account UI has been removed".
4. **Flags are upstream-owned and may drift.** Flag names can be renamed or removed across Penpot
   versions, and setting a flag is not evidence it worked. The D1 gate must assert each flag **took
   effect in the SPA**, not merely that we passed it — otherwise a silent upstream rename degrades the
   experience with every gate still green.
5. **Single-instance vs. multi-window.** M5 shipped a single-instance guard so a second launch cannot
   boot a second supervised stack. D5's document-open path must *forward* to the running instance. Get
   this wrong and either documents fail to open or a second stack fights the first over ports and the DB.
6. **D2 is the scope risk.** "Make the lighttable do what the dashboard did" is a real product surface
   (create/rename/move/delete/organize), and it is where this chapter could balloon. Keep it to the
   single-user operations the RPCs already support; resist inventing a file manager.
7. **Screenshots rot.** Committed images drift from reality as the UI changes. Mitigation: the web
   shots are regenerated by `scripts/shots.sh` rather than hand-captured, so refreshing them is one
   command; native shots are few and explicitly listed in a checklist.

## Open questions (decide before the milestone that needs them)

1. ~~**Window-per-file, or one window?**~~ **RESOLVED (before D3): window-per-file.** Each file gets
   its own window with the filename in the title, so the Window menu, `⌘\`` cycling and D5's
   document behaviour all have something real to list. The known cost, accepted deliberately: every
   window boots the full Penpot SPA including the WASM renderer, so several open files means real
   memory use. Two consequences that bind later milestones — D3 must stop assuming a single
   hardcoded `"main"` window, and D5's single-instance guard must **forward** a document to the
   running app rather than boot a second stack.
2. **Keep Penpot's viewer (`/view`)?** It exists for share links (meaningless offline) but is also
   "present mode", which is genuinely useful locally. Keep, hide, or reach it only from our own UI?
   **Decide before D6.**
3. **Comments.** Collaborative by design, but usable as notes-to-self. Leave them (they are in-canvas
   and invariant 3 protects them anyway) or surface them as a native panel later? **Defer to chapter 5.**
