# E7 — Plugin packages: activation + supply-chain spike (GO / CSP-GO)

**Verdict: ACTIVATION GO. CSP-EGRESS CSP-GO. Overall: GO.** Both legs proven
live twice on a Penpot 2.16.2 self-hosted stack (dedicated ports proxy 9022 /
backend 6484 / postgres 5557 / valkey 6500; off-origin beacon observer 9024).
Gate: `scripts/e7-plugins-spike.sh` (green twice as the activation spike, then
green twice again after the thin build — the gate now drives the REAL product
routes with PRODUCT DEFAULTS: no plugin env is passed; plugins flag, whitelist
pin, and the CSP header are default-ON in every boot). Because activation
landed GO, the staged E7 thin build shipped (see the thin-build section).

## The exact promise (ship with this wording, unchanged)

> **content-pinned + offline + consent-gated (local per-machine consent
> ledger) + egress-contained via a proxy-injected CSP on the SPA document
> (`default-src` baseline + `connect-src`/`img-src`/`form-action` fences,
> CSP-GO).**

The real containment pins are exactly four: the **content pin** (drift is
surfaced, never silently re-applied), **offline** (nothing is fetched from the
network), the **consent gate** (Penpot's own native Install/Allow, recorded in
a per-machine ledger that is the sole re-apply authority — a cloned vault
registers nothing), and the **CSP**. The `penpotPluginsWhitelist` is **NOT** a
containment pin — it is cosmetic (disclaimer-only; see the whitelist section).

A plugin is still arbitrary executable JS with `content:write` — once opened it
can read AND rewrite every shape/component/token in the OPEN file. The CSP
result contains *network exfiltration across the fenced vectors* (fetch/XHR/ws
via `connect-src`; images via `img-src`; form posts via `form-action`; the
`default-src` baseline catches the rest), NOT in-file data access. Never claim
data-isolation; claim egress-containment + the four pins above.

## Consent ledger — the re-apply authority (finding 1)

The lockfile alone must NEVER auto-register a plugin. `lock.json` is
git-versioned and portable (E6 carries it across vaults), so if a lock pin
drove boot re-apply, opening a cloned/pulled vault would seed a
consented-looking registration with no native Install/Allow ever having
happened on THIS machine — one Open → arbitrary `content:write` JS. That is the
consent-bypass this build closes.

Two separate files now govern the pointer:

- **`lock.json`** (vault root, git-versioned, E6-portable) keeps its
  `LockEntry.plugin_props` pin for **portability + gallery visibility** only. A
  pin is NO LONGER sufficient to auto-register.
- **`<data_dir>/plugin-consent.json`** (the consent ledger — a sibling of
  `postgres/`, OUTSIDE the vault, NOT git-versioned, survives a DB wipe, does
  NOT travel with the vault) is the **authority for boot re-apply**. Shape:
  `{ pluginId → { consentedContentHash, host, code, consentedAt } }`, atomic
  write (the lock/manifest discipline), schema-versioned.

Boot re-apply (`reapply_plugin_props` / the pure `plan_plugin_reapply`)
re-applies a pointer ONLY when ALL hold: (a) a `lock.json` pin exists, (b) the
pointer `host` is a local proxy origin (finding 5), (c) a ledger entry exists
for that pluginId, (d) the ledger's `consentedContentHash` == the CURRENT
package content hash (finding 3). Otherwise nothing is written and the
`/__api/packages/plugins` listing surfaces the state: `availableNeedsConsent`
(cloned vault — pin present, ledger absent) or `driftedNeedsReconsent` (served
code changed since consent). Insert-only always: a DB pointer the user already
has is never overwritten.

The capture loop records a ledger entry only when it observes a genuine
native-manager consent (a local-origin pointer live in the DB that we did not
seed) — sound precisely because re-apply never seeds without ledger authority.
A native-manager uninstall (a pluginId observed present this session that then
vanishes) prunes both the lock pin and the ledger entry; an absence never seen
present this session (a cloned pin, or a drift-declined pin) is left untouched.

## What a plugin package is

A git repo of static assets (`manifest.json` + `plugin.js` + icon) under
`<vault>/.penpot-packages/<pkg>/`, served AT THE LOCAL PROXY ORIGIN at
`/__packages/<pkg>/<path>` (scaffold route added in `apps/desktop/src/packages.rs`
— `serve_plugin_asset`, path-traversal-guarded; the bare `/__packages` gallery
page in `vault-index` is untouched). Carried-and-pointed-at, **never imported
into the design DB**. The registry pointer is DB-only derived state, re-appliable
after a wipe.

## Activation (the gate) — proven

The bundled-chromium browser leg (`scripts/ecosystem-spike/e7_activation_nav.cjs`,
offline playwright + `runtime/exporter-browsers`) drives Penpot's OWN native
Plugin Manager — test automation of the native UI, not our integration mechanism:

1. `/__bootstrap` server login → workspace deep link → workspace boots headless
   (CLJS + render-wasm).
2. Toolbar **Plugins** button → manager modal → type
   `http://localhost:9022/__packages/e7-fixture-plugin/manifest.json` → **Install**.
3. Penpot's OWN consent prompt (permissions dialog) → **Allow**. This calls
   `install_plugin!` → writes the pointer to profile props → shows the
   plugin-management modal. **Install does NOT run the plugin.**
4. The management modal's **Open** button (`open-plugin` → `load-plugin!`)
   evaluates `plugin.js`, which creates the `E7-FIXTURE-SHAPE` rectangle
   (verified over the `get-file` RPC — UI-independent).

**Invariant 3 witness (FULL SCRIPT SET — finding 4):** the witness hashes the
whole EXECUTED script set, not just `index.html` + `main.js`. It parses every
`<script src>` out of the served `index.html` (config.js, polyfills, libs.js —
the plugin SES runtime — main.js, and the chunks index.html pulls; parsed, not
hardcoded), fetches each, asserts **HTTP 200 + NON-EMPTY** before hashing (this
kills the old vacuous empty-hash pass a 404 could slip through), and sha256s
`index.html ++ every referenced body`. That combined hash is IDENTICAL
before/after the whole install flow, `<script>` count unchanged, and `main.js`
byte-identical. No patched/injected/driven SPA.

The claim is FLOW-invariance: the install/consent/open flow mutates no served
byte. The witness is before/after WITHIN ONE boot, where `config.js` is stable.
NOTE `config.js` is the one file the product rewrites — the boot-time
`enable-plugins` flag + whitelist injection is an INTENTIONAL config VALUE
change (a config value, not an app-logic patch), applied once at boot and
constant across the flow, so the same-boot before/after correctly proves
flow-invariance without hiding that one intentional config edit.

## Persistence — proven (now the PRODUCT path)

`update-profile-props` stores the registry under **`props.plugins.data.<pluginId>`**,
e.g. `{code, description, host, icon, name, permissions}` with
`host = "http://localhost:9022"` (the manifest ORIGIN) — server-side
malli-validated on 2.16.2 as `plugins = {ids: [string], data: {id → pointer}}`
(a write that omits `ids` is a 400; discovered by live probing). The shipped
flow the gate asserts end-to-end:

1. **Capture (consent recorded, never granted):** the product reconcile
   (`spawn_plugin_reconcile`, 5s loop) sees the pointer the USER's native
   Install+Allow wrote and pins it into BOTH `lock.json` —
   `LockEntry.plugin_props` (pluginId → canonical pointer JSON) + the content
   pin in `content_hash`, `file_id` empty (a plugin package never
   materializes a vault file) — AND the per-machine consent ledger
   (`<data_dir>/plugin-consent.json`, `consentedContentHash` = the current
   served-surface hash). The ledger is the RE-APPLY AUTHORITY; the lock pin is
   portability/visibility only. Uninstall through the native manager prunes
   both, so a reboot never resurrects against user intent.
2. **Wipe:** `rm -rf <data>/postgres` + reboot → READY on the wiped DB. The
   ledger lives OUTSIDE `postgres/` (at the data-dir root), so it survives.
3. **Boot re-apply (invariant 1, ledger-authorized):** the boot pass re-inserts
   a lock-pinned pointer missing from profile props via `update-profile-props`
   ONLY when the ledger authorizes it AND its consented content hash still
   matches the live served code AND its host is local (INSERT-ONLY — a DB entry
   is never overwritten; user consent wins). The gate asserts the ledger file
   survived the wipe, the restored `props.plugins.data` is IDENTICAL to the
   consented install's value, and the boot log carries `E7 boot re-apply …
   applied=1` (insert-only, so `applied=1` also proves the pointer was
   genuinely DB-only derived state after the wipe). A same-machine wipe keeps
   the data dir + ledger, so re-apply fires; a CLONED vault carries no ledger,
   so nothing re-applies (the `availableNeedsConsent` regression leg).

## CSP-egress probe — CSP-GO (the honest crux)

**Runtime mechanism (captured, not assumed):** the fixture ran with `hasSES:
true` and only TWO frames present — the workspace document and Penpot's own
`rasterizer.html` iframe. There is **no separate plugin document/worker frame**.
Penpot evaluates `plugin.js` inside a **SES `Compartment` in the SPA page
context** (`libs.js`: `hardenIntrinsics` / `createCompartment`; the plugin's
`fetch` endowment is a wrapped `window.fetch`). The plugin's UI iframe (`GXt`)
only exists if the plugin calls `openModal` — ours does not.

**Consequence:** a CSP header on the plugin ASSET response does nothing (the
code is fetched as text and evaluated in-page). Egress is governed by the CSP on
the **SPA document**. So the proxy adds a `Content-Security-Policy` header on
every `text/html` response (`crates/proxy` `html_csp`), **ON BY DEFAULT**
(CSP-GO shipped).

**Sharpened CSP (finding 2 — no longer `connect-src` only.)** A
`connect-src`-only policy fences fetch/XHR/WebSocket but leaves `img-src` /
`media-src` / `form-action` / everything-else WIDE OPEN — an
`new Image().src = off-origin` beacon (or a form POST) still exfiltrates. The
shipped default now leads with a `default-src` BASELINE and opens back up
exactly the vectors the app needs (empirically tuned live so the SPA +
render-wasm + the plugin SES `Compartment` all still work):

```
default-src 'self' data: blob:;
script-src 'self' 'unsafe-inline' 'unsafe-eval' 'wasm-unsafe-eval' blob:;
style-src 'self' 'unsafe-inline';  img-src 'self' data: blob:;
font-src 'self' data:;  media-src 'self' data: blob:;
worker-src 'self' blob:;  child-src 'self' blob:;  frame-src 'self';
connect-src 'self' ws://localhost:<port> ws://127.0.0.1:<port>;
form-action 'self';  base-uri 'self';  object-src 'none'
```

- `default-src 'self' data: blob:` — the same-origin baseline; `data:`/`blob:`
  carry NO off-origin host, so any un-enumerated directive is still fenced.
- `script-src` deliberately allows `'unsafe-eval'` + `'wasm-unsafe-eval'` — the
  SES `Compartment` needs `eval`/`Function` and render-wasm needs wasm
  compilation. A stricter `script-src` would break plugin evaluation and the
  app; that vector genuinely CANNOT be fenced without breaking things, so we
  name it honestly rather than over-claim.
- `img-src 'self' data: blob:` fences the image-beacon exfil vector;
  `connect-src` fences fetch/XHR/ws; `form-action` fences form-post exfil.

`'self'` allows `/api` + the `/__packages` code fetch; the explicit `ws://`
origins keep the notifications websocket unambiguous across engines (the GUI
runs in WKWebView, where `connect-src 'self'` + ws historically differed).
`PENPOT_LOCAL_CSP` overrides the value verbatim; `off`/`none`/`0` disables — the
gate's csp-off probe legs use that escape hatch, where the egress promise does
not hold. Header-only — the served `index.html` BYTES are **identical** with the
header on (invariant 3: a proxy header is arguably inside it; a JS patch is
not), and the header rides html documents only (asserted: `js/config.js`
carries none).

The gate now proves containment across **more than one vector**, at the level
each vector is reachable:

- **connect-src (fetch):** fired FROM the plugin — the SES Compartment endows
  `fetch` (its only network primitive). Beacon `src=fetch`.
- **img-src (image):** the plugin sandbox CANNOT construct one (`new Image()`
  throws `Image is not a constructor` in the hardened Compartment — witnessed
  live), so the image vector is fired at the **SPA-document level** by the
  browser leg (`page.evaluate(() => new Image().src = off-origin)`), which is
  exactly the level the proxy CSP `img-src` governs. Beacon `src=img`.

| leg | proxy CSP | `src=fetch` beacon | `src=img` beacon | plugin loads | console |
|-----|-----------|--------------------|------------------|--------------|---------|
| OFF | none | **OBSERVED** | **OBSERVED** | yes | egress left |
| ON  | default-src+fences | **ABSENT** | **ABSENT** | yes | `Refused… violates …` |

Both off-origin beacons (`127.0.0.1:9024`, a different port = different origin)
are blocked BEFORE leaving the browser under the CSP, while the plugin still
loads and produces its observable effect. Note the honest scope: a plugin's
only egress primitive is `fetch` (connect-src), so for the PLUGIN threat model
connect-src is the operative fence; the `default-src`/`img-src`/`form-action`
baseline hardens the whole SPA document as defense-in-depth (finding 2), and the
gate proves the `img-src` fence works at that document level. **CSP-GO.**

## Feature flags / whitelist (proven live — now PRODUCT DEFAULTS)

- Frontend flag: `enable-plugins` appended to the FRONTEND flag string only
  (`config.js`), DEFAULT-ON in every boot (`compose_frontend_flags`);
  `PENPOT_LOCAL_EXTRA_FRONTEND_FLAGS` appends more. Backend `PENPOT_FLAGS`
  untouched. The bundle reports `plugins/runtime` among enabled features.
- `penpotPluginsWhitelist` pinned to BOTH local-origin spellings
  (`http://localhost:<port>`, `http://127.0.0.1:<port>`) by default
  (`default_plugins_whitelist` → JSON array in `config.js`);
  `PENPOT_LOCAL_PLUGINS_WHITELIST` overrides. The gate asserts both defaults
  with NO env set.
  **CAVEAT — the whitelist is COSMETIC, not a containment pin (finding 6):**
  verified against the 2.16.2 bundle, `app.config/plugins-whitelist` is
  consulted SOLELY to hide the `permissions-disclaimer` for trusted hosts. It
  does **not** gate installs and does **not** block installs from other origins.
  The real containment pins are exactly: **content-pin + offline + consent-gate
  (the per-machine ledger) + CSP (`default-src` baseline + `connect-src` /
  `img-src` / `form-action` fences)**. Do NOT list "whitelisted-origin" among
  the containment pins — it is disclaimer-only.

## Design findings for the thin build

1. **Manifest `code`/`icon` MUST be origin-absolute paths (or `version: 2`).**
   For v1 manifests Penpot sets `host` = manifest ORIGIN (path stripped;
   `parse-manifest`), and loads code as `new URL(code, host)`. A relative
   `"code": "plugin.js"` from a package at a SUBPATH resolves to
   `/plugin.js` → the SPA fallback → `index.html` → SES rejects the HTML
   (`SES_HTML_COMMENT_REJECTED`). The fixture uses
   `"code": "/__packages/e7-fixture-plugin/plugin.js"`. The installer must
   rewrite/emit absolute code/icon paths when it serves a carried package.
2. **Surface-don't-apply:** the discovered plugin is presented; the USER clicks
   Install/Allow/Open. No pre-seeding path — the gate asserts no pointer exists
   before the browser leg's explicit steps.
3. **Ship the CSP ON by default** for the plugin promise to hold; document that
   `connect-src 'self'` is the minimal egress fence and does not restrict
   scripts/wasm/eval (the SES Compartment needs eval; a stricter `script-src`
   would break plugin evaluation — untested here, out of scope: `connect-src`
   alone suffices for egress containment and leaves the app intact).

## Artifacts

- `scripts/e7-plugins-spike.sh` — the staged activation + CSP gate (green twice).
- `scripts/ecosystem-spike/e7_activation_nav.cjs` — native Plugin Manager browser leg.
- `scripts/ecosystem-spike/e7_probe.py` — RPC witnesses (seed, profile-props, re-apply, shape count).
- `scripts/ecosystem-spike/e7_beacon.py` — off-origin egress observer.
- `scripts/ecosystem-spike/e7-fixture-plugin/` — the fixture package (now
  attempts BOTH a fetch and an image off-origin beacon).
- `crates/sync-core/src/consent.rs` — the per-machine consent ledger
  (`ConsentLedger`/`ConsentRecord`, `<data_dir>/plugin-consent.json`).
- `crates/proxy` `html_csp` (+ `PENPOT_LOCAL_CSP`); `apps/desktop`
  `PENPOT_LOCAL_EXTRA_FRONTEND_FLAGS` / `PENPOT_LOCAL_PLUGINS_WHITELIST` and the
  `/__packages/<pkg>/{*path}` serve route (hardened in the thin build).
- `scripts/m4-artifact-test.sh` (g5) — the offline dmg leg (run by the m4
  orchestration, needs a fresh dmg).
- `justfile` — `e7` recipe, chained into `e2e`.

## Thin build (shipped — activation GO)

The staged thin build landed the promise in product Rust (verified live on a
wipe→reboot round trip):

- **Serving:** `GET /__packages/{pkg}/{*path}` (`apps/desktop/src/packages.rs`,
  `serve_plugin_asset`) hardened for product: safe package id, per-segment
  path validation (no `..`/`.`/dotfiles — `.git/config` is a 400), and a
  canonicalize containment check so a symlink inside a hostile package can
  never read outside the package home (404). Content types by extension.
- **Boot wiring (default ON):** `enable-plugins` rides the FRONTEND flag
  string and `penpotPluginsWhitelist` pins both local-origin spellings in
  every boot (`compose_frontend_flags` / `default_plugins_whitelist`);
  `PENPOT_LOCAL_EXTRA_FRONTEND_FLAGS` / `PENPOT_LOCAL_PLUGINS_WHITELIST`
  extend/override.
- **CSP (CSP-GO → shipped ON by default, sharpened per finding 2):** the proxy
  adds the `default-src` baseline + `script-src`(eval/wasm) + `img-src` +
  `connect-src` + `form-action` + `object-src 'none'` policy above on every
  `text/html` response (`resolve_html_csp` / `default_html_csp`);
  `PENPOT_LOCAL_CSP` overrides the value, `PENPOT_LOCAL_CSP=off` disables it
  (gate probe legs only — the egress promise does not hold there).
- **Pointer pin + LEDGER-AUTHORIZED re-apply (invariant 1, finding 1):** a boot
  reconcile (`spawn_plugin_reconcile`, mirroring the E3 re-link pattern)
  re-inserts a lock-pinned pointer missing from profile props via
  `update-profile-props` ONLY when the per-machine consent ledger authorizes it
  (ledger entry present AND `consentedContentHash` == live content hash AND host
  local — the pure `plan_plugin_reapply`), insert-only (DB/user consent wins for
  present entries; the 2.16.2 schema is `plugins = {ids: [string], data:
  {pluginId → pointer}}` and the merge keeps `ids` in sync, malli-validated
  server-side). A capture loop RECORDS what the user installs through the native
  manager into BOTH `LockEntry.plugin_props` (portability pin + content pin) AND
  the ledger (`consentedContentHash`); a native-manager uninstall prunes both.
  `lock.json` is the portability/visibility pin, NOT the re-apply authority —
  the ledger is. A cloned vault (no ledger) registers nothing.
- **Surfacing (surface-don't-apply):** `GET /__api/packages/plugins` lists
  discovered plugin packages (manifest URL, permissions, live, and the consent
  `state`: `available` / `availableNeedsConsent` / `driftedNeedsReconsent` /
  `installed`); the `/__packages` gallery page renders them with a copy-the-
  manifest-URL affordance and instructions to install through Penpot's own
  Plugin Manager. Nothing is ever registered automatically.

## The gate, the dmg leg, and the e2e chaining decision

**Gate:** `scripts/e7-plugins-spike.sh` (`just e7`; dedicated ports
9022/6484/5557/6500, beacon 9024; PID-scoped teardown — it never touches
another gate's port block). After the thin build it drives the REAL product
surfaces end-to-end, all in PRODUCT-DEFAULT boots (no plugin env):

- default flags + whitelist witnessed in the served `config.js`; hardened
  serve route (`.git/config` → 400);
- discovery surface before install (`installed=false`, `live=false` — no
  pre-seeding path) and after (`installed=true`, `live=true`);
- the native Plugin Manager browser leg (bundled chromium): URL → Install →
  Penpot's own consent → Open → `E7-FIXTURE-SHAPE` asserted over RPC;
- invariant-3 FULL-SCRIPT-SET witness (finding 4): `index.html` + every
  referenced script (each 200 + non-empty) sha256 identical before/after,
  `<script>` count unchanged;
- product capture pin in `lock.json` + consent ledger; DB wipe (keep data dir +
  ledger) → ledger-authorized boot re-apply (ledger file survived, restored
  pointer identical, `E7 boot re-apply … applied=1` log witness);
- **SEEDED-VAULT (finding 1 security regression):** a lock.json pin with NO
  local ledger (a cloned vault, fresh data dir) registers NOTHING —
  `state=availableNeedsConsent`, no pointer in profile props, no ledger written;
- **DRIFT (finding 3):** consent, mutate `plugin.js`, wipe DB, reboot → NOT
  auto-re-registered, `state=driftedNeedsReconsent`;
- CSP-egress MULTI-VECTOR probe: `PENPOT_LOCAL_CSP=off` leg → BOTH fetch and
  image beacons OBSERVED; product-DEFAULT leg (no env) → BOTH ABSENT +
  CSP-blocked while the plugin still loads (default-src + img-src + connect-src
  fences witnessed on the SPA document header).

**dmg leg:** `scripts/m4-artifact-test.sh` gained (g5): the installed .app,
under `env -i` + poisoned proxies (fully offline) with a plugin package
present, serves `/__packages/<pkg>/manifest.json` + `plugin.js`, refuses
dotfile paths (400), lists the package un-installed on
`/__api/packages/plugins`, and ships the DEFAULT CSP header on the SPA
document. (Run by the m4 orchestration with a fresh dmg, not by `just e7`.)

**Chaining decision:** `just e7` IS chained into `just e2e` — the build
precedent (E1–E4): E7's thin build landed product code (proxy CSP layer,
boot flag/whitelist defaults, the serve route, the lock capture/re-apply
reconcile) that the ladder must keep honest. The pure-verdict spikes E5/E6
stay unchained (spike precedent — no product code to regress).

**Final gate runs (thin build, product defaults):** green TWICE, 28 PASS /
0 FAIL each, `ACTIVATION-VERDICT: GO` + `CSP-VERDICT: CSP-GO`. Captured in
those runs: SPA witness `index.html` sha256 `4ae3ade9…` / `main.js`
`c54f55e1…` identical before/after install and byte-identical under the CSP
header; lock pin landed within the 45s window with `fileId` empty; wipe →
boot re-apply restored `props.plugins.data` identical with `applied=1`;
csp-off leg 1 beacon request observed at `127.0.0.1:9024`; product-default
leg 0 requests, 2 CSP violations in the browser console (`Refused to
connect … connect-src 'self' ws://localhost:9022 ws://127.0.0.1:9022`)
while `E7-FIXTURE-SHAPE` still appeared over RPC. Teardown PID-scoped; all
E7 ports (9022/6484/5557/6500/9024) clean after both runs and the sibling
gate port block was never touched.
