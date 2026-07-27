# Automatic versioned releases from conventional commits

**Date:** 2026-07-27
**Status:** approved design, not yet implemented
**Extends:** `docs/RELEASES.md` (the manual `v*` tag flow and the rolling `latest`
prerelease stay; this adds the automatic versioned path). Reverses one of its
positions: RELEASES.md:34 says bumping the version is "a product-owner decision";
after this change the decision is made by the commits, and the committed
`Cargo.toml` version is explicitly a placeholder.

## Problem

Every merge to `main` already builds and republishes the rolling `latest`
prerelease (dmg + AppImage, ~15 min). But a **versioned** release is a manual
act — `git tag vX.Y.Z && git push` — and the chain broke exactly there: v0.4.1
was cut 2026-07-21, and 30 commits (including new `feat(c3)` work) have landed
since with no versioned release. The Releases page shows a stale "Latest".

Goal: when merged work contains features or fixes, a versioned GitHub Release
appears automatically, with correct semver and a changelog, at zero manual cost.

## Principle

**The git tag is the only source of version truth.** One build per merge; the
publish step decides what to do with the artifacts:

- always: roll the `latest` prerelease (unchanged behavior);
- additionally, when the commits since the last `v*` tag warrant it: publish
  the **same artifacts** as a real Release `vX.Y.Z`, creating the tag.

## 1. `scripts/release-plan.sh` — the decision, out of YAML

A pure program over git history. No network, no side effects. Testable locally
against the real repo and against synthetic repos — unlike YAML, which can only
be exercised by pushing to GitHub. Consistent with the repo's pattern: every
gate is a script in `scripts/`.

```
input:  the checked-out repo (HEAD, tags reachable via fetch-depth: 0)
output (stdout, key=value lines + a changelog file):
  bump=major|minor|patch|none
  last_tag=v0.4.1            (empty if no v* tag exists)
  version=0.5.0              (bump=none -> next-patch + "-dev.<shortsha>", e.g. 0.4.2-dev.27b0168)
  release_tag=v0.5.0         (empty when bump=none)
  changelog file: grouped "### Features / ### Bug fixes / ### Other",
                  one line per commit: scope, subject, short-sha link
```

Rules, applied to `git log <last_tag>..HEAD --no-merges`:

| Commit | Bump |
|---|---|
| type `!` suffix (e.g. `feat(c3)!:`) or `BREAKING CHANGE:` in body | major |
| `feat` | minor |
| `fix`, `perf` | patch |
| `docs`, `test`, `refactor`, `chore`, `ci`, `style`, non-conforming | none |

Highest wins. Merge commits are excluded: PR titles in this repo are narrative
("D6: residue audit…"); the semantics live in the commits inside the PR.

Edge cases the script must define (and the synthetic-repo tests must pin):

- **No `v*` tag reachable** → treat as first-release: version = `0.1.0` if
  any feat/fix exists, else `bump=none`. (Cannot happen in this repo, but the
  script must not crash on a fresh clone.)
- **HEAD already tagged `v*`** (e.g. the manual-tag path ran first) →
  `bump=none`; rolling version derives from that tag.
- **Non-conforming subjects** count as `none`, never fail the run.
- **Pre-releases**: tags like `v0.2.0-rc1` are NOT counted as "last release
  tag"; only bare `vX.Y.Z` anchor the range. (Validation-run tags must not
  eat a real release's commit range.)

## 2. `release.yml` changes

| Job | Change |
|---|---|
| `prepare` | Stops reading `Cargo.toml`. Checkout gains `fetch-depth: 0` (**today it is a shallow clone — without it no tags are visible**). Runs `release-plan.sh`; exports `version`, `release_tag`, `bump`; uploads `changelog.md` as an artifact. Existing outputs (`tag`, `prerelease`, `rolling`) keep their meaning for the rolling leg. |
| `macos-dmg`, `linux-appimage` | **Untouched.** They already consume `needs.prepare.outputs.version`; the fragile, proven build recipes do not change. |
| `publish` | Downloads the changelog artifact. Always publishes the rolling `latest` (delete + recreate, as today). Then, if `release_tag` is non-empty: `gh release create <release_tag> --target $GITHUB_SHA` with `prerelease=false`, uploading the **same files**. Release body gains a "What's changed" section (the changelog) above the existing install caveats. |

The `push: tags: ["v*"]` and `workflow_dispatch` triggers stay as escape
hatches, byte-for-byte the current behavior. Manual tag pushes take precedence
naturally: the auto path sees HEAD already tagged and yields `bump=none`.

## 3. Why this cannot loop or double-build

`gh release create` with the default `GITHUB_TOKEN` creates the tag ref via
API. GitHub does not fire workflows for events created with that token — the
new `v*` tag does **not** retrigger `release.yml`. This is the platform's
anti-recursion rule used as a feature: no `[skip ci]`, no guard to maintain.
It also halves CI minutes versus today's manual flow, where a release built
twice (once on the `main` push, once on the tag push — see runs 29821916058 /
29821936647 for v0.4.1, 16 min each, same commit; macOS runners bill 10×).

Because the version is needed to name the assets (`tauri` bakes it into the
bundle at build time), it is computed in `prepare`, **before** the builds —
never derived afterwards.

## 4. Accepted consequences

- **Versions move fast.** Two feat-PRs merged the same day yield 0.5.0 then
  0.6.0. This is the chosen trade of full automation.
- **A refactor-only merge cuts no release** — it only rolls `latest`. To force
  a release, mark a commit `!` (major) or include a `feat`/`fix`.
- **The rolling version string changes** from `0.3.0-dev.<sha>` (committed
  Cargo.toml) to next-patch-dev, e.g. `0.4.2-dev.<sha>` — it now sorts
  *before* the eventual real 0.4.2 instead of after 0.3.0.
- **`Cargo.toml` stays at `0.3.0`** with a comment: released versions come
  from git tags; this field is a placeholder. Local `cargo` builds report the
  placeholder. No auto-commits to `main`, hence no race between close merges.

## 5. Verification

1. **Real history:** `scripts/release-plan.sh` on this repo must output
   `bump=minor`, `last_tag=v0.4.1`, `version=0.5.0`, and a changelog listing
   the `feat(c3)` commits.
2. **Synthetic repos** (throwaway `git init` fixtures): no tags; docs-only;
   fix-only; feat+fix; breaking `!`; HEAD tagged; rc-tag present but ignored.
3. **Live validation:** a `workflow_dispatch` run producing a deletable
   prerelease (the RELEASES.md:132 pattern), confirming the changelog artifact
   flows into the release body.
4. **First real merge after adoption** must produce `v0.5.0` with the *Latest*
   badge and both assets downloadable via
   `gh release download v0.5.0 --repo AlbyIanna/penpot-desktop`.
5. `docs/RELEASES.md` updated: the automatic path documented, the "product-owner
   decision" paragraph revised, the memory-noted flakiness check ("verify the
   rolling latest actually advanced") extended to the versioned release.
