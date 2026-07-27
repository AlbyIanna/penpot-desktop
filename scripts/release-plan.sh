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
HEAD_TAG="$(git tag --points-at HEAD --list 'v*' | { grep -E "$BARE" || true; } | sort -V | tail -1)"
LAST_TAG="$(git tag --merged HEAD --list 'v*' | { grep -E "$BARE" || true; } \
              | { if [ -n "$HEAD_TAG" ]; then grep -vxF "$HEAD_TAG" || true; else cat; fi; } \
              | sort -V | tail -1)"
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
