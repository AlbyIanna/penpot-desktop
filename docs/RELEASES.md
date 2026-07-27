# Releasing Penpot Local

The distribution pipeline is `.github/workflows/release.yml`. It builds the two
user-installable artifacts and attaches them to a **GitHub Release**:

| Platform | Asset name |
|---|---|
| macOS 15.4+ (Apple Silicon / arm64) | `Penpot-Local_<version>_macos-arm64.dmg` |
| Linux (x86_64) | `Penpot-Local_<version>_linux-x86_64.AppImage` |

The build recipes are the exact, already-proven steps from
`.github/workflows/package.yml` (the `macos-dmg` and `linux-appimage` jobs):
runtime-bundle build → slim `tauri build` → (Linux) post-`linuxdeploy` inject of
`penpot-runtime/` + `appimagetool` repack. `package.yml` is left untouched and
keeps producing its expiring workflow artifacts; `release.yml` is the only path
that creates durable Release downloads.

## Private-repo reality (today)

The repository is **private**. GitHub Release assets on a private repo are
downloadable **only by authenticated users who have access to the repo**. There
is no anonymous download URL. Collaborators fetch assets with the `gh` CLI (it
carries their auth) or from the Releases page in the browser while signed in.

The pipeline needs **zero changes** if the repo is later made public — the same
Release assets simply become anonymously downloadable and `gh release download`
keeps working unchanged. Nothing in the workflow assumes private-vs-public.

## How to cut a release

The version is **derived from git tags** — never hardcoded in the workflow or
in `Cargo.toml`. There are two paths:

### Automatic (the normal path)

Every merge to `main` runs `scripts/release-plan.sh` (gated by
`scripts/release-plan-test.sh`, chained into `just e2e`), which reads the
conventional commits since the last bare `vX.Y.Z` tag:

| Commits since the last release tag | Bump |
|---|---|
| a `!` type suffix (`feat(api)!:`) or a `BREAKING CHANGE:` footer | major |
| at least one `feat` | minor |
| only `fix` / `perf` | patch |
| only `docs`/`test`/`refactor`/`chore`/`ci`/non-conforming | none — the rolling `latest` still updates |

When a release is warranted, the `publish` job publishes the **same
artifacts** it just attached to `latest` as a real Release `vX.Y.Z` (with the
*Latest* badge), creating the tag with the default `GITHUB_TOKEN` — which
GitHub never retriggers workflows for, so there is no loop and no second
build. The release body carries a generated "What's changed" changelog grouped
from the same commits. Two deliberate exclusions: **merge commits are
ignored** (PR titles here are narrative; the semantics live in the commits
inside the PR), and **`rc`/suffixed tags** (e.g. `v0.2.0-rc1`) never anchor
the commit range — a validation run must not eat a real release's changelog.

Consequence to know: versions move fast (two feat-PRs merged the same day
yield e.g. 0.5.0 then 0.6.0), and a refactor-only chapter cuts no release
unless a commit is marked `!` or carries a `feat`/`fix`.

### Manual (the escape hatch)

Push a tag yourself — unchanged, and it takes precedence naturally (the auto
path sees HEAD already tagged and stands down):

1. Push a tag:

   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```

   The `push: tags: ["v*"]` trigger fires. Both artifacts build in parallel; the
   `publish` job creates a Release for tag `v0.2.0` (not a prerelease) and
   attaches the dmg + AppImage.

2. The Release appears at
   `https://github.com/AlbyIanna/penpot-desktop/releases/tag/v0.2.0`.

The committed workspace version in `Cargo.toml` is a **placeholder** (it stays
at its historical value): the released version comes from the git tag, patched
into the runner's `Cargo.toml` at build time (ephemeral, never committed).
Local `cargo` builds report the placeholder — that is expected.

### Rolling "latest" builds from `main` (automatic)

Every push to `main` (i.e. every merge) automatically republishes a single
**rolling prerelease** so there are always-current downloadables without minting
a permanent version:

- Trigger: `push: branches: ["main"]`.
- Version: **next-patch-dev**, e.g. `0.4.2-dev.84148b6` — derived by
  `scripts/release-plan.sh` from the last release tag (+ short commit sha) so
  it sorts *before* the eventual real `0.4.2`, no longer read from the
  committed `Cargo.toml`.
- Tag: `latest` — the `publish` job **deletes and recreates** it each run so the
  tag tracks the newest `main` commit (a plain `gh release edit` cannot move a tag).
- Marked as a **prerelease**, with a "Rolling build" note in the body.
- Stable URL: `https://github.com/AlbyIanna/penpot-desktop/releases/tag/latest`.
- After a merge lands, verify not only that `latest` actually advanced but —
  when the commits warranted a release — that the new `vX.Y.Z` Release exists
  with both assets (the dmg CI step is known-flaky).

Scope: the rolling build only **builds + publishes** the artifacts — the code was
already gated before it reached `main` (local `just e2e`); it does **not** re-run
the milestone suites (they need Docker + ~2 h). Each run is ~20 min of macOS +
Linux runner time. For a **pinned, stable** release, push a `v*` tag (above) —
that path is unchanged and takes precedence over the rolling one.

### Validation / prerelease runs without a permanent tag

`release.yml` is also `workflow_dispatch`-able, with inputs:

- `version` (string, e.g. `0.2.0-rc1`) — used for asset names and the throwaway tag.
- `prerelease` (boolean, default `true`) — marks the Release as a prerelease.

A dispatch run builds the same artifacts and cuts a Release under tag
`v<version>`, creating the tag ref pointing at the dispatched commit. Because the
`publish` job uses the `gh` CLI, both the Release and its tag are fully
**deletable** afterward — nothing is permanent. This is how the pipeline is
validated without minting a real release tag.

## Downloading a release

Authenticated user with repo access, using the GitHub CLI:

```sh
# Download both assets for v0.2.0 into the current directory:
gh release download v0.2.0 --repo AlbyIanna/penpot-desktop

# Or just one platform:
gh release download v0.2.0 --repo AlbyIanna/penpot-desktop \
  --pattern '*_macos-arm64.dmg'
gh release download v0.2.0 --repo AlbyIanna/penpot-desktop \
  --pattern '*_linux-x86_64.AppImage'
```

Once the repo is public these same commands work for anyone, and the browser
Releases page offers direct download links.

## Install caveats (also in the generated Release notes)

### macOS (Apple Silicon / arm64)

- Requires **macOS 15.4 or newer** on an **Apple Silicon** Mac. Intel Macs are
  not supported (the bundled theseus Postgres links `_strchrnul`, libSystem ≥
  15.4; `minimumSystemVersion` is `15.4`).
- The dmg is **ad-hoc signed** (no Developer ID, not notarized). On first launch
  Gatekeeper blocks a double-click of the downloaded app. **Right-click the app →
  Open**, then confirm — or clear quarantine:
  `xattr -d com.apple.quarantine "/Applications/Penpot Local.app"`. Later
  launches open normally.

### Linux (x86_64)

- **x86_64 only** (no arm64 AppImage is built).
- Requires **FUSE** to run the AppImage: `sudo apt install libfuse2` on
  Debian/Ubuntu, or run with `--appimage-extract-and-run`.
- The injected runtime relies on a small set of **host system libraries** per the
  AppImage excludelist convention (fontconfig / X11 / glib for ImageMagick,
  krb5 / libxml2 for Postgres). Present on normal desktop distros; absent in
  minimal containers (see `docs/milestones/m4.md`, "Honest CI-coverage notes").

Both artifacts run fully offline: embedded Postgres, Valkey, the backend JVM,
the proxy, and the packaged exporter ship inside the artifact.

## For the ReleaseCI validation phase

`release.yml` cannot be executed from a local dev machine — GitHub Actions runs
only on GitHub's runners. Validate it live with a `workflow_dispatch` run that
produces a deletable prerelease:

```sh
# 1. Trigger a validation build (deletable prerelease under tag v0.2.0-rc1):
gh workflow run release.yml \
  --repo AlbyIanna/penpot-desktop \
  --ref ci/n3-release \
  -f version=0.2.0-rc1 \
  -f prerelease=true

# 2. Watch it to completion:
gh run watch --repo AlbyIanna/penpot-desktop \
  "$(gh run list --repo AlbyIanna/penpot-desktop \
       --workflow release.yml --limit 1 --json databaseId \
       --jq '.[0].databaseId')"

# 3. Prove the assets are downloadable from the Release:
gh release view v0.2.0-rc1 --repo AlbyIanna/penpot-desktop
gh release download v0.2.0-rc1 --repo AlbyIanna/penpot-desktop \
  --dir /tmp/rel-check
ls -la /tmp/rel-check   # expect the dmg AND the AppImage, non-empty

# 4. Tear down the throwaway release AND its tag:
gh release delete v0.2.0-rc1 --repo AlbyIanna/penpot-desktop --yes --cleanup-tag
```

Assumptions the ReleaseCI phase must satisfy: the `ci/n3-release` branch carries
this `release.yml` (dispatch reads the workflow file from `--ref`); the actor
has `contents: write` on the repo; and the macOS 15 + Ubuntu 24.04 runners are
available (same as `package.yml`). Expect ~9 min per platform build (warm caches)
before `publish` runs.

> **Platform caveat — `workflow_dispatch` needs the workflow on the default
> branch.** GitHub only registers a workflow's `workflow_dispatch` trigger once
> the workflow file exists on the repository's **default branch** (`main`).
> Before `release.yml` is merged to `main`, `gh workflow run release.yml --ref
> ci/n3-release` returns `HTTP 404: workflow release.yml not found on the default
> branch`, even though the file is present on the branch. This is a platform
> constraint, not a workflow bug. Two consequences:
>
> - **Pre-merge validation** (what the ReleaseCI phase does from `ci/n3-release`)
>   is driven by the **push-tag path** instead, which reads the workflow from the
>   pushed tag's own tree and needs no default-branch registration:
>
>   ```sh
>   git tag v0.2.0-rc1 ci/n3-release && git push origin v0.2.0-rc1
>   # ... watch the run, prove downloads, then tear down:
>   gh release delete v0.2.0-rc1 --repo AlbyIanna/penpot-desktop --yes --cleanup-tag
>   git push origin :refs/tags/v0.2.0-rc1   # delete the tag ref if it lingers
>   ```
>
>   The push-tag path resolves `prerelease=false` (a real, but still deletable,
>   Release). That is fine for validation — the whole thing is torn down after.
> - **After `release.yml` is merged to `main`**, the `gh workflow run ... -f
>   version=... -f prerelease=true` dispatch commands above work exactly as
>   written. Real releases are always cut by pushing a `vX.Y.Z` tag, which never
>   depended on dispatch registration.
