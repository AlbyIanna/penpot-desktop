# Spike: is a component's contract machine-extractable from disk?

Answers the first open question in [../ecosystem-design.md](../ecosystem-design.md):
the ecosystem's versioning story defines a component's **contract** as exactly
`{variant names, exposed properties, tokens used}` (implementation excluded), so a
change is **patch** (implementation only), **minor** (contract grew), or **major**
(contract lost/renamed). This spike checks whether those three fields are cleanly
present in the on-disk normalized binfile-v3 JSON, stable across a round-trip, and
diffable into patch/minor/major.

**Verdict: GO WITH CAVEATS.** Two of the three legs are solid and round-trip-stable
with real data (**tokens used**, **first-class variant properties**). The third —
**variant names** — is solid too, but only recoverable via a naming/path *heuristic*
in the bundled corpus, because the 2024-era templates predate Penpot's first-class
variants. Nothing here sinks the design; the caveats a PLAN3 must absorb are listed
at the end.

Everything below was produced by execution against **Penpot 2.16.2** (the `penpot-m0`
docker stack) on 2026-07-15. Throwaway scripts live in
[`scripts/ecosystem-spike/`](../../scripts/ecosystem-spike/); they reuse
`scripts/roundtrip.py`'s client + normalizer (the same semantics the sync daemon uses).

## Grounding data

Both bundled templates were imported as-new and their normalized `.penpot` trees
dumped (`import_and_dump.py`):

| template | on-disk | files | pages | components | tokens.json | `appliedTokens` shapes |
|---|---|---|---|---|---|---|
| `penpot-design-system` | 37 MB v3-zip | 24 246 | 41 | 730 | — | 0 |
| `tokens-starter-kit` | 3.4 MB v3-zip | 1 717 | 10 | 0 | ✓ (14 sets) | 1 551 |

The corpus is **split**: the design system has components/variants but no tokens; the
tokens kit has tokens but no components. Neither bundled file contains a *first-class*
variant or a component that *uses* a token — so those two combinations were produced
by execution (injection round-trip, below), not just read off disk.

## The on-disk JSON shapes (real excerpts)

### A component definition — `files/<fid>/components/<id>.json`

Thin. It only names the component and points at its main instance; the structure lives
in the main-instance shape tree.

```json
{
  "id": "046b950b-8782-809f-8004-217fbeccb248",
  "mainInstanceId": "046b950b-8782-809f-8004-217a495eda19",
  "mainInstancePage": "bc1ad05b-5a3a-80ed-8004-57d0a9009de9",
  "modifiedAt": "2024-07-12T07:15:26.939Z",
  "name": "Main",
  "path": "Dark / Workspace / Design Tab / Component"
}
```

Keys present across all 730 design-system components: `id, mainInstanceId,
mainInstancePage, modifiedAt, name, path` (+ `deleted`/`objects` on 6 tombstones,
`annotation` on 1). The main instance is a `frame` shape with `mainInstance: true`,
`componentId`, and a `shapes` child-id array you can walk to reach every descendant.

### Variants — two models, and which one the corpus uses

Penpot 2.16.2 supports **first-class variants** (`app/common/types/variant.cljc` in the
runtime jar). The authoritative malli schema:

```clojure
schema:variant-property   [:map [:name :string] [:value :string]]
schema:variant-component  ; merged into schema:component
  [:map [:variant-id {:optional true} uuid]
        [:variant-properties {:optional true} [:vector schema:variant-property]]]
schema:variant-shape      ; on the main-instance root shape
  [:map [:variant-id ...] [:variant-name {:optional true} :string] ...]
```

So a first-class variant set serializes structurally: each component JSON carries
`variantId` + `variantProperties` (a vector of `{name, value}`), and the main-instance
shape carries `variantId`/`variantName`. **Exposed properties are the distinct
`variantProperties[].name`** — this IS Penpot's component-property system; there is no
separate Figma-style typed/boolean/instance-swap property mechanism in 2.16.2 (grep of
the jar's `types/` schemas: the only `propert*` schema is `variant-property`).

**But the bundled `penpot-design-system` (data-model v67, authored 2024) contains ZERO
first-class variants** — `grep variantProperties` = 0 files, no `variantId` on any
shape/component, even though the file's `features` list advertises `variants/v1`
(features are inherited from the team, not proof of use). Its variants are the **legacy
naming convention**: a *variant set* is a group of components sharing a `path`, and each
component's `name` is the variant. Real example — path `Dark / Combobox` groups 9
components:

```
Active, Default, Disabled, Empty, Error, Focus, Hover, Selected, Success
```

### Tokens — `files/<fid>/tokens.json` (DTCG / Tokens-Studio format)

```json
{
  "$metadata": { "activeSets": [...], "activeThemes": [...], "tokenSetOrder": [...] },
  "$themes":  [ { "name": "Global", "group": "Always enabled",
                  "selectedTokenSets": { "Foundations - Colors": "enabled", ... } } ],
  "Foundations - Scales": {
    "space": { "linear": { "100": { "$type": "spacing", "$value": "{linear.100}" } } }
  },
  "Light - Base": {
    "buttonPrimary": { "background": { "default": { "$type": "color",
                                                    "$value": "{accent.600}" } } } }
}
```

14 token sets, `$type`/`$value` leaf tokens, `{ref}` cross-references, themes that
enable/disable sets. This is exactly what `vault-index` could index for token *names*.

### How a shape references a token — `appliedTokens`

Every shape can carry an `appliedTokens` map: **shape attribute → token path**.

```json
{ "id": "...", "type": "text", "name": "V2",
  "appliedTokens": { "fill": "layerBase.text" } }
```

Across `tokens-starter-kit`, the distinct attribute keys and how often they appear:

```
fill:1106  columnGap:439  rowGap:390  height:129  width:111  p1..p4:~70 each
r1..r4:63 each  fontSize:53  strokeColor:36  strokeWidth:36  opacity:15  rotation:11
```

Token refs are dotted paths into `tokens.json` (`spacing.sm`, `layerBase.text`,
`radius.modular.full`, ...). **This is the "tokens used" leg, and it is fully
structured.**

## The extractor

[`extract_contract.py`](../../scripts/ecosystem-spike/extract_contract.py) takes a
normalized `.penpot` tree and emits one contract per **variant set**:

- **set key** — `variantId` for first-class sets, else the shared `path`.
- **variantNames** — the component `name`s in the set (both models).
- **exposedProperties** — distinct `variantProperties[].name` (first-class only; empty
  for path-convention — kept independent from variant names so adding a property axis
  reads as growth, not a rename).
- **tokensUsed** — the union of `appliedTokens` values over every shape in every main
  instance of the set.

It's pure/offline and contains **no ids in the contract body** — only names, property
names, and token paths — which is what makes it round-trip-stable (ids churn, contracts
don't).

### Real extracted contracts

`penpot-design-system` → **201 variant sets** (all path-convention, 0 with tokens):

```json
{"set":"Dark / Combobox","setKind":"path-convention","componentCount":9,
 "variantNames":["Active","Default","Disabled","Empty","Error","Focus","Hover","Selected","Success"],
 "exposedProperties":[], "tokensUsed":[]}
{"set":"Dark / Input","setKind":"path-convention","componentCount":7,
 "variantNames":["Active","Default","Disabled","Error","Focus","Hover","Success"],
 "exposedProperties":[], "tokensUsed":[]}
{"set":"Icons / Actions","setKind":"path-convention","componentCount":15,
 "variantNames":["Close M","Configure","Copy","Corners","Delete", ...], ...}
```

`tokens-starter-kit` → **0 contracts** (no components) — it contributes the token
*vocabulary* the design system's components would consume, not components itself.

A **complete three-legged contract** (all fields populated) only exists after the
injection test below:

```json
{"set":"6938347f-…","setKind":"first-class-variant","componentCount":1,
 "variantNames":["Default"],
 "exposedProperties":["Size","State"],
 "tokensUsed":["layerBase.text"]}
```

## Stability across a round-trip (verified by execution)

[`roundtrip_preserve.py`](../../scripts/ecosystem-spike/roundtrip_preserve.py) runs the
N6 settle semantics (export → normalize → in-place re-import → re-export) on
`tokens-starter-kit` and checks two things:

**A. Real-data token stability.** A shape's `appliedTokens = {"fill":"layerBase.text"}`
is **byte-identical after the round-trip** → the "tokens used" leg survives.

**B. First-class variant preservation (injection).** A schema-valid first-class variant
component was written into the tree by hand — a `variantId` + `variantProperties:
[{Size,Large},{State,Default}]` on a new `components/<id>.json`, and
`variantId`/`variantName`/`appliedTokens` on the promoted main-instance frame — then
re-imported in place and re-exported. Result:

```
[B RESULT] component json survived: True
[B RESULT] component variant fields: {"variantId":"6938…","variantProperties":
           [{"name":"Size","value":"Large"},{"name":"State","value":"Default"}],
           "name":"Default","path":"Spike / InjectedVariantSet"}
[B RESULT] shape fields: {"variantId":"6938…","variantName":"Size=Large, State=Default",
           "mainInstance":true,"appliedTokens":{"fill":"layerBase.text"}}
```

**Penpot 2.16.2's binfile round-trip preserves first-class variant properties and
applied tokens written to disk, verbatim.** This is the decisive GO evidence: the
contract fields are not lossy on the import path. (`revn`/timestamps churn per the M0
normalization spec, but no contract field does.)

## Diffability — patch vs minor vs major (the crux, proven)

[`make_deltas.py`](../../scripts/ecosystem-spike/make_deltas.py) takes the
round-tripped tree (which now has a real first-class variant with all three legs) and
edits the on-disk JSON directly to build three deltas;
[`diff_contracts.py`](../../scripts/ecosystem-spike/diff_contracts.py) classifies each
(patch = contract identical; minor = only grew; major = anything removed/renamed):

| delta | on-disk edit | classifier output |
|---|---|---|
| **(i) implementation-only** | move the main-instance shape (`x/y`) + change an inline (non-token) `fillColor` | **PATCH** — "no contract changes" |
| **(ii) contract grew** | add `variantProperties: {Theme, Dark}` | **MINOR** — `exposedProperties added: ["Theme"]` |
| **(iii) contract lost/renamed** | remove the `State` property + repoint a token (`layerBase.text`→`layerOne.text`) | **MAJOR** — `exposedProperties removed: ["State"]` |

Raw output:

```
===== (i) implementation-only ===== OVERALL BUMP: PATCH  (no contract changes)
===== (ii) added exposed property ===== OVERALL BUMP: MINOR
   exposedProperties {added: ["Theme"]}
===== (iii) removed property + changed token ===== OVERALL BUMP: MAJOR
   exposedProperties {removed: ["State"]},  tokensUsed {added: ["layerOne.text"]}
```

The extracted-contract diff cleanly separates the three severities. One honest nuance
surfaced by execution: **`tokensUsed` is a set-union over the whole variant set**, so
repointing one shape's token registers as *major* only if the old token is dropped
set-wide; if a sibling shape still uses it, the swap reads as an addition (minor). That
is the correct semantics ("does the set still depend on token X?"), and delta (iii)
shows it — the major there is driven by the property removal while the token swap shows
as an addition because `layerBase.text` is still used elsewhere in the set.

## Verdict, per leg

| contract leg | recoverable from disk? | round-trip stable? | verdict |
|---|---|---|---|
| **tokens used** (`appliedTokens`) | **Yes**, fully structured (attr → token path) | **Yes** (real data) | **SOLID** |
| **exposed properties** (`variantProperties[].name`) | **Yes** when first-class variants are used; **absent** in the 2024 bundled corpus | **Yes** (injection round-trip) | **SOLID where used; not present in legacy files** |
| **variant names** | **Yes**, but via a **path/name heuristic** in legacy files; structural when first-class | **Yes** | **SOLID (heuristic for legacy)** |

**Nothing is genuinely unrecoverable.** The one true gap is that the bundled 2024
templates never exercised first-class variants/properties, so "exposed properties" is
empty there — not because the data model can't express it (it can, and round-trips
cleanly), but because those files predate the feature.

## GO WITH CAVEATS — what a PLAN3 must account for

1. **Two variant models, forever.** The extractor must handle both first-class
   (`variantId`/`variantProperties`) and the legacy path/name convention. Real files in
   the wild (and the design system we ship) are legacy; new files will be first-class.
   Contract extraction is a union of both, not one or the other.
2. **Legacy "exposed properties" are undeclared.** In path-convention sets the property
   *axes* aren't on disk — only variant names are. A contract for a legacy set has an
   empty `exposedProperties`, so a legacy→first-class migration will *look* like the
   contract grew (minor). Decide whether a variant-model migration is exempt from the
   bump rules or gets special-cased.
3. **Set identity must be matched by a stable key, not `variantId`.** `variantId` is a
   uuid that survives *in-place* import but is remapped by *import-as-new*. Diff two
   versions of a package by variant-set **name/path**, not raw id, or a re-imported
   package reads as "everything removed + everything added" (all-major noise).
4. **`tokensUsed` is a set-union at the set granularity.** That's the right default, but
   it means per-variant token changes are invisible unless the token leaves the set
   entirely. If per-variant token contracts matter, extract per-component, not per-set.
5. **Tokens are the cross-package edge.** `appliedTokens` values are bare paths
   (`layerBase.text`) with no owning-package qualifier. When tokens live in a *different*
   package (the dependency-graph question), the contract needs to record *which* token
   package/version a path resolves against — the path alone is ambiguous across
   packages. This is the seam between "just a folder" and a package manager, and it is
   not yet designed.
6. **Component `annotation`/`objects` tombstones exist** (deleted components keep a
   record); the extractor already skips `deleted: true`, but the versioner should treat
   a component going `deleted` as a major (set shrank).

None of these is a blocker. The core mechanism — extract `{variant names, exposed
properties, tokens used}` from the normalized tree, diff it, classify patch/minor/major —
works on real 2.16.2 data and is stable across the exact round-trip the sync daemon runs.
