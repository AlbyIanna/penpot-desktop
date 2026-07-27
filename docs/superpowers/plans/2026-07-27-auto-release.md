# Automatic Versioned Releases Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every merge to `main` whose commits contain a `feat`/`fix` automatically publishes a versioned GitHub Release (correct semver + changelog) from the same single build that already feeds the rolling `latest` prerelease.

**Architecture:** A pure bash script (`scripts/release-plan.sh`) reads git history and decides bump/version/changelog; `release.yml`'s `prepare` job runs it (needs `fetch-depth: 0`); the `publish` job publishes the same artifacts to the rolling `latest` and — when a release is warranted — to a new `vX.Y.Z` created with the default `GITHUB_TOKEN` (which cannot retrigger workflows). The build jobs are untouched.

**Tech Stack:** bash, git, GitHub Actions, `gh` CLI. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-07-27-auto-release-design.md`

## Global Constraints

- **The build jobs (`macos-dmg`, `linux-appimage`) must not change** — they already consume `needs.prepare.outputs.version`.
- **Commits of THIS work use types `ci:`, `test:`, `docs:` only** (never `feat`/`fix`) — once merged, these commits are themselves parsed; they must not influence the bump.
- Only bare `vX.Y.Z` tags anchor the commit range; `v*-rc*` and any suffixed tag never do.
- The version must be computed in `prepare`, **before** the builds (tauri bakes it into the bundle name).
- Never interpolate `github.*` context into shell bodies — pass through `env:` (existing convention in `release.yml`).
- Version strings must pass the existing injection guard: only `[A-Za-z0-9.+-]`.
- Gate scripts follow the repo style: `#!/usr/bin/env bash`, `set -uo pipefail` (test gate) / `set -euo pipefail` (plan script), `pass()`/`fail()` counters, exit non-zero on any FAIL.
- `Cargo.toml` workspace version stays `0.3.0` (becomes a documented placeholder).

## File Structure

| File | Role |
|---|---|
| `scripts/release-plan.sh` (create) | The decision: bump, version, release_tag, changelog. Pure over git history. |
| `scripts/release-plan-test.sh` (create) | Gate: synthetic throwaway git repos pin every rule and edge case. |
| `justfile` (modify, ~line 439) | Chain `release-plan-test.sh` into `just e2e`. |
| `.github/workflows/release.yml` (modify: `prepare` ~lines 56-110, `publish` ~lines 320-end) | `fetch-depth: 0`, run the script, new `release_tag` output, changelog artifact; publish the versioned release. |
| `docs/RELEASES.md` (modify) | Document the automatic path; revise the "product-owner decision" paragraph. |
| `Cargo.toml` (modify, line ~21) | Comment marking the version as a placeholder. |

---

### Task 1: `release-plan.sh` — core rules (bump, version arithmetic, changelog)

**Files:**
- Create: `scripts/release-plan.sh`
- Create: `scripts/release-plan-test.sh`

**Interfaces:**
- Produces (consumed by Task 3's `prepare` job): invocation `bash scripts/release-plan.sh <changelog-out.md>` run from the repo root; stdout key=value lines:
  - `bump=major|minor|patch|none`
  - `last_tag=<vX.Y.Z or empty>` (previous bare release tag, never one pointing at HEAD)
  - `head_tag=<vX.Y.Z or empty>` (bare tag pointing at HEAD)
  - `version=<X.Y.Z>` when `bump!=none`, else `<X.Y.(Z+1)>-dev.<shortsha>`
  - `release_tag=v<version>` when `bump!=none`, else empty
  - changelog written to `<changelog-out.md>`: `### Features` / `### Bug fixes` / `### Other` groups (empty groups omitted), one line per commit. Optional env `REPO_URL` turns the short sha into a commit link.

- [ ] **Step 1: Write the failing test gate (core cases)**

Create `scripts/release-plan-test.sh`:

```bash
#!/usr/bin/env bash
# Gate for scripts/release-plan.sh: the conventional-commit release decision.
# Builds throwaway git repos (empty commits — no files needed) and pins:
#   bump rules   feat->minor, fix/perf->patch, breaking->major, rest->none
#   version math X.Y.Z arithmetic; bump=none -> next-patch + "-dev.<shortsha>"
#   anchoring    only bare vX.Y.Z tags anchor the range (rc tags never);
#                a bare tag on HEAD forces bump=none
#   changelog    grouped, merge commits excluded, prefix stripped
# Pure and fast: no network, no ports, no stack. Chained into `just e2e`.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$ROOT/scripts/release-plan.sh"
PASS=0; FAIL=0
pass() { echo "  PASS  $*"; PASS=$((PASS+1)); }
fail() { echo "  FAIL  $*"; FAIL=$((FAIL+1)); }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# --- fixture helpers ---------------------------------------------------------
G() { git -c user.email=t@test -c user.name=t -C "$1" "${@:2}"; }
mk_repo() {  # mk_repo <name> -> path; starts with one chore commit on main
  local r="$TMP/$1"
  mkdir -p "$r"
  G "$r" init -q -b main "$r" >/dev/null
  G "$r" commit -q --allow-empty -m "chore: root"
  echo "$r"
}
cm() { G "$1" commit -q --allow-empty -m "$2"; }   # cm <repo> <subject>
cmb() { G "$1" commit -q --allow-empty -m "$2" -m "$3"; }  # with body
run_plan() {  # run_plan <repo> -> stdout of the script; changelog at <repo>/ch.md
  (cd "$1" && bash "$SCRIPT" ch.md)
}
val() { printf '%s\n' "$1" | sed -n "s/^$2=//p"; }  # val <output> <key>
expect() {  # expect <label> <output> <key> <want>
  local got; got="$(val "$2" "$3")"
  if [ "$got" = "$4" ]; then pass "$1: $3=$4"; else fail "$1: $3 got '$got' want '$4'"; fi
}

# --- 1. no tags + a feat -> first release 0.1.0 ------------------------------
R="$(mk_repo t1)"; cm "$R" "feat: first feature"
OUT="$(run_plan "$R")"
expect "no-tag+feat" "$OUT" bump minor
expect "no-tag+feat" "$OUT" version 0.1.0
expect "no-tag+feat" "$OUT" release_tag v0.1.0
expect "no-tag+feat" "$OUT" last_tag ""

# --- 2. no tags + docs only -> none, 0.0.1-dev.<sha> -------------------------
R="$(mk_repo t2)"; cm "$R" "docs: readme"
OUT="$(run_plan "$R")"
SHORT="$(G "$R" rev-parse --short=7 HEAD)"
expect "no-tag+docs" "$OUT" bump none
expect "no-tag+docs" "$OUT" version "0.0.1-dev.$SHORT"
expect "no-tag+docs" "$OUT" release_tag ""

# --- 3. tag v1.2.3 + docs/refactor/chore only -> none, 1.2.4-dev -------------
R="$(mk_repo t3)"; G "$R" tag v1.2.3
cm "$R" "docs: notes"; cm "$R" "refactor(core): shuffle"; cm "$R" "chore: deps"
OUT="$(run_plan "$R")"
SHORT="$(G "$R" rev-parse --short=7 HEAD)"
expect "quiet" "$OUT" bump none
expect "quiet" "$OUT" last_tag v1.2.3
expect "quiet" "$OUT" version "1.2.4-dev.$SHORT"
expect "quiet" "$OUT" release_tag ""

# --- 4. tag v1.2.3 + fix -> patch 1.2.4 --------------------------------------
R="$(mk_repo t4)"; G "$R" tag v1.2.3; cm "$R" "fix(sync): off-by-one"
OUT="$(run_plan "$R")"
expect "fix" "$OUT" bump patch
expect "fix" "$OUT" version 1.2.4
expect "fix" "$OUT" release_tag v1.2.4

# --- 5. tag v1.2.3 + feat + fix -> minor 1.3.0 -------------------------------
R="$(mk_repo t5)"; G "$R" tag v1.2.3
cm "$R" "fix: small"; cm "$R" "feat(cli): new verb"; cm "$R" "perf: faster"
OUT="$(run_plan "$R")"
expect "feat+fix" "$OUT" bump minor
expect "feat+fix" "$OUT" version 1.3.0
expect "feat+fix" "$OUT" release_tag v1.3.0

# --- 6. changelog: grouped, prefix stripped, scope bolded --------------------
CH="$(cat "$TMP/t5/ch.md")"
echo "$CH" | grep -q '^### Features$'      && pass "changelog: Features group"   || fail "changelog: Features group missing"
echo "$CH" | grep -q '^### Bug fixes$'     && pass "changelog: Bug fixes group"  || fail "changelog: Bug fixes group missing"
echo "$CH" | grep -q '\*\*cli:\*\* new verb' && pass "changelog: scoped subject" || fail "changelog: scoped subject missing"
echo "$CH" | grep -q 'feat(cli):'          && fail "changelog: prefix not stripped" || pass "changelog: prefix stripped"

echo
echo "release-plan-test: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
```

- [ ] **Step 2: Run the gate — expect failure (script missing)**

Run: `bash scripts/release-plan-test.sh`
Expected: fails immediately — `scripts/release-plan.sh: No such file or directory` (every case FAILs / the run aborts non-zero).

- [ ] **Step 3: Implement `scripts/release-plan.sh`**

```bash
#!/usr/bin/env bash
# Release plan: decide, from the conventional commits since the last release
# tag, whether HEAD deserves a versioned release, which version, and with what
# changelog. PURE over git history: no network, no side effects beyond the
# changelog file. This is the single place the version decision lives — the
# release workflow only transports its outputs (see docs/RELEASES.md).
#
# Usage:  release-plan.sh <changelog-out.md>     (run from the repo root)
# Env:    REPO_URL (optional) — e.g. https://github.com/OWNER/REPO; when set,
#         changelog short-shas become commit links.
# Stdout: key=value lines
#   bump=major|minor|patch|none
#   last_tag=v1.2.3      previous bare vX.Y.Z tag (never one pointing at HEAD;
#                        empty if none exists)
#   head_tag=v1.3.0      bare vX.Y.Z tag pointing at HEAD (empty if none) —
#                        when present, bump is forced to none: this commit IS
#                        already a release (the manual-tag path owns it)
#   version=1.3.0        bump!=none: the next version
#          =1.2.4-dev.<shortsha>  bump=none: next-patch prerelease (sorts
#                        BEFORE the eventual real 1.2.4)
#   release_tag=v1.3.0   empty when bump=none
#
# Rules (highest wins, merge commits excluded — PR titles are narrative here;
# the semantics live in the commits inside the PR):
#   type! / BREAKING CHANGE: -> major     feat -> minor     fix|perf -> patch
#   docs|test|refactor|chore|ci|style|non-conforming -> none
# Only bare vX.Y.Z tags anchor the range: rc/suffixed tags (v0.2.0-rc1) are
# validation artifacts and must never eat a real release's commit range.
set -euo pipefail

OUT="${1:?usage: release-plan.sh <changelog-out.md>}"

# --- tag anchoring -----------------------------------------------------------
BARE='^v[0-9]+\.[0-9]+\.[0-9]+$'
HEAD_TAG="$(git tag --points-at HEAD --list 'v*' | grep -E "$BARE" | sort -V | tail -1 || true)"
LAST_TAG="$(git tag --merged HEAD --list 'v*' | grep -E "$BARE" \
              | { [ -n "$HEAD_TAG" ] && grep -vxF "$HEAD_TAG" || cat; } \
              | sort -V | tail -1 || true)"
if [ -n "$LAST_TAG" ]; then RANGE="$LAST_TAG..HEAD"; else RANGE="HEAD"; fi

# --- classify commits --------------------------------------------------------
BUMP="none"
raise() {  # raise <level>: escalate BUMP (major > minor > patch > none)
  case "$1:$BUMP" in
    major:*) BUMP="major" ;;
    minor:major) ;; minor:*) BUMP="minor" ;;
    patch:major|patch:minor) ;; patch:*) BUMP="patch" ;;
  esac
}
FEATS=""; FIXES=""; OTHERS=""
while IFS=$'\t' read -r sha subject; do
  [ -n "$sha" ] || continue
  short="${sha:0:7}"
  ref="($short)"
  [ -n "${REPO_URL:-}" ] && ref="([${short}](${REPO_URL}/commit/${sha}))"
  type=""; scope=""; bang=""; text="$subject"
  if [[ "$subject" =~ ^([A-Za-z]+)(\(([^\)]*)\))?(!)?:[[:space:]]*(.*)$ ]]; then
    type="${BASH_REMATCH[1]}"; scope="${BASH_REMATCH[3]}"
    bang="${BASH_REMATCH[4]}"; text="${BASH_REMATCH[5]}"
  fi
  line="- ${scope:+**${scope}:** }${text} ${ref}"
  body="$(git log -1 --format=%b "$sha")"
  if [ "$bang" = "!" ] || grep -q '^BREAKING CHANGE:' <<<"$body"; then
    raise major
  fi
  case "$type" in
    feat)      raise minor; FEATS="${FEATS}${line}"$'\n' ;;
    fix|perf)  raise patch; FIXES="${FIXES}${line}"$'\n' ;;
    *)                      OTHERS="${OTHERS}${line}"$'\n' ;;
  esac
done < <(git log "$RANGE" --no-merges --format=$'%H\t%s')

# A bare tag on HEAD means this commit is already released (manual-tag path).
[ -n "$HEAD_TAG" ] && BUMP="none"

# --- version arithmetic ------------------------------------------------------
SHORT_HEAD="$(git rev-parse --short=7 HEAD)"
BASE="${HEAD_TAG:-$LAST_TAG}"
if [ -z "$BASE" ]; then
  # First release ever: any release-worthy work lands as 0.1.0.
  if [ "$BUMP" = "none" ]; then VERSION="0.0.1-dev.${SHORT_HEAD}"
  else VERSION="0.1.0"; fi
else
  IFS=. read -r MA MI PA <<<"${BASE#v}"
  case "$BUMP" in
    major) VERSION="$((MA+1)).0.0" ;;
    minor) VERSION="${MA}.$((MI+1)).0" ;;
    patch) VERSION="${MA}.${MI}.$((PA+1))" ;;
    none)  VERSION="${MA}.${MI}.$((PA+1))-dev.${SHORT_HEAD}" ;;
  esac
fi
RELEASE_TAG=""
[ "$BUMP" != "none" ] && RELEASE_TAG="v${VERSION}"

# --- changelog ---------------------------------------------------------------
{
  [ -n "$FEATS" ]  && printf '### Features\n\n%s\n'  "$FEATS"
  [ -n "$FIXES" ]  && printf '### Bug fixes\n\n%s\n' "$FIXES"
  [ -n "$OTHERS" ] && printf '### Other\n\n%s\n'     "$OTHERS"
  if [ -z "$FEATS$FIXES$OTHERS" ]; then printf 'No changes since %s.\n' "${BASE:-the beginning}"; fi
} >"$OUT"

echo "bump=$BUMP"
echo "last_tag=$LAST_TAG"
echo "head_tag=$HEAD_TAG"
echo "version=$VERSION"
echo "release_tag=$RELEASE_TAG"
```

Then: `chmod +x scripts/release-plan.sh scripts/release-plan-test.sh`

- [ ] **Step 4: Run the gate — expect all core cases PASS**

Run: `bash scripts/release-plan-test.sh`
Expected: `release-plan-test: N passed, 0 failed`, exit 0.

Note: `mk_repo` uses `git init -q -b main` — available in git ≥ 2.28, fine on this machine and on `ubuntu-24.04` runners.

- [ ] **Step 5: Sanity-check against the real repo history**

Run from the repo root (NOT committed as a test — history moves):
```bash
bash scripts/release-plan.sh /tmp/real-ch.md && head -30 /tmp/real-ch.md
```
Expected: `bump=minor`, `last_tag=v0.4.1`, `version=0.5.0`, `release_tag=v0.5.0`; the changelog's `### Features` lists the `feat(c3)` commits (penpot CLI, daemon verbs, runtime.json) with scopes bolded.

- [ ] **Step 6: Commit**

```bash
git add scripts/release-plan.sh scripts/release-plan-test.sh
git commit -m "ci: release-plan.sh — the conventional-commit release decision, with its gate"
```

---

### Task 2: edge cases — rc exclusion, HEAD-tagged, breaking, merge exclusion — then chain into `just e2e`

**Files:**
- Modify: `scripts/release-plan-test.sh` (append cases before the summary block)
- Modify: `scripts/release-plan.sh` (only if a case fails)
- Modify: `justfile` (~line 439, end of the `e2e` recipe)

**Interfaces:**
- Consumes: Task 1's script and gate.
- Produces: the frozen contract Tasks 3–4 rely on; `just e2e` runs the gate.

- [ ] **Step 1: Append the edge-case tests**

Insert into `scripts/release-plan-test.sh`, immediately BEFORE the final `echo`/summary block:

```bash
# --- 7. breaking via "!" -> major --------------------------------------------
R="$(mk_repo t7)"; G "$R" tag v1.2.3; cm "$R" "feat(api)!: drop legacy verbs"
OUT="$(run_plan "$R")"
expect "bang" "$OUT" bump major
expect "bang" "$OUT" version 2.0.0

# --- 8. breaking via body footer -> major ------------------------------------
R="$(mk_repo t8)"; G "$R" tag v1.2.3
cmb "$R" "fix: change default port" "BREAKING CHANGE: the control port moved"
OUT="$(run_plan "$R")"
expect "body-breaking" "$OUT" bump major
expect "body-breaking" "$OUT" version 2.0.0

# --- 9. rc tags never anchor the range ---------------------------------------
R="$(mk_repo t9)"; G "$R" tag v1.2.3
cm "$R" "feat: one"; G "$R" tag v1.3.0-rc1; cm "$R" "feat: two"
OUT="$(run_plan "$R")"
expect "rc-ignored" "$OUT" last_tag v1.2.3
expect "rc-ignored" "$OUT" version 1.3.0
# BOTH feats (before and after the rc tag) are in the changelog:
grep -q 'one' "$TMP/t9/ch.md" && pass "rc-ignored: pre-rc commit in changelog" \
                              || fail "rc-ignored: pre-rc commit missing"

# --- 10. bare tag on HEAD -> bump=none, rolling derives from it --------------
R="$(mk_repo t10)"; G "$R" tag v1.2.3; cm "$R" "feat: shipped"; G "$R" tag v1.3.0
OUT="$(run_plan "$R")"
SHORT="$(G "$R" rev-parse --short=7 HEAD)"
expect "head-tagged" "$OUT" bump none
expect "head-tagged" "$OUT" head_tag v1.3.0
expect "head-tagged" "$OUT" version "1.3.1-dev.$SHORT"
expect "head-tagged" "$OUT" release_tag ""

# --- 11. merge commits are excluded ------------------------------------------
R="$(mk_repo t11)"; G "$R" tag v1.2.3
G "$R" checkout -q -b feature
cm "$R" "fix: real work"
G "$R" checkout -q main
cm "$R" "docs: mainline noise"
G "$R" merge -q --no-ff feature -m "feat: LYING merge subject"
OUT="$(run_plan "$R")"
expect "merge-excluded" "$OUT" bump patch   # the fix counts; the merge's fake feat does not
grep -q 'LYING' "$TMP/t11/ch.md" && fail "merge-excluded: merge subject leaked" \
                                 || pass "merge-excluded: merge subject absent"

# --- 12. non-conforming subjects -> none, never crash ------------------------
R="$(mk_repo t12)"; G "$R" tag v1.2.3
cm "$R" "just a plain sentence with (parens) and: colons"
OUT="$(run_plan "$R")"
expect "non-conforming" "$OUT" bump none
```

- [ ] **Step 2: Run the gate**

Run: `bash scripts/release-plan-test.sh`
Expected: PASS across the board — Task 1's implementation already encodes these rules. If any case fails, fix `release-plan.sh` (not the test) until green; the likely trouble spots are the `grep -vxF` tag exclusion and the merge `--no-merges` flag.

- [ ] **Step 3: Chain into `just e2e`**

In `justfile`, append to the `e2e` recipe (after line 439, `bash scripts/d6-residue.sh`):

```make
    bash scripts/release-plan-test.sh
```

- [ ] **Step 4: Verify the recipe parses and the gate runs under just**

Run: `just release-plan-test 2>/dev/null; bash -n scripts/release-plan.sh && bash -n scripts/release-plan-test.sh && just --summary | head -1`
Expected: `bash -n` silent (both scripts parse); `just --summary` prints the recipe list (justfile parses). (`just release-plan-test` may say "unknown recipe" — the gate is chained inside `e2e`, not a standalone recipe; that is fine and consistent with other gates.)

- [ ] **Step 5: Commit**

```bash
git add scripts/release-plan-test.sh scripts/release-plan.sh justfile
git commit -m "test(ci): release-plan edge cases — rc anchoring, HEAD tag, breaking, merge exclusion; chain into e2e"
```

---

### Task 3: `release.yml` `prepare` — full history, run the plan, export outputs, upload the changelog

**Files:**
- Modify: `.github/workflows/release.yml` (the `prepare` job, ~lines 56–110)

**Interfaces:**
- Consumes: Task 1's script contract (`bash scripts/release-plan.sh changelog.md`, key=value stdout).
- Produces (consumed by Task 4's `publish`): new job output `release_tag` (empty ⇒ rolling only); artifact `release-changelog` containing `changelog.md`. Existing outputs `version`, `tag`, `prerelease`, `rolling` keep their meaning.

- [ ] **Step 1: Give the checkout full history and run the plan**

Replace the `prepare` job's checkout step and the `- id: v` step. The checkout becomes:

```yaml
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0   # release-plan.sh needs every tag and the full commit range
```

The `- id: v` step becomes (note the added `release_tag` output and the added `REPO_URL`; the dispatch and tag branches are byte-identical to today):

```yaml
      - id: v
        env:
          # Pass untrusted context through env (never interpolate github.* into
          # the shell body — command-injection surface).
          EVENT_NAME: ${{ github.event_name }}
          INPUT_VERSION: ${{ github.event.inputs.version }}
          INPUT_PRERELEASE: ${{ github.event.inputs.prerelease }}
          REF: ${{ github.ref }}
          REF_NAME: ${{ github.ref_name }}
          SHA: ${{ github.sha }}
          REPO_URL: ${{ github.server_url }}/${{ github.repository }}
        run: |
          set -euo pipefail
          # The version decision lives in scripts/release-plan.sh (testable
          # locally; see scripts/release-plan-test.sh). This step only routes
          # its outputs per event. Run it on EVERY event so the changelog
          # artifact always exists for publish.
          PLAN="$(bash scripts/release-plan.sh changelog.md)"
          echo "$PLAN"
          plan() { printf '%s\n' "$PLAN" | sed -n "s/^$1=//p"; }

          ROLLING="false"
          RELEASE_TAG=""
          if [ "$EVENT_NAME" = "workflow_dispatch" ]; then
            # Manual validation run: a deletable prerelease under the given version.
            VERSION="${INPUT_VERSION#v}"
            TAG="v${VERSION}"
            PRERELEASE="$INPUT_PRERELEASE"
          elif [ "${REF#refs/tags/}" != "$REF" ]; then
            # A v* tag -> the official, pinned versioned release (manual path).
            TAG="$REF_NAME"
            VERSION="${TAG#v}"
            PRERELEASE="false"
          else
            # A push to main -> always roll "latest"; ADDITIONALLY, when the
            # commits since the last release tag warrant it, publish the same
            # artifacts as a real vX.Y.Z (the auto path). The tag is created
            # by publish with GITHUB_TOKEN, which cannot retrigger workflows.
            VERSION="$(plan version)"
            RELEASE_TAG="$(plan release_tag)"
            TAG="latest"
            PRERELEASE="true"
            ROLLING="true"
          fi
          # Reject anything that is not a sane semver-ish token: injection guard
          # AND a fail-fast for a malformed tag/version.
          case "$VERSION" in
            *[!A-Za-z0-9.+-]*|"") echo "invalid version: '$VERSION'" >&2; exit 1 ;;
          esac
          echo "version=$VERSION"         >>"$GITHUB_OUTPUT"
          echo "tag=$TAG"                 >>"$GITHUB_OUTPUT"
          echo "prerelease=$PRERELEASE"   >>"$GITHUB_OUTPUT"
          echo "rolling=$ROLLING"         >>"$GITHUB_OUTPUT"
          echo "release_tag=$RELEASE_TAG" >>"$GITHUB_OUTPUT"
          echo "Resolved version=$VERSION tag=$TAG prerelease=$PRERELEASE rolling=$ROLLING release_tag=$RELEASE_TAG"

      - name: Upload changelog artifact
        uses: actions/upload-artifact@v4
        with:
          name: release-changelog
          path: changelog.md
          if-no-files-found: error
```

And add `release_tag` to the job's `outputs:` block:

```yaml
    outputs:
      version: ${{ steps.v.outputs.version }}
      tag: ${{ steps.v.outputs.tag }}
      prerelease: ${{ steps.v.outputs.prerelease }}
      rolling: ${{ steps.v.outputs.rolling }}
      release_tag: ${{ steps.v.outputs.release_tag }}
```

Also update the two header comment blocks at the top of the file: the "Triggers" list gains a line for the auto path (`push main -> rolling latest + auto vX.Y.Z when commits warrant`), and the "Version derivation" paragraph now points at `scripts/release-plan.sh` instead of the committed `Cargo.toml`.

- [ ] **Step 2: Validate the YAML parses**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml')); print('yaml ok')"`
Expected: `yaml ok`. (If PyYAML is missing: `python3 -m pip install --user pyyaml` or use `ruby -ryaml -e "YAML.load_file('.github/workflows/release.yml'); puts 'yaml ok'"`.)

- [ ] **Step 3: Dry-run the routing logic locally**

The step body is plain bash over env vars — exercise the main-push branch exactly as the runner would:

```bash
cd "$(git rev-parse --show-toplevel)"
env EVENT_NAME=push REF=refs/heads/main REF_NAME=main SHA=$(git rev-parse HEAD) \
    GITHUB_OUTPUT=/tmp/gh-out bash -c '
  set -euo pipefail
  PLAN="$(bash scripts/release-plan.sh /tmp/ch.md)"
  plan() { printf "%s\n" "$PLAN" | sed -n "s/^$1=//p"; }
  VERSION="$(plan version)"; RELEASE_TAG="$(plan release_tag)"
  echo "would build VERSION=$VERSION RELEASE_TAG=$RELEASE_TAG"'
```
Expected: `would build VERSION=0.5.0 RELEASE_TAG=v0.5.0` (values move with history; the point is: non-empty, semver-shaped, consistent with Task 1 Step 5).

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci(release): prepare derives version from release-plan.sh over full history"
```

---

### Task 4: `release.yml` `publish` — changelog in the body, versioned-release leg

**Files:**
- Modify: `.github/workflows/release.yml` (the `publish` job, ~line 320 to end)

**Interfaces:**
- Consumes: `needs.prepare.outputs.release_tag` (Task 3); artifact `release-changelog` (auto-downloaded — the existing `download-artifact` step has no `name:` filter, so it lands at `artifacts/release-changelog/changelog.md`).
- Produces: the rolling `latest` (unchanged semantics) and, when `release_tag` is non-empty, the Release `vX.Y.Z` with the same two assets.

- [ ] **Step 1: Rewrite the publish run step**

Add `RELEASE_TAG: ${{ needs.prepare.outputs.release_tag }}` to the step's `env:` block, then replace the step's `run:` body with:

```bash
set -euo pipefail

DMG="$(ls artifacts/*/Penpot-Local_*_macos-arm64.dmg | head -1)"
APPIMAGE="$(ls artifacts/*/Penpot-Local_*_linux-x86_64.AppImage | head -1)"
test -f "$DMG"
test -f "$APPIMAGE"

# What's-changed section from the prepare job's release-plan run.
CHANGELOG="artifacts/release-changelog/changelog.md"
WHATS_CHANGED=""
if [ -s "$CHANGELOG" ]; then
  WHATS_CHANGED="$(printf "## What's changed\n\n"; cat "$CHANGELOG")"
fi

# write_body <file> <rolling: true|false> — one body builder for both legs so
# the rolling and the versioned release can never drift apart.
write_body() {
  local file="$1" rolling="$2" note=""
  if [ "$rolling" = "true" ]; then
    note=$'> **Rolling build** — automatically republished from the latest commit on `main`. It moves with `main`; for a pinned, stable release use a versioned `v*` release.\n'
  fi
  cat >"$file" <<EOF
Penpot Local ${VERSION} — offline, local-first Penpot desktop.

${note}

## Downloads

| Platform | Asset |
|---|---|
| macOS (Apple Silicon) | \`$(basename "$DMG")\` |
| Linux (x86_64) | \`$(basename "$APPIMAGE")\` |

${WHATS_CHANGED}

## macOS (Apple Silicon / arm64)

- Requires **macOS 15.4 or newer** on an **Apple Silicon** Mac. Intel Macs are not supported.
- The app is **ad-hoc signed** (no Developer ID, not notarized). On first launch Gatekeeper blocks a double-click. **Right-click the app → Open**, then confirm — or run \`xattr -d com.apple.quarantine "/Applications/Penpot Local.app"\`. Subsequent launches open normally.

## Linux (x86_64)

- **x86_64 only** (no arm64 AppImage).
- Requires **FUSE** to run the AppImage (\`sudo apt install libfuse2\` on Debian/Ubuntu, or run with \`--appimage-extract-and-run\`).
- The bundled runtime relies on a small set of host system libraries per the AppImage excludelist convention (fontconfig / X11 / glib for ImageMagick, krb5 / libxml2 for postgres). Present on normal desktop distros; absent in minimal containers.

Everything runs offline — the embedded Postgres, Valkey, backend JVM, proxy, and packaged exporter ship inside the artifact. No account, no network.
EOF
}

FLAGS=()
if [ "$PRERELEASE" = "true" ]; then FLAGS+=(--prerelease); fi

BODY_FILE="$(mktemp)"
write_body "$BODY_FILE" "$ROLLING"

# Rolling "latest": delete the previous release + tag so the recreated
# tag tracks the current main commit (gh release edit cannot move a tag).
if [ "$ROLLING" = "true" ] && gh release view "$TAG" >/dev/null 2>&1; then
  echo "Rolling latest: removing the previous $TAG release + tag."
  gh release delete "$TAG" --yes --cleanup-tag
fi

if gh release view "$TAG" >/dev/null 2>&1; then
  echo "Release $TAG exists — updating notes/flags."
  gh release edit "$TAG" --notes-file "$BODY_FILE" "${FLAGS[@]}"
else
  echo "Creating release $TAG (target $GITHUB_SHA)."
  gh release create "$TAG" \
    --title "Penpot Local ${VERSION}" \
    --notes-file "$BODY_FILE" \
    --target "$GITHUB_SHA" \
    "${FLAGS[@]}"
fi

gh release upload "$TAG" "$DMG" "$APPIMAGE" --clobber

# Auto versioned release (push-to-main with release-worthy commits): the SAME
# artifacts, published a second time under vX.Y.Z. gh creates the tag via the
# API with GITHUB_TOKEN — which GitHub never retriggers workflows for, so
# this cannot loop and cannot double-build.
if [ -n "${RELEASE_TAG:-}" ]; then
  REL_BODY="$(mktemp)"
  write_body "$REL_BODY" "false"
  if gh release view "$RELEASE_TAG" >/dev/null 2>&1; then
    echo "Release $RELEASE_TAG exists (re-run) — updating."
    gh release edit "$RELEASE_TAG" --notes-file "$REL_BODY" --latest
  else
    echo "Creating versioned release $RELEASE_TAG (target $GITHUB_SHA)."
    gh release create "$RELEASE_TAG" \
      --title "Penpot Local ${VERSION}" \
      --notes-file "$REL_BODY" \
      --target "$GITHUB_SHA" \
      --latest
  fi
  gh release upload "$RELEASE_TAG" "$DMG" "$APPIMAGE" --clobber
fi

echo "Attached:"
echo "  $(basename "$DMG")"
echo "  $(basename "$APPIMAGE")"
```

Note: no `--prerelease` on the versioned leg (it is a real release) and `--latest` is explicit so the *Latest* badge moves to it.

- [ ] **Step 2: Validate YAML + the embedded bash**

Run:
```bash
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml')); print('yaml ok')"
python3 - <<'PY' > /tmp/publish-body.sh
import yaml
wf = yaml.safe_load(open('.github/workflows/release.yml'))
steps = wf['jobs']['publish']['steps']
print([s for s in steps if 'Release' in s.get('name','')][0]['run'])
PY
bash -n /tmp/publish-body.sh && echo "publish bash ok"
```
Expected: `yaml ok` and `publish bash ok`.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci(release): publish cuts the auto vX.Y.Z from the same build as the rolling latest"
```

---

### Task 5: docs — RELEASES.md and the Cargo.toml placeholder

**Files:**
- Modify: `docs/RELEASES.md` (the "How to cut a release" area, lines ~29–74)
- Modify: `Cargo.toml` (the `[workspace.package]` `version` line, ~line 22)

**Interfaces:**
- Consumes: the shipped behavior of Tasks 1–4 (document what IS, not what was planned).

- [ ] **Step 1: Update `docs/RELEASES.md`**

Three edits:

1. In "How to cut a release" (line ~31), replace the opening and step 1 with:

```markdown
The version is **derived from git tags** — never hardcoded in the workflow or
in `Cargo.toml`. There are two paths:

- **Automatic (the normal path):** every merge to `main` runs
  `scripts/release-plan.sh`, which reads the conventional commits since the
  last bare `vX.Y.Z` tag. At least one `feat` → minor; only `fix`/`perf` →
  patch; a `!` type suffix or a `BREAKING CHANGE:` footer → major; only
  `docs`/`test`/`refactor`/`chore` → no versioned release (the rolling
  `latest` still updates). When a release is warranted, the `publish` job
  publishes the **same artifacts** it just attached to `latest` as a real
  Release `vX.Y.Z`, creating the tag with the default `GITHUB_TOKEN` (which
  GitHub never retriggers workflows for — no loop, no second build). The
  release body carries a generated "What's changed" changelog grouped from
  the same commits. Merge commits are ignored: semantics live in the commits
  inside the PR, not in PR titles. `rc`/suffixed tags (e.g. `v0.2.0-rc1`)
  never anchor the commit range.

- **Manual (the escape hatch):** push a tag yourself — unchanged:
```

keeping the existing `git tag` / `git push` snippet as the manual path, and then:

2. Replace the RELEASES.md:34 "product-owner decision" paragraph with:

```markdown
The committed workspace version in `Cargo.toml` is a **placeholder** (it stays
at its historical value): the released version comes from the git tag, patched
into the runner's `Cargo.toml` at build time (ephemeral, never committed).
Local `cargo` builds report the placeholder — that is expected.
```

3. In the "Rolling latest" section (line ~62), update the version example: the rolling version is now **next-patch-dev** (e.g. `0.4.2-dev.84148b6`, derived by `release-plan.sh` so it sorts *before* the eventual real 0.4.2), no longer the committed `Cargo.toml` version. Add one line: after a release lands, verify not only that `latest` advanced but that the new `vX.Y.Z` release exists with both assets (the dmg CI step is flaky — an existing known issue).

- [ ] **Step 2: Mark the Cargo.toml version as a placeholder**

In `Cargo.toml`, directly above the `[workspace.package]` `version = "0.3.0"` line (~line 22), add:

```toml
# Placeholder — released versions are derived from git tags by the release
# workflow (scripts/release-plan.sh; see docs/RELEASES.md). Do not bump.
```

- [ ] **Step 3: Verify nothing else still claims Cargo.toml drives the version**

Run: `grep -rn "committed workspace version" .github/ docs/RELEASES.md | grep -v "placeholder"`
Expected: no hits that describe the OLD behavior (the release.yml header comment was already updated in Task 3; fix any straggler found).

- [ ] **Step 4: Verify the workspace still builds metadata cleanly**

Run: `cargo metadata --no-deps --format-version 1 >/dev/null && echo "cargo ok"`
Expected: `cargo ok` (the comment must not break the TOML).

- [ ] **Step 5: Commit**

```bash
git add docs/RELEASES.md Cargo.toml
git commit -m "docs(releases): document the automatic versioned path; Cargo.toml version is a placeholder"
```

---

### Task 6: live validation — a deletable prerelease run, then teardown

**Files:** none (operational verification; requires pushing the branch and ~35 min of runner time — macOS bills 10×).

**Interfaces:**
- Consumes: everything shipped in Tasks 1–5, pushed to the feature branch.

- [ ] **Step 1: Push the branch**

```bash
git push -u origin HEAD
```

- [ ] **Step 2: Dispatch a validation run against this branch**

`release.yml` already exists on `main`, so `workflow_dispatch --ref <branch>` reads the branch's edited workflow (the RELEASES.md:168 caveat only applies before first merge — not our case):

```bash
gh workflow run release.yml --repo AlbyIanna/penpot-desktop \
  --ref "$(git branch --show-current)" \
  -f version=0.5.0-rc1 -f prerelease=true
```

- [ ] **Step 3: Watch it to completion**

```bash
gh run watch --repo AlbyIanna/penpot-desktop \
  "$(gh run list --repo AlbyIanna/penpot-desktop --workflow release.yml \
      --limit 1 --json databaseId --jq '.[0].databaseId')"
```
Expected: all four jobs green. The `prepare` log must show the `release-plan.sh` key=value output (with `fetch-depth: 0` the tags resolved). This validates: script-in-CI, changelog artifact plumbing, and that the dispatch/tag paths still work. (The auto `vX.Y.Z` leg itself cannot fire pre-merge — it requires a real push to `main`; it is covered by the local tests and by Step 6.)

- [ ] **Step 4: Prove the release body and assets**

```bash
gh release view v0.5.0-rc1 --repo AlbyIanna/penpot-desktop --json body \
  --jq .body | grep -A5 "What's changed"
gh release download v0.5.0-rc1 --repo AlbyIanna/penpot-desktop --dir /tmp/rel-check
ls -la /tmp/rel-check
```
Expected: the body contains "What's changed" with the `feat(c3)` entries; both the dmg and the AppImage download, non-empty.

- [ ] **Step 5: Tear down the throwaway release and tag**

```bash
gh release delete v0.5.0-rc1 --repo AlbyIanna/penpot-desktop --yes --cleanup-tag
git push origin :refs/tags/v0.5.0-rc1 2>/dev/null || true
```

- [ ] **Step 6: Post-merge check (deferred — after the PR merges)**

After the PR merges to `main`, the push run must produce BOTH:
```bash
gh release view latest --repo AlbyIanna/penpot-desktop   # rolling, moved to the merge sha
gh release view v0.5.0 --repo AlbyIanna/penpot-desktop   # the auto release, Latest badge
gh release download v0.5.0 --repo AlbyIanna/penpot-desktop --dir /tmp/v050-check
```
Expected: `v0.5.0` exists, `isLatest: true`, both assets present (version may be higher than 0.5.0 if other feats merge first — the check is "a new bare tag exists at the merge sha", not the literal number).

---

## Self-review (done at planning time)

- **Spec coverage:** script+rules → T1/T2; `fetch-depth: 0` + prepare → T3; single-build double-publish + changelog body + `--latest` → T4; RELEASES.md revision + Cargo.toml placeholder → T5; live validation + first-merge check → T6. Spec §5.1 real-history check → T1 Step 5 (deliberately not a committed test: history moves). No gaps found.
- **Type consistency:** the script's stdout keys (`bump`, `last_tag`, `head_tag`, `version`, `release_tag`) match what T3's `plan()` extracts and T4's env consumes; artifact name `release-changelog` matches between T3 upload and T4 path.
- **Placeholder scan:** clean — every step carries its code or exact expected output.
