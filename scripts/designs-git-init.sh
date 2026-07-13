#!/usr/bin/env bash
# M5: enable git versioning for a Penpot Local designs folder.
#
#   usage: designs-git-init.sh <designs-dir>        (also: `just git-init <dir>`)
#
# Idempotent — safe to run any number of times. What it does:
#   1. `git init` if <designs-dir> is not already a git repo (.git present).
#   2. Writes a .gitignore that ignores ONLY tool-transient noise:
#        .DS_Store, `.penpot-sync.json.tmp-*` (manifest atomic-write temps),
#        `*.penpot.tmp-*/` + `*.penpot.old-*/` (two-phase directory-swap
#        leftovers — cleaned by startup reconciliation, never user data).
#      Deliberately NOT ignored: `*.conflict-*.penpot/` (conflicts must stay
#      visible), `*.exports/` (versioning the rendered boards is the point),
#      and `.penpot-sync.json` (the path↔file-id manifest lets a fresh clone
#      resurrect files under their original Penpot ids).
#   3. Writes DESIGNS-README.md explaining the layout, the git no-overlay
#      lesson, tool-owned .penpot dirs, and conflict-copy semantics.
#      Existing .gitignore / DESIGNS-README.md are never overwritten.
#   4. Makes an initial commit ONLY if step 1 created the repo just now
#      (never commits into a pre-existing repo, not even an empty one).
#
# The desktop app's tray action "Enable git versioning" runs this exact
# script (embedded via include_str! in apps/desktop/src/gitinit.rs).

set -euo pipefail

DESIGNS_DIR="${1:?usage: designs-git-init.sh <designs-dir>}"

if ! command -v git >/dev/null 2>&1; then
    echo "ERROR: git is not installed (or not on PATH)" >&2
    exit 1
fi
if [ ! -d "$DESIGNS_DIR" ]; then
    echo "ERROR: designs dir does not exist: $DESIGNS_DIR" >&2
    exit 1
fi
cd "$DESIGNS_DIR"

# --- 1. init (only when not already a repo) -----------------------------------
FRESH_REPO=0
if [ -e .git ]; then
    echo "git: already a repository (.git exists) — leaving history untouched"
else
    git init --quiet
    FRESH_REPO=1
    echo "git: initialized empty repository in $DESIGNS_DIR/.git"
fi

# --- 2. .gitignore (create once, never clobber user edits) --------------------
if [ -f .gitignore ]; then
    echo ".gitignore: already exists — not touching it"
else
    cat > .gitignore <<'EOF'
# Penpot Local — ignore ONLY tool-transient noise. Everything else in this
# tree is your data and should be versioned, including:
#   *.penpot/                 design sources (tool-owned, still versioned)
#   *.exports/                auto-rendered SVG/PNG boards (tracked on purpose)
#   *.conflict-*.penpot/      conflict copies (tracked: conflicts stay visible)
#   .penpot-sync.json         path <-> Penpot-file-id manifest (lets a fresh
#                             clone restore files under their original ids)

.DS_Store

# sync manifest atomic-write temp files
.penpot-sync.json.tmp-*

# two-phase directory-swap leftovers (cleaned at startup, never user data)
*.penpot.tmp-*/
*.penpot.old-*/
EOF
    echo ".gitignore: written"
fi

# --- 3. DESIGNS-README.md (create once, never clobber user edits) -------------
if [ -f DESIGNS-README.md ]; then
    echo "DESIGNS-README.md: already exists — not touching it"
else
    cat > DESIGNS-README.md <<'EOF'
# Your Penpot designs folder

This folder tree is the **source of truth** for Penpot Local. The Penpot
database is a disposable cache: delete it, restart the app, and everything
is rebuilt from these files.

## Layout

    <project>/                    each top-level folder is a Penpot project
      <file>.penpot/              one Penpot file (unzipped .penpot archive):
        manifest.json             binfile manifest
        files/<id>.json           file data; per-page JSON under files/<id>/pages/
        objects/                  media metadata + binary blobs (images/fonts)
      <file>.exports/             auto-rendered SVG/PNG per board
      <file>.conflict-<ts>.penpot/  a conflict copy (see below)
    .penpot-sync.json             sync manifest: path <-> Penpot file id

## Reverting with git — the no-overlay lesson

Plain `git checkout <commit> -- <dir>` runs in **overlay mode**: it does NOT
delete files added since that commit. Reverting a design that gained shapes
this way silently imports a merged tree (old files + the new pages/shapes).
Use one of these instead — they also *delete* files that didn't exist then:

    git checkout --no-overlay <commit> -- <file>.penpot/
    git restore --source=<commit> --worktree --staged <file>.penpot/

(`git restore` defaults to no-overlay mode, which is why it is safe.)

## .penpot directories are tool-owned

The sync daemon regenerates `.penpot` directories wholesale on every export.
A stray file you drop inside one triggers an import and is then silently
swept away by the next DB→FS export swap. Keep your own files OUTSIDE
`.penpot` directories (anywhere else in this tree is fine).

## Conflict copies

If both Penpot (the app/database) and this folder changed since the last
sync, the tool never silently overwrites either side: the on-disk version
wins (this folder is the source of truth), and the database version is saved
next to the file as `<name>.conflict-<timestamp>.penpot/`. Conflict copies
are never watched, synced, or auto-deleted — inspect them, merge what you
need, then delete them yourself. They are tracked by git so conflicts stay
visible in `git status`.
EOF
    echo "DESIGNS-README.md: written"
fi

# --- 4. initial commit (fresh repos only) --------------------------------------
if [ "$FRESH_REPO" -eq 1 ]; then
    # Fall back to a local identity so the commit works on machines without
    # a global git identity configured (never overrides an existing one).
    IDENT_ARGS=()
    git config user.email >/dev/null 2>&1 || IDENT_ARGS+=(-c user.email=penpot-local@localhost)
    git config user.name >/dev/null 2>&1 || IDENT_ARGS+=(-c user.name="Penpot Local")
    git add -A
    if git "${IDENT_ARGS[@]+"${IDENT_ARGS[@]}"}" commit --quiet -m "Initial commit: Penpot Local designs"; then
        echo "git: initial commit created"
    else
        echo "WARNING: initial commit failed (repo left initialized, files staged)" >&2
    fi
else
    echo "git: pre-existing repository — no commit made (that's yours to do)"
fi

echo "OK: git versioning is set up in $DESIGNS_DIR"
