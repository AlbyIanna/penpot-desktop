#!/usr/bin/env bash
# Reusable web-surface capture for milestone docs (PLAN4 "Definition of done").
#
# Captures our own /__ pages AND Penpot's SPA surfaces at a FIXED 1280px-wide
# viewport into a given out-dir, driving the bundled offline chromium. It boots
# NOTHING — point it at an already-running stack, so a gate can capture mid-run.
#
# NOTE (PLAN4 honest split): this covers WEB surfaces only. Native chrome
# (menu bar, Preferences window, native dialogs) is outside any browser and is
# captured manually from D3 onward; that part is NOT CI-reproducible.
#
# usage: BASE=http://localhost:9046 OUT_DIR=docs/milestones/d1/img \
#          bash scripts/shots.sh home=/__home dashboard=/
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export REPO_ROOT="$ROOT"
BASE="${BASE:?set BASE to a running stack, e.g. http://localhost:9046}"
OUT_DIR="${OUT_DIR:?set OUT_DIR, e.g. docs/milestones/d1/img}"

if [ "$#" -eq 0 ]; then
    echo "usage: BASE=.. OUT_DIR=.. $0 <name=path> [<name=path> ...]" >&2
    exit 2
fi

mkdir -p "$OUT_DIR"
node "$ROOT/scripts/shots_capture.cjs" "$BASE" "$OUT_DIR" "$@"
