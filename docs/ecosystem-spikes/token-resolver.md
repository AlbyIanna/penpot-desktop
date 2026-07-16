# Spike: cross-package token resolver (mirror-and-surface)

Answers the second open question in [../ecosystem-design.md](../ecosystem-design.md) —
tokens are the cross-package edge ([contract-extractability caveat 5](contract-extractability.md)):
`appliedTokens` values are bare dotted paths (`layerBase.text`) with **no owning-package
qualifier**, so "which package does this token come from" is the seam between "just a
folder" and a package manager. This spike (PLAN3 E5) converts the prior code-read
findings into an **executed GO/NO-GO** on the **mirror-and-surface** token-package model.

**Verdict: GO WITH CAVEATS.** Mirroring a package's DTCG sets into a consumer's
`tokens.json` by explicit tooling round-trips A=B, carries a durable provenance stamp
(`tokens_lib` `:external-id`), resolves exactly as upstream's activation semantics imply
(order and theme activation ARE the contract), and is fully analyzable by a **static,
never-injected resolver** that separates real breakage from the starter kit's own
pre-existing noise. The caveats — what tokens.json will *not* carry, what "canonical
package content" must mean, and why read-only is convention-plus-detection — are listed
at the end; none is a blocker.

Everything below was produced by execution against **Penpot 2.16.2** (the app's own
re-provisioned headless stack, dedicated E5 ports 8998/6459/5533/6476/8999) on
2026-07-16, green on two consecutive fresh-stack runs. The executable gate is
[`scripts/e5-tokens-spike.sh`](../../scripts/e5-tokens-spike.sh) (`just e5`); its
helpers are [`scripts/ecosystem-spike/e5_probe.py`](../../scripts/ecosystem-spike/e5_probe.py)
(live probe, reusing `scripts/roundtrip.py`'s RPC client + normalizer) and
[`scripts/ecosystem-spike/e5_resolver.py`](../../scripts/ecosystem-spike/e5_resolver.py)
(the static resolver). **Every claim below falls into one of three evidence classes —
read them precisely:**

1. **Live-captured** (serialization, survival, RPC-observability): produced by the app's
   own stack and read back over the wire or from the exported bytes — e.g. what survives
   `tokens.json` round-trips, `tokenSetOrder`/`activeThemes` as `get-file data.tokensLib`
   reports them, A=B semantic hashes.
2. **Resolver-over-live-bytes** (resolved values): computed by OUR `e5_resolver.py` over
   the live-exported bytes — the backend never dereferences aliases, so every *resolved
   value* (`layerBase.text -> #565251`, token math, drift, bump severity) is our
   resolver's output, not a Penpot resolution-engine observation.
3. **Jar-confirmed** (resolution semantics): the *direction* of the rules our resolver
   implements — later-set-in-`tokenSetOrder` wins the merge, one active theme per group —
   is read from `app/common/types/tokens_lib.cljc` inside `runtime/backend/penpot.jar`.
   It is cross-confirmed by class 2 matching class 1's bytes, but it is NOT observed from a
   live Penpot resolution engine (there is none server-side).

The consequence is the standing caveat (caveat 5): a future Penpot that changed the
client-side merge *direction* would be caught by this gate ONLY if the serialized bytes
changed with it. A pure **resolution-semantics** change (same bytes, different merge)
is invisible here — **serialization drift is caught; resolution-semantics drift is not.**

## The model under test

A **token package** is a git repo of DTCG set files under `.penpot-packages/` (the E2
home). **Install = MIRROR**: an explicit tooling verb copies the package's sets into the
consumer file's `files/<fid>/tokens.json` — by editing the exported normalized tree and
in-place re-importing (never by driving the SPA; ecosystem invariant 3). Each mirror is
stamped with provenance and is **read-only by convention**: drift of a mirrored set is
handled by the exact conflict rule (`.conflict-<ts>` copy, overwrite neither side —
surface, don't apply). The resolver that reasons about updates is **static**: offline
files in, JSON report out.

## Grounding data

The real-world dump is the bundled `tokens-starter-kit` template (3.4 MB v3-zip, 14
token sets, 0 components — see [contract-extractability.md](contract-extractability.md)),
imported as-new to become "the package". Live numbers from the gate run:

| measure | value |
|---|---|
| token sets | 14 |
| tokens in the template **bytes** | 495 |
| tokens after 2.16.2 **import** | **490** — import silently drops 5 `strokeWidth`-type tokens (`line.xs/sm/md/lg/none`, unknown `$type`) |
| `appliedTokens` shape-attribute refs | 2 857 across the kit dump |
| distinct applied paths | 471 (kit dump) |
| **pre-existing dangling applied paths** | **16** (787 shape refs; the noise baseline — see below) |
| dangling *value*-refs (`{ref}` in `$value`) | 0 — `freeVariables = []` under the kit's own activation |

## 1. Mirror round-trips A=B (exit criterion a)

The probe authors a consumer (`create-file` + `update-file` placing a rect with
`appliedTokens: {fill: layerBase.text, width: modular.xl}`), then writes the consumer's
`tokens.json` containing **6 verbatim-mirrored package sets** (`Foundations - Fixed`,
`Foundations - Colors`, `Modular Scale`, `Color theme - Vibrant`, `Light - Base`,
`Dark - Base`) + its own `Consumer` set + 2 scripted collision sets + the provenance
theme, adds `design-tokens/v1` to the manifest and file features, and re-imports in
place. Then the `roundtrip.py` settle: export → normalize → in-place re-import →
re-export → semantic tree hash (sorted keys, LF, `createdAt`/`modifiedAt` stripped).

```
run 3: A = B = eb26fe5e438b5fb9a7a7d12e5402f832ae101034cc5f549d398e13f08d0b16da
run 4: A = B = 701f933c2288f3bc36dee1dfc5e70208f0827d01ae47132f6f867ccb37dbddcb   (fresh stack)
```

All 6 mirrored sets are **flatten-equal to the package source after the round trip** —
equal at the flatten level (`{path: {type, value, description}}`), which is exactly what
makes them drift-checkable. This is *flatten*-level equality, not raw-byte equality: the
canonical content is the settled export (§7 / caveat 2), and drift is a flatten-map
comparison, not a byte diff of the `tokens.json` container.

## 2. The provenance stamp: what survives `tokens.json` (and what doesn't)

The durable stamp is a **`$themes` entry in group `penpot:package`** whose `"id"` string
is not a uuid: 2.16.2's import keeps the raw string as the theme's `:external-id`
(schema is `:string`; `uuid/parse*` only feeds the internal `:id`) and **re-exports it
verbatim**. Its `selectedTokenSets` enumerate the mirrored, package-owned sets — that
list is the read-only-set declaration the drift detector consumes. Live capture:

```json
{ "id": "pkg:tokens-starter-kit@2.16.2-kit",
  "group": "penpot:package",
  "description": "",
  "selectedTokenSets": ["Color theme - Vibrant", "Dark - Base", "Foundations - Colors",
                        "Foundations - Fixed", "Light - Base", "Modular Scale"] }
```

Survival matrix, all captured live in one probe run:

| carrier | fate on import→export |
|---|---|
| theme `"id"` (non-uuid string) → `:external-id` | **SURVIVES verbatim** — the stamp |
| theme `selectedTokenSets` | **SURVIVES** — the package-owned set list |
| token-level `"$description"` | **SURVIVES** (`make-token` keeps it, `token->dtcg-token` re-emits) |
| theme `"description"` | **BLANKED** — `parse-multi-set-dtcg-json` calls `make-token-theme` without `:description` |
| extra `$metadata` key (`penpotPackages`) | **DROPPED** — export writes exactly `tokenSetOrder`/`activeThemes`/`activeSets` |
| set-root `"$description"` | **REJECTED at import** — `check-multi-set-dtcg-data` throws `server-error :assertion`; the whole import fails |

Consequence: `tokens.json` is **not an extensible carrier**. Richer provenance (version
pins, content hashes, source URL) must live in the vault's `lock.json` (E2), keyed by
the theme's external-id.

## 3. Order is contract (exit criterion b)

Two consumer sets define the same bare path with different values —
`E5 Collide - PkgA` (`collide.winner = #111111`) and `E5 Collide - PkgB` (`#222222`) —
both active, `tokenSetOrder [..., PkgA, PkgB]`. Upstream's
`get-tokens-in-active-sets` is an ordered merge where the later set overwrites:

```
before flip: winner = E5 Collide - PkgB  -> #222222   (later in tokenSetOrder)
flip ONLY $metadata.tokenSetOrder (PkgA <-> PkgB), in-place re-import, re-export:
after flip:  winner = E5 Collide - PkgA  -> #111111
```

The flipped order **survives the round trip** (exported tail =
`['Consumer', 'E5 Collide - PkgB', 'E5 Collide - PkgA']`) and is **RPC-observable**:
`get-file` returns `data.tokensLib` JSON-encoded through `export-dtcg-json`, and its
`$metadata.tokenSetOrder` equals the exported order both before and after the flip. The
classifier over the live before/after pair: **order-flip-on-collision = MAJOR**.

## 4. Theme activation is contract too (same file, same path, two values)

Within one file (the imported kit, same file id across the flip), the bare path
`layerBase.text` resolves:

```
activeThemes [..., "Color mode/Light", ...] -> #565251  (set "Light - Base", {neutral.600})
activeThemes [..., "Color mode/Dark",  ...] -> #958e8a  (set "Dark - Base",  {neutral.400})
```

The flip was pure file data (`$metadata.activeThemes` edit + in-place re-import), the
RPC side observed it, and the static resolver run on the **light** dump with
`--activate "Color mode/Dark"` (implementing `activate-theme`'s one-active-theme-per-
group rule) reproduces the live-flipped resolution exactly. Classifier:
**theme-only change = MAJOR-BEHAVIORAL**. Note the backend never dereferences aliases —
resolution is client-side (StyleDictionary in the SPA) — so *resolved-value* claims are
resolver-over-live-bytes (evidence class 2); order and activation ARE live-observable
(class 1).

**Honesty note on the static cross-check.** In this fixture the static-activation match is
*not discriminating* for the one-active-theme-per-group rule: `Dark - Base` follows
`Light - Base` in `tokenSetOrder`, so a plain later-set-wins merge would land the same
`layerBase.text` value even without applying group-exclusivity. The check confirms our
resolver matches the live bytes; it does **not** by itself prove one-theme-per-group —
that rule is jar-confirmed (class 3), not discriminated by this fixture. The probe check
is labelled `D.static-activation-matches-live-flip(non-discriminating)` to say so. A
fixture that ordered the two theme sets the other way would make the rule discriminating;
authoring it is a chapter-4 refinement, not a blocker for this GO-WITH-CAVEATS verdict.

## 5. The static resolver, headless (exit criterion c)

[`e5_resolver.py`](../../scripts/ecosystem-spike/e5_resolver.py) is stdlib-only Python:
offline files in (a normalized tree or a bare `tokens.json`), JSON report out — never
injected. The gate re-runs it **after the stack is shut down** to make "static" literal.
Semantics mirrored from `tokens_lib.cljc`: `tokenSetOrder ∪ remaining keys`; the hidden
theme (always active, sets = `activeSets`); one active theme per group; active set names
= union over active themes; ordered merge, later wins; `{ref}` aliases dereferenced
recursively; token math substituted and arithmetic-evaluated (regex-gated).

Headless over the live kit dump:

- **490 tokens resolved**, per-token deps listed for every ref-bearing token — e.g.
  `modular.xl` (`{modular.lg}*{density}`) → deps `[density, modular.lg]`, resolved
  `38.146973`; `body` → `{font-size.modular.xs}` → `15.625`. Token math is real.
- File-level `freeVariables = []`: every `$value` ref resolves in the active sets.
- **Partial-mirror deps are real**: the consumer mirrors `Modular Scale` *without* any
  Density set, and the resolver reports `danglingValueRefs {density: [9 modular.*
  tokens]}` — it names exactly what else a mirror needs. This is the dependency edge a
  future installer must chase.
- **Composite ($value is a dict/list, e.g. typography/shadow) handling is minimal**: the
  resolver recurses composites and collects/dereferences `{refs}` from **string leaves
  only** (scanning `json.dumps(dict)` would mint phantom refs from the object's own
  braces). Full composite semantics (per-sub-property typing, `$value` shorthand) are a
  chapter-4 refinement — leaf-string refs are enough for the dependency and free-variable
  accounting this spike needs.
- **Token math is bounded**: arithmetic is regex-gated AND the `**` power operator,
  overlong expressions, and oversized operands are rejected *before* `eval`, so a hostile
  `$value` like `99**999999` degrades to unresolved (left symbolic) instead of hanging.

## 6. Pre-existing dangling refs: the pinned noise baseline

The starter kit **ships broken refs**. Under active-set resolution, **16 distinct
`appliedTokens` paths dangle** (787 shape-attribute refs in the kit dump): 11 ship
dangling in the template itself —

```
spacing.sm ×612  spacing.md ×54  spacing.xs ×21  radius.none ×20  stroke.sm ×15
spacing.2 ×15    spacing.xl ×9   fixed.xl ×8     spacing.lg ×4    radius.2xs ×4
fixed.none ×4
```

— and **5 more are minted by 2.16.2's import itself** dropping the unknown-type
`strokeWidth` tokens (`line.xs/sm/md/lg/none`, 21 refs). **Pinning the baseline from the
live dump, not the template bytes, is load-bearing.**

Separation mechanism: `classify_bump` computes `danglingBaseline(before)` and counts
only `newDangling = after − before` as breakage. Verified live: a synthetic drop of
`layerBase.text` from `Light - Base` yields **MAJOR** (rule
`dropped-token-you-depend-on`, `newDangling = [layerBase.text]`) while all 16 baseline
paths stay classified as already-open noise. Also exercised: a shadowing-add (new
`E5 Brand Overrides` set appended, redefining `layerBase.text = #ff0000`; adds-only) =
**READS-MINOR-BEHAVES-MAJOR** — the token analogue of contract-extractability caveat 2,
special-cased by the classifier.

**The classifier is founded on a resolved-view diff.** `classify_bump` first flattens
each tree to `{dotted-path: resolved-value}` under its own active sets+themes and diffs
those: **any existing path whose resolved value moved is behavioral breakage**, no matter
how it was authored. This closes three multi-axis misses an earlier structural-only pass
graded PATCH/MINOR — an in-place `$value` edit of an applied token (`value-changed` =
MAJOR-BEHAVIORAL), dropping the *winning* colliding definition so a lower set silently
wins (`winning-definition-drop` = MAJOR), and a theme flip accompanied by any harmless
addition (`theme-change` = MAJOR-BEHAVIORAL, no longer masked to MINOR by the add). The
specific named cause (order-flip, winning-drop, dropped-token, value-changed, theme,
shadowing-add) is kept when identifiable; anything else that moved a value falls through
to MAJOR-BEHAVIORAL rather than to PATCH. These are locked by the gate's offline
adversarial pairs (`G.*`, no live stack needed, in both runs). Rank:
`MAJOR > MAJOR-BEHAVIORAL > READS-MINOR-BEHAVES-MAJOR > MINOR > PATCH`.

## 7. The rules this verdict ships

**Mirror-ownership rule.** A mirrored set is *package-owned*: stamped by the provenance
theme (group `penpot:package`, external-id `pkg:<name>@<version>`, `selectedTokenSets`
= the owned sets) and **read-only by convention** — enforced by detection, not
prevention (Penpot has no per-set ACL). The canonical package content is the
**post-import settled export** (the fixpoint), not the repo's raw bytes — see caveat 2.

**Drift = conflict copy.** Because mirrored sets stay flatten-equal to the source
(§1), drift detection is: flatten each provenance-declared set to
`{path: {type, value, description}}` and compare against the package's settled export.
All three fields round-trip (the survival matrix), so a consumer edit to a mirrored
token's `$description` is real drift and is now included in the comparison (an earlier
type+value-only compare read such an edit as "clean"). Demonstrated live:
`e5_resolver.py drift` returned all-6-clean on the real mirror, and flagged a mutated
`Light - Base` as `DRIFTED` (`changedPaths = ["layerBase.text"]`) prescribing
`conflict-copy (.conflict-<ts>), overwrite neither`. The E4 conflict channel already
implements the copy half for managed files; a build only wires detection to it. Nothing
ever silently rewrites the consumer.

## GO WITH CAVEATS — what a chapter-4 build must absorb

1. **`tokens.json` is not an extensible carrier.** Only the theme id (external-id), its
   `selectedTokenSets`, and token-level `$description` survive; theme descriptions are
   blanked, `$metadata` extras dropped, set-root keys rejected at import (a whole-import
   failure, not a silent drop). Version/hash pins belong in `lock.json`, keyed by the
   theme external-id.
2. **Canonical content = the post-import settled export, not raw repo bytes.** 2.16.2
   silently drops unknown-type tokens (5 `strokeWidth` tokens in the shipped kit), which
   both changes content and mints new dangling refs. Mirror the fixpoint; pin the
   dangling baseline from the *live* dump.
3. **Bare paths + global activation mean mirrored sets can shadow and be shadowed**
   (PLAN3 caveat 5, confirmed live). Read-only is convention enforced by drift
   detection; the classifier's shadowing-add special case
   (READS-MINOR-BEHAVES-MAJOR) is the guard against "adds-only" updates that flip
   resolutions.
4. **The provenance theme must stay INACTIVE** — its group `penpot:package` must never
   appear in `activeThemes`, or the one-active-theme-per-group rule would activate all
   mirrored sets at once. Activation is carried by `activeSets`/mirrored real themes
   (as the probe does).
5. **Resolved values are a client-side notion — and resolution-semantics drift is NOT
   gated.** The backend stores but never dereferences aliases; order and activation are
   RPC-observable (`get-file data.tokensLib`), resolution is the static resolver's job.
   Any future UI surface ("this update changes 37 resolved values") is resolver output,
   not a server query. **Standing caveat:** this gate catches *serialization* drift (the
   bytes: `tokenSetOrder`, `activeThemes`, set contents), but a future Penpot that changed
   the client-side merge *direction* without changing the bytes — a pure
   resolution-semantics change — would **not** be caught. The resolver's semantics are
   jar-confirmed against 2.16.2 (`tokens_lib.cljc`), so re-read the jar and re-run this
   gate on every Penpot version bump. See the three evidence classes in the intro.
6. **Composite ($value = dict/list) token support is minimal.** The resolver handles
   composites by collecting and dereferencing `{refs}` from string leaves only; full
   per-sub-property composite semantics are deferred to a chapter-4 build. Sufficient for
   dependency/free-variable accounting; not a full StyleDictionary composite resolver.

What a build would do with this: E2's installer grows a `mirror` verb for token
packages (copy settled sets + stamp + lock entry); the sync daemon's drift pass runs
`check_drift` per provenance theme and routes hits into the existing `.conflict-<ts>` +
E4 surface channel; the E4 update poll runs `classify_bump` between the locked version
and the fetched one and surfaces PATCH/MINOR/READS-MINOR-BEHAVES-MAJOR/
MAJOR-BEHAVIORAL/MAJOR — never auto-applying. The resolver's `danglingValueRefs`
listing gives the installer its dependency-chasing input (caveat: it names *set-level*
needs by free variable, not a package registry — cross-package naming still rides on
the lockfile).

## The gate

`just e5` → [`scripts/e5-tokens-spike.sh`](../../scripts/e5-tokens-spike.sh): boots a
fresh re-provisioned 2.16.2 on the dedicated E5 ports, runs the live probe
(mirror + A=B, provenance survival matrix, collision + order flip + RPC cross-checks,
theme flip, resolver-over-dump, synthetic drop, shadowing-add, drift), then re-runs the
resolver **with the stack down** and asserts the pinned numbers (490 tokens, 0 free
variables, 16-path baseline, token-math deps). Run-twice idempotent (fresh mktemp dirs
+ fresh DB each run); teardown frees all five ports even on failure. Green twice on
2026-07-16.

**Deliberately NOT chained into `just e2e`** — recorded decision: spike precedent (the
contract-extractability spike's scripts were never chained either), and E5 lands no
product code, so the ladder has nothing of E5's to regress. Re-run it on any Penpot
version bump (the semantics and pinned baselines are 2.16.2-specific) and when the
token-package build starts.
