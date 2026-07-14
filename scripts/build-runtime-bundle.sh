#!/usr/bin/env bash
#
# build-runtime-bundle.sh — produce the self-contained `penpot-runtime/` bundle
# (macOS arm64) that the packaged desktop app ships in its Tauri resources dir.
#
# Bundle layout (the M4 contract; the app's layout resolver consumes this):
#
#   penpot-runtime/
#     backend/            penpot.jar, log4j2.xml, builtin-templates/, scripts/, run.sh,
#                         version.txt  (from penpotapp/backend:$PENPOT_VERSION /opt/penpot/backend)
#     frontend/           static SPA   (from penpotapp/frontend:$PENPOT_VERSION /var/www/app)
#     jre/                jlink-minimized JRE (bin/java) built from $JLINK_HOME with
#                         EXACTLY the module set of the upstream backend image's runtime
#                         (∪ whatever jdeps demands of penpot.jar — verified a subset)
#     bin/
#       valkey-server     relocatable (openssl dylibs in bin/lib/, @loader_path install names)
#       identify          wrapper script -> bin/im/identify with bundled dylib closure,
#                         coder modules and config (fully relocatable ImageMagick)
#       penpot-watchdog   the SIGKILL orphan reaper (cargo build --release from this repo)
#       node              official node v$NODE_VERSION arm64 binary (sha256-pinned
#                         tarball; the upstream exporter image pins the same major) —
#                         runs the exporter child in packaged mode (N2)
#       lib/, im/         support payload for the above (not on PATH themselves)
#     postgres/
#       $POSTGRES_VERSION/   ready postgresql_embedded-compatible installation
#                            (theseus-rs/postgresql-binaries layout: bin/ lib/ share/).
#                            Pre-seeding this is what makes the packaged app's FIRST
#                            boot fully offline (postgresql_embedded otherwise hits
#                            the GitHub API + downloads on first run).
#     exporter/           the extracted penpot-exporter app (app.js + node_modules,
#                         pure JS — penpotapp/exporter:$PENPOT_VERSION, same tree
#                         scripts/fetch-penpot.sh materializes into runtime/exporter)
#     exporter-browsers/  playwright-managed chromium HEADLESS SHELL only (the full
#                         chromium + ffmpeg stay out: the exporter launches
#                         `chromium.launch({headless})`, which playwright ≥1.49
#                         serves from the headless shell build — proven in P10)
#     licenses/           license texts for every bundled component
#     MANIFEST.json       component versions + licenses index
#     VERSION             the pinned Penpot tag (same semantics as runtime/VERSION)
#
# node history: M4 shipped WITHOUT node after proving the backend never execs it
# (SVG media upload works with node hidden — that proof still holds and the
# backend PATH story is unchanged). N2 adds node back exclusively as the
# exporter's runtime (PLAN2.md risk 1, DECIDED option (a): package the exporter
# as the render path).
#
# Sources:
#   backend/frontend  docker create/cp from the pinned images (default), or
#                     `--no-docker`: a pinned static `crane` binary (google/
#                     go-containerregistry) downloads + flattens the images
#                     straight from Docker Hub — for CI macOS runners without
#                     a docker daemon.
#   jre               $JLINK_HOME (default /opt/homebrew/opt/openjdk, must be JDK 26)
#   valkey/imagemagick  homebrew (made relocatable here)
#   postgres          ~/.cache/penpot-local/pg-install (populated by the test suites)
#                     or a one-time download of the same theseus archive
#   penpot-watchdog   cargo build --release -p supervisor --bin penpot-watchdog
#                     (override with $PENPOT_WATCHDOG_BIN_SRC to skip the build, e.g.
#                     a CI artifact)
#
# Idempotent: a completed bundle records a fingerprint (component versions);
# when it matches and the key artifacts exist, the build is skipped (--force to
# rebuild). The PROVE verification suite ALWAYS runs, on skip too:
#   P1  jre/bin/java boots penpot.jar's Clojure runtime (namespace-miss sanity check)
#   P2  jre/bin/java config-error-level boot: nonexistent DB URI reaches the
#       'initialize connection pool' backend log line
#   P3  bin/valkey-server from a scratch dir with env -i: --version + live PING/PONG
#   P4  bin/identify from a scratch dir with env -i: identifies a generated PNG + SVG
#   P5  postgres/: initdb + pg_ctl start + pg_isready + stop with env -i
#   P6  relocation audit: no Mach-O under bin/ references /opt/homebrew or /usr/local
#   P7  (best effort) P3/P4 again under sandbox-exec denying ALL /opt/homebrew reads
#   P8  bin/node from a scratch dir with env -i reports exactly v$NODE_VERSION
#   P9  exporter app boots on the BUNDLED node (own valkey, env -i, poisoned
#       proxies) and answers GET /readyz 200
#   P10 the bundled playwright launches the bundled HEADLESS SHELL (no full
#       chromium anywhere) and evaluates JS in a page — proves the shell-only
#       browsers dir is sufficient for renders
#
# Usage: scripts/build-runtime-bundle.sh [--dest DIR] [--force] [--no-docker]
# Env:   PENPOT_VERSION (2.16.2), JLINK_HOME, VALKEY_BIN, IDENTIFY_BIN,
#        PG_INSTALL_CACHE, PENPOT_WATCHDOG_BIN_SRC,
#        BUNDLE_TEST_VALKEY_PORT (6414), BUNDLE_TEST_PG_PORT (5466),
#        BUNDLE_TEST_EXPORTER_PORT (6473)

set -euo pipefail

# ---------------------------------------------------------------------------
# Pins & configuration
# ---------------------------------------------------------------------------
PENPOT_VERSION="${PENPOT_VERSION:-2.16.2}"
BACKEND_IMAGE="penpotapp/backend:${PENPOT_VERSION}"
FRONTEND_IMAGE="penpotapp/frontend:${PENPOT_VERSION}"
EXPORTER_IMAGE="penpotapp/exporter:${PENPOT_VERSION}"

# node for the exporter child (N2): the upstream exporter image pins v24.16.0;
# official nodejs.org build, sha256 from SHASUMS256.txt (verified 2026-07-14).
NODE_VERSION="24.16.0"
NODE_URL="https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-darwin-arm64.tar.gz"
NODE_SHA256="39189dab4eeb15706c424af0ac08a3044c9e48f7db12a7d77f6b7aafc7dd5df6"

# Embedded postgres: MUST match crates/supervisor DEFAULT_POSTGRES_VERSION.
POSTGRES_VERSION="15.18.0"
PG_ARCHIVE_URL="https://github.com/theseus-rs/postgresql-binaries/releases/download/${POSTGRES_VERSION}/postgresql-${POSTGRES_VERSION}-aarch64-apple-darwin.tar.gz"

# JDK: the jar is built with --enable-preview on JDK 26; major must match EXACTLY.
JLINK_HOME="${JLINK_HOME:-/opt/homebrew/opt/openjdk}"
EXPECTED_JDK_MAJOR=26

# Module set of the upstream backend image's bundled JRE. Derived 2026-07-13 via:
#   docker run --rm --entrypoint sh penpotapp/backend:2.16.2 -c "java --list-modules"
# When docker is available the list is re-derived live and MUST match this pin
# (fail loudly on drift — updating the pin is a deliberate step). jdeps on
# penpot.jar (java.base,java.logging,jdk.unsupported) is verified to be a subset.
EXPECTED_MODULES="java.base,java.compiler,java.datatransfer,java.desktop,java.instrument,java.logging,java.management,java.management.rmi,java.naming,java.net.http,java.prefs,java.rmi,java.scripting,java.se,java.security.jgss,java.security.sasl,java.sql,java.sql.rowset,java.transaction.xa,java.xml,java.xml.crypto,jdk.attach,jdk.compiler,jdk.internal.jvmstat,jdk.internal.md,jdk.internal.opt,jdk.javadoc,jdk.jcmd,jdk.jfr,jdk.management.agent,jdk.net,jdk.unsupported,jdk.zipfs"

# crane (--no-docker path): pinned static binary from google/go-containerregistry.
CRANE_VERSION="v0.20.2"
CRANE_URL="https://github.com/google/go-containerregistry/releases/download/${CRANE_VERSION}/go-containerregistry_Darwin_arm64.tar.gz"
CRANE_SHA256="b47a8291d1069656bcfb8346dc9494f03e734d7a4058961fa53f0dfc9cb41abb"

VALKEY_BIN="${VALKEY_BIN:-/opt/homebrew/bin/valkey-server}"
IDENTIFY_BIN="${IDENTIFY_BIN:-/opt/homebrew/bin/identify}"
MAGICK_BIN="${MAGICK_BIN:-/opt/homebrew/bin/magick}"
PG_INSTALL_CACHE="${PG_INSTALL_CACHE:-$HOME/.cache/penpot-local/pg-install}"

TEST_VALKEY_PORT="${BUNDLE_TEST_VALKEY_PORT:-6414}"
TEST_PG_PORT="${BUNDLE_TEST_PG_PORT:-5466}"
TEST_EXPORTER_PORT="${BUNDLE_TEST_EXPORTER_PORT:-6473}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${REPO_ROOT}/dist/penpot-runtime"
FORCE=0
NO_DOCKER=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dest) DEST="$2"; shift 2 ;;
    --dest=*) DEST="${1#--dest=}"; shift ;;
    --force) FORCE=1; shift ;;
    --no-docker) NO_DOCKER=1; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
mkdir -p "$(dirname "$DEST")"
DEST="$(cd "$(dirname "$DEST")" && pwd)/$(basename "$DEST")"

log()  { echo "[build-runtime-bundle] $*"; }
die()  { echo "[build-runtime-bundle] ERROR: $*" >&2; exit 1; }

[[ "$(uname -s)/$(uname -m)" == "Darwin/arm64" ]] \
  || die "this script currently targets macOS arm64 only (got $(uname -s)/$(uname -m))"
command -v python3 >/dev/null || die "python3 is required"
command -v curl    >/dev/null || die "curl is required"

CACHE_DIR="$(dirname "$DEST")/.cache"
mkdir -p "$CACHE_DIR"

# ---------------------------------------------------------------------------
# Fingerprint / idempotency
# ---------------------------------------------------------------------------
JAVA_VERSION="$("$JLINK_HOME/bin/java" --version 2>/dev/null | head -1 | awk '{print $2}')" \
  || die "no java at $JLINK_HOME/bin/java (set JLINK_HOME)"
[[ "${JAVA_VERSION%%.*}" == "$EXPECTED_JDK_MAJOR" ]] \
  || die "JDK at $JLINK_HOME is $JAVA_VERSION; the pinned Penpot jar needs major $EXPECTED_JDK_MAJOR exactly (--enable-preview)"
[[ -x "$JLINK_HOME/bin/jlink" ]] || die "no jlink at $JLINK_HOME/bin/jlink"

VALKEY_VERSION="$("$VALKEY_BIN" --version | sed -n 's/.*v=\([^ ]*\).*/\1/p')" \
  || die "cannot run $VALKEY_BIN (set VALKEY_BIN)"
[[ -n "$VALKEY_VERSION" ]] || die "cannot parse valkey version from $VALKEY_BIN --version"

IDENTIFY_REAL="$(python3 -c 'import os,sys;print(os.path.realpath(sys.argv[1]))' "$IDENTIFY_BIN")"
[[ -f "$IDENTIFY_REAL" ]] || die "identify not found at $IDENTIFY_BIN (set IDENTIFY_BIN)"
# /opt/homebrew/Cellar/imagemagick/<version>/bin/identify -> prefix + version
IM_PREFIX="$(dirname "$(dirname "$IDENTIFY_REAL")")"
IM_VERSION="$(basename "$IM_PREFIX")"
[[ -d "$IM_PREFIX/lib/ImageMagick" ]] \
  || die "cannot locate the ImageMagick keg from $IDENTIFY_REAL (expected .../Cellar/imagemagick/<ver>/bin/identify)"

FINGERPRINT="layout=2 penpot=${PENPOT_VERSION} jdk=${JAVA_VERSION} valkey=${VALKEY_VERSION} imagemagick=${IM_VERSION} postgres=${POSTGRES_VERSION} node=${NODE_VERSION} exporter=${PENPOT_VERSION}-hoisted browsers=headless-shell-only"

# The headless-shell payload dir (chromium_headless_shell-<rev>/…), if present.
bundle_headless_shell() { # bundle_headless_shell <bundle-root>
  compgen -G "$1/exporter-browsers/chromium_headless_shell-*/chrome-headless-shell-mac-arm64/chrome-headless-shell" >/dev/null 2>&1
}

bundle_is_current() {
  [[ -f "$DEST/.fingerprint" ]] \
    && [[ "$(cat "$DEST/.fingerprint")" == "$FINGERPRINT" ]] \
    && [[ -s "$DEST/backend/penpot.jar" ]] \
    && [[ -f "$DEST/frontend/index.html" ]] \
    && [[ -x "$DEST/jre/bin/java" ]] \
    && [[ -x "$DEST/bin/valkey-server" ]] \
    && [[ -x "$DEST/bin/identify" ]] \
    && [[ -x "$DEST/bin/penpot-watchdog" ]] \
    && [[ -x "$DEST/bin/node" ]] \
    && [[ -x "$DEST/postgres/$POSTGRES_VERSION/bin/initdb" ]] \
    && [[ -s "$DEST/exporter/app.js" ]] \
    && [[ -d "$DEST/exporter/node_modules" ]] \
    && [[ -d "$DEST/exporter/node_modules/date-fns" ]] \
    && bundle_headless_shell "$DEST" \
    && [[ -f "$DEST/MANIFEST.json" ]] \
    && [[ -f "$DEST/VERSION" ]]
}

# ---------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------

# fetch_verified <url> <sha256|-> <out>   (sha256 '-' = skip pin, integrity via
# a .sha256 side file when the caller arranges it)
fetch_verified() {
  local url="$1" sha="$2" out="$3"
  if [[ -f "$out" ]]; then
    if [[ "$sha" == "-" ]] || echo "$sha  $out" | shasum -a 256 -c - >/dev/null 2>&1; then
      return 0
    fi
    rm -f "$out"
  fi
  log "downloading $(basename "$out") from $url ..."
  curl -fsSL --retry 3 -o "$out.part" "$url" || die "download failed: $url"
  if [[ "$sha" != "-" ]]; then
    echo "$sha  $out.part" | shasum -a 256 -c - >/dev/null \
      || die "sha256 mismatch for $url (expected $sha, got $(shasum -a 256 "$out.part" | awk '{print $1}'))"
  fi
  mv "$out.part" "$out"
}

# Make the set of Mach-O files under a root relocatable: copy the recursive
# non-system dylib closure into <root>/<libsub>, rewrite install names to
# @loader_path-relative, re-sign (mandatory on arm64 after install_name_tool),
# and print 'formula<TAB>keg-path' lines for every homebrew keg that
# contributed a dylib (for license collection).
relocate_machos() { # relocate_machos <root> <libsub>
  python3 - "$@" <<'PYEOF'
import os, subprocess, sys, shutil

root, libsub = sys.argv[1], sys.argv[2]
libdir = os.path.join(root, libsub)
os.makedirs(libdir, exist_ok=True)

def deps(p):
    out = subprocess.run(["otool", "-L", p], capture_output=True, text=True).stdout.splitlines()
    # first line is the file's own name; for dylibs the second line repeats the id
    return [l.split()[0] for l in out[1:] if l.strip()]

def is_local(d):
    return d.startswith("/opt/homebrew") or d.startswith("/usr/local")

def is_macho(p):
    try:
        with open(p, "rb") as f:
            return f.read(4) in (b"\xcf\xfa\xed\xfe", b"\xca\xfe\xba\xbe")
    except OSError:
        return False

machos = []
for dp, _, fns in os.walk(root):
    if os.path.realpath(dp) == os.path.realpath(libdir):
        continue
    for fn in fns:
        p = os.path.join(dp, fn)
        if is_macho(p):
            machos.append(p)

copied, kegs = {}, {}
queue = list(machos)
while queue:
    p = queue.pop()
    for d in deps(p):
        if not is_local(d):
            continue
        real = os.path.realpath(d)
        base = os.path.basename(d)
        if base in copied:
            if copied[base] != real:
                sys.exit(f"FATAL: dylib basename clash: {base} from {real} and {copied[base]}")
            continue
        tgt = os.path.join(libdir, base)
        shutil.copy2(real, tgt)
        os.chmod(tgt, 0o755)
        copied[base] = real
        queue.append(tgt)
        # /opt/homebrew/Cellar/<formula>/<ver>/... -> keg for license collection
        parts = real.split(os.sep)
        if "Cellar" in parts:
            i = parts.index("Cellar")
            if len(parts) > i + 2:
                kegs["/".join([""] + parts[1:i + 3])] = parts[i + 1]

def fix(p, prefix, set_id):
    args = []
    if set_id:
        args += ["-id", "@loader_path/" + os.path.basename(p)]
    for d in deps(p):
        if is_local(d):
            args += ["-change", d, prefix + os.path.basename(d)]
    if args:
        r = subprocess.run(["install_name_tool"] + args + [p], capture_output=True, text=True)
        if r.returncode != 0:
            sys.exit(f"FATAL: install_name_tool failed on {p}: {r.stderr}")
    subprocess.run(["codesign", "-f", "-s", "-", p], capture_output=True)

for fn in os.listdir(libdir):
    p = os.path.join(libdir, fn)
    if is_macho(p):
        fix(p, "@loader_path/", set_id=True)

for p in machos:
    rel = os.path.relpath(libdir, os.path.dirname(p))
    prefix = ("@loader_path/" + rel + "/").replace("/./", "/")
    fix(p, prefix, set_id=p.endswith((".so", ".dylib")))

for keg, formula in sorted(kegs.items()):
    print(f"{formula}\t{keg}")
PYEOF
}

# return 0 iff no Mach-O under the root references /opt/homebrew or /usr/local
# (offenders are printed to stderr)
audit_no_local_refs() { # audit_no_local_refs <root>
  python3 - "$1" <<'PYEOF'
import os, subprocess, sys
root, bad = sys.argv[1], []
for dp, _, fns in os.walk(root):
    for fn in fns:
        p = os.path.join(dp, fn)
        try:
            with open(p, "rb") as f:
                if f.read(4) not in (b"\xcf\xfa\xed\xfe", b"\xca\xfe\xba\xbe"):
                    continue
        except OSError:
            continue
        out = subprocess.run(["otool", "-L", p], capture_output=True, text=True).stdout
        for line in out.splitlines()[1:]:
            d = line.split()[0] if line.strip() else ""
            if d.startswith("/opt/homebrew") or d.startswith("/usr/local"):
                bad.append(f"{p}: {d}")
for b in bad:
    print(b, file=sys.stderr)
sys.exit(1 if bad else 0)
PYEOF
}

# ---------------------------------------------------------------------------
# Build (skipped when the fingerprint matches)
# ---------------------------------------------------------------------------
BUNDLE="$DEST"
STAGING=""
CIDS=()
cleanup() {
  for cid in "${CIDS[@]-}"; do
    [[ -n "$cid" ]] && docker rm -f "$cid" >/dev/null 2>&1 || true
  done
  [[ -n "$STAGING" && -d "$STAGING" ]] && rm -rf "$STAGING"
  # The trap runs under `set -e`: never let the last AND-list's failure
  # (empty $STAGING on the skip path) turn a green run into exit 1.
  return 0
}
trap cleanup EXIT

if [[ "$FORCE" -eq 0 ]] && bundle_is_current; then
  log "bundle at $DEST already matches fingerprint — skipping build (use --force to rebuild)"
  log "fingerprint: $FINGERPRINT"
else
  mkdir -p "$(dirname "$DEST")"
  STAGING="$(mktemp -d "$(dirname "$DEST")/.staging.XXXXXX")"
  BUNDLE="$STAGING"
  log "building into staging $STAGING"
  log "fingerprint: $FINGERPRINT"

  # ----- 1. backend/ + frontend/ -------------------------------------------
  if [[ "$NO_DOCKER" -eq 0 ]]; then
    command -v docker >/dev/null || die "docker CLI not found (use --no-docker for the crane path)"
    docker info >/dev/null 2>&1 || die "docker daemon is not running (use --no-docker for the crane path)"

    log "extracting backend from ${BACKEND_IMAGE} (docker create/cp) ..."
    CID="$(docker create "$BACKEND_IMAGE")"; CIDS+=("$CID")
    docker cp -q "$CID:/opt/penpot/backend" "$STAGING/backend"
    docker rm "$CID" >/dev/null; CIDS=()

    log "extracting frontend from ${FRONTEND_IMAGE} (docker create/cp) ..."
    CID="$(docker create "$FRONTEND_IMAGE")"; CIDS+=("$CID")
    docker cp -q "$CID:/var/www/app" "$STAGING/frontend"
    docker rm "$CID" >/dev/null; CIDS=()
  else
    # CI path: pinned static crane binary flattens the images from Docker Hub.
    CRANE="$CACHE_DIR/crane-$CRANE_VERSION"
    if [[ ! -x "$CRANE" ]]; then
      fetch_verified "$CRANE_URL" "$CRANE_SHA256" "$CACHE_DIR/crane.tar.gz"
      tar -xzf "$CACHE_DIR/crane.tar.gz" -C "$CACHE_DIR" crane
      mv "$CACHE_DIR/crane" "$CRANE"
      rm -f "$CACHE_DIR/crane.tar.gz"
    fi
    log "exporting ${BACKEND_IMAGE} via crane (no docker daemon) ..."
    mkdir -p "$STAGING/.backend-rootfs"
    "$CRANE" export "$BACKEND_IMAGE" - --platform linux/arm64 \
      | tar -xf - -C "$STAGING/.backend-rootfs" opt/penpot/backend \
      || die "crane export of $BACKEND_IMAGE failed"
    mv "$STAGING/.backend-rootfs/opt/penpot/backend" "$STAGING/backend"
    rm -rf "$STAGING/.backend-rootfs"

    log "exporting ${FRONTEND_IMAGE} via crane (no docker daemon) ..."
    mkdir -p "$STAGING/.frontend-rootfs"
    "$CRANE" export "$FRONTEND_IMAGE" - --platform linux/arm64 \
      | tar -xf - -C "$STAGING/.frontend-rootfs" var/www/app \
      || die "crane export of $FRONTEND_IMAGE failed"
    mv "$STAGING/.frontend-rootfs/var/www/app" "$STAGING/frontend"
    rm -rf "$STAGING/.frontend-rootfs"
  fi

  [[ -s "$STAGING/backend/penpot.jar" ]] || die "backend extraction produced no penpot.jar"
  [[ -f "$STAGING/frontend/index.html" ]] || die "frontend extraction produced no index.html"
  JAR_SIZE="$(wc -c < "$STAGING/backend/penpot.jar" | tr -d ' ')"
  [[ "$JAR_SIZE" -gt 50000000 ]] || die "penpot.jar suspiciously small ($JAR_SIZE bytes)"
  IMAGE_VERSION="$(tr -d '[:space:]' < "$STAGING/backend/version.txt" 2>/dev/null || true)"
  [[ -z "$IMAGE_VERSION" || "$IMAGE_VERSION" == "$PENPOT_VERSION" ]] \
    || die "backend/version.txt says '$IMAGE_VERSION' but pin is '$PENPOT_VERSION'"
  log "OK backend/ (penpot.jar $JAR_SIZE bytes) + frontend/"

  # ----- 2. jre/ ------------------------------------------------------------
  MODULES="$EXPECTED_MODULES"
  if [[ "$NO_DOCKER" -eq 0 ]]; then
    log "deriving module set from the upstream image runtime ..."
    LIVE_MODULES="$(docker run --rm --entrypoint sh "$BACKEND_IMAGE" -c "java --list-modules" \
      | sed 's/@.*//' | paste -sd, -)"
    [[ "$LIVE_MODULES" == "$EXPECTED_MODULES" ]] \
      || die "upstream module set drifted from the pin — update EXPECTED_MODULES deliberately.
live:   $LIVE_MODULES
pinned: $EXPECTED_MODULES"
    MODULES="$LIVE_MODULES"
  else
    log "no docker: using the pinned upstream module set"
  fi

  log "jdeps sanity: modules penpot.jar itself demands ..."
  JDEPS_MODULES="$("$JLINK_HOME/bin/jdeps" --multi-release "$EXPECTED_JDK_MAJOR" \
      --print-module-deps --ignore-missing-deps "$STAGING/backend/penpot.jar" 2>/dev/null | tail -1)"
  MISSING=""
  for m in ${JDEPS_MODULES//,/ }; do
    [[ ",$MODULES," == *",$m,"* ]] || MISSING="$MISSING,$m"
  done
  if [[ -n "$MISSING" ]]; then
    log "WARNING: jdeps demands modules missing from the upstream set:${MISSING#,} — adding them"
    MODULES="$MODULES${MISSING}"
  fi

  log "jlink ($JLINK_HOME, $(echo "$MODULES" | tr ',' '\n' | wc -l | tr -d ' ') modules) ..."
  rm -rf "$STAGING/jre"
  "$JLINK_HOME/bin/jlink" \
    --add-modules "$MODULES" \
    --strip-debug --no-man-pages --no-header-files --compress zip-6 \
    --output "$STAGING/jre"
  "$STAGING/jre/bin/java" --version >/dev/null || die "jlink output does not run"
  echo "$MODULES" | tr ',' '\n' > "$STAGING/jre/MODULES"
  log "OK jre/ ($( du -sh "$STAGING/jre" | awk '{print $1}' ))"

  # ----- 3. bin/ --------------------------------------------------------------
  mkdir -p "$STAGING/bin/lib" "$STAGING/licenses"
  : > "$STAGING/.kegs" # formula<TAB>keg lines for license collection

  # valkey-server
  log "bin/valkey-server (from $VALKEY_BIN) ..."
  cp "$VALKEY_BIN" "$STAGING/bin/valkey-server"
  chmod +w "$STAGING/bin/valkey-server"
  relocate_machos "$STAGING/bin" "lib" >> "$STAGING/.kegs"
  VALKEY_KEG="$(python3 -c 'import os,sys;p=os.path.realpath(sys.argv[1]);print(p.split("/bin/")[0])' "$VALKEY_BIN")"
  echo "valkey	$VALKEY_KEG" >> "$STAGING/.kegs"

  # identify (+ magick, same closure) as a fully relocatable ImageMagick payload
  log "bin/identify (relocatable ImageMagick from $IM_PREFIX) ..."
  IM_DIR="$STAGING/bin/im"
  mkdir -p "$IM_DIR/lib" "$IM_DIR/modules/coders" "$IM_DIR/modules/filters" "$IM_DIR/config"
  cp "$IDENTIFY_REAL" "$IM_DIR/identify"
  [[ -f "$MAGICK_BIN" ]] && cp "$(python3 -c 'import os,sys;print(os.path.realpath(sys.argv[1]))' "$MAGICK_BIN")" "$IM_DIR/magick"
  MODULES_SRC="$(echo "$IM_PREFIX"/lib/ImageMagick/modules-*)"
  [[ -d "$MODULES_SRC/coders" ]] || die "ImageMagick coder modules not found under $IM_PREFIX/lib/ImageMagick"
  cp "$MODULES_SRC/coders/"*.so "$IM_DIR/modules/coders/"
  cp "$MODULES_SRC/filters/"*.so "$IM_DIR/modules/filters/" 2>/dev/null || true
  # .la descriptors are REQUIRED (libltdl loads modules via them); strip the
  # absolute homebrew paths they embed (only dlname= matters at runtime).
  for sub in coders filters; do
    for la in "$MODULES_SRC/$sub"/*.la; do
      [[ -f "$la" ]] || continue
      sed -e "s/^dependency_libs=.*/dependency_libs=''/" -e "s|^libdir=.*|libdir=''|" \
        "$la" > "$IM_DIR/modules/$sub/$(basename "$la")"
    done
  done
  cp "$IM_PREFIX"/etc/ImageMagick-7/*.xml "$IM_DIR/config/" 2>/dev/null || true
  cp "$IM_PREFIX"/share/ImageMagick-7/*.xml "$IM_DIR/config/" 2>/dev/null || true
  cp "$IM_PREFIX"/lib/ImageMagick/config-*/configure.xml "$IM_DIR/config/" 2>/dev/null || true
  chmod -R u+w "$IM_DIR"
  relocate_machos "$IM_DIR" "lib" >> "$STAGING/.kegs"
  echo "imagemagick	$IM_PREFIX" >> "$STAGING/.kegs"
  cat > "$STAGING/bin/identify" <<'WRAPEOF'
#!/bin/sh
# penpot-runtime relocatable ImageMagick `identify` (see build-runtime-bundle.sh).
# The env vars point the modules-build ImageMagick at the bundled coders/config
# instead of the compile-time homebrew Cellar paths.
d="$(cd "$(dirname "$0")" && pwd)"
MAGICK_CODER_MODULE_PATH="$d/im/modules/coders"; export MAGICK_CODER_MODULE_PATH
MAGICK_CODER_FILTER_PATH="$d/im/modules/filters"; export MAGICK_CODER_FILTER_PATH
MAGICK_CONFIGURE_PATH="$d/im/config"; export MAGICK_CONFIGURE_PATH
exec "$d/im/identify" "$@"
WRAPEOF
  chmod +x "$STAGING/bin/identify"

  # penpot-watchdog
  if [[ -n "${PENPOT_WATCHDOG_BIN_SRC:-}" ]]; then
    log "bin/penpot-watchdog (from PENPOT_WATCHDOG_BIN_SRC=$PENPOT_WATCHDOG_BIN_SRC) ..."
    cp "$PENPOT_WATCHDOG_BIN_SRC" "$STAGING/bin/penpot-watchdog"
  else
    log "bin/penpot-watchdog (cargo build --release) ..."
    # shellcheck disable=SC1091
    [[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"
    (cd "$REPO_ROOT" && cargo build -q --release -p supervisor --bin penpot-watchdog) \
      || die "cargo build of penpot-watchdog failed"
    cp "$REPO_ROOT/target/release/penpot-watchdog" "$STAGING/bin/penpot-watchdog"
  fi
  chmod +x "$STAGING/bin/penpot-watchdog"
  # pure-Rust: must already be free of homebrew deps
  otool -L "$STAGING/bin/penpot-watchdog" | tail -n +2 | grep -qE "/opt/homebrew|/usr/local" \
    && die "penpot-watchdog links non-system libraries — unexpected" || true

  # ----- 4. postgres/ ---------------------------------------------------------
  mkdir -p "$STAGING/postgres"
  if [[ -d "$PG_INSTALL_CACHE/$POSTGRES_VERSION/bin" ]]; then
    log "postgres/ (pre-seeding from $PG_INSTALL_CACHE/$POSTGRES_VERSION) ..."
    cp -R "$PG_INSTALL_CACHE/$POSTGRES_VERSION" "$STAGING/postgres/$POSTGRES_VERSION"
  else
    log "postgres/ (cache miss — downloading the pinned theseus archive once) ..."
    fetch_verified "$PG_ARCHIVE_URL.sha256" "-" "$CACHE_DIR/pg.tar.gz.sha256"
    PG_SHA="$(awk '{print $1}' "$CACHE_DIR/pg.tar.gz.sha256")"
    fetch_verified "$PG_ARCHIVE_URL" "$PG_SHA" "$CACHE_DIR/pg.tar.gz"
    mkdir -p "$STAGING/postgres/.extract"
    tar -xzf "$CACHE_DIR/pg.tar.gz" -C "$STAGING/postgres/.extract"
    # the archive root is either bin/lib/share directly or one wrapping dir
    if [[ -d "$STAGING/postgres/.extract/bin" ]]; then
      mv "$STAGING/postgres/.extract" "$STAGING/postgres/$POSTGRES_VERSION"
    else
      INNER="$(find "$STAGING/postgres/.extract" -mindepth 1 -maxdepth 1 -type d | head -1)"
      [[ -d "$INNER/bin" ]] || die "unexpected postgres archive layout"
      mv "$INNER" "$STAGING/postgres/$POSTGRES_VERSION"
      rm -rf "$STAGING/postgres/.extract"
    fi
  fi
  [[ -x "$STAGING/postgres/$POSTGRES_VERSION/bin/initdb" ]] || die "postgres payload has no initdb"

  # ----- 4b. bin/node (N2: exporter runtime) ----------------------------------
  log "bin/node (official v$NODE_VERSION darwin-arm64, sha256-pinned) ..."
  fetch_verified "$NODE_URL" "$NODE_SHA256" "$CACHE_DIR/node-v$NODE_VERSION-darwin-arm64.tar.gz"
  NODE_EXTRACT="$STAGING/.node-extract"
  mkdir -p "$NODE_EXTRACT"
  tar -xzf "$CACHE_DIR/node-v$NODE_VERSION-darwin-arm64.tar.gz" -C "$NODE_EXTRACT" \
    "node-v$NODE_VERSION-darwin-arm64/bin/node" \
    "node-v$NODE_VERSION-darwin-arm64/LICENSE"
  cp "$NODE_EXTRACT/node-v$NODE_VERSION-darwin-arm64/bin/node" "$STAGING/bin/node"
  cp "$NODE_EXTRACT/node-v$NODE_VERSION-darwin-arm64/LICENSE" "$STAGING/licenses/node-LICENSE.txt"
  rm -rf "$NODE_EXTRACT"
  chmod +x "$STAGING/bin/node"
  # Official build links system libs only — relocatable by construction; the
  # P6 audit (whole bin/) enforces it.
  NODE_REPORTED="$("$STAGING/bin/node" --version)"
  [[ "$NODE_REPORTED" == "v$NODE_VERSION" ]] \
    || die "bundled node reports $NODE_REPORTED, expected v$NODE_VERSION"

  # ----- 4c. exporter/ ---------------------------------------------------------
  # Prefer the repo's already-extracted tree (scripts/fetch-penpot.sh) when it
  # matches the pin; otherwise extract from the exporter image (docker/crane).
  # The app tree is pure JS (zero native .node bindings — M5-verified), so the
  # linux image's node_modules run on macOS as-is.
  #
  # SYMLINK-FREE node_modules, rebuilt HOISTED from the upstream lockfile:
  # the image ships a pnpm layout whose resolution depends on directory
  # symlinks into the .pnpm store, and tauri's resource copy silently DROPS
  # directory symlinks (verified live: the installed .app lost every
  # top-level package link → ERR_MODULE_NOT_FOUND 'date-fns'; naively
  # dereferencing with cp -RL breaks pnpm's sibling resolution instead —
  # playwright could no longer find playwright-core). So node_modules is
  # re-materialized with `pnpm install --node-linker=hoisted
  # --frozen-lockfile` against the exporter's own pnpm-lock.yaml: the exact
  # locked, integrity-checksummed dependency graph in an npm-style layout
  # with zero symlinks. Needs the registry once per build (pnpm store caches
  # it); package CONTENT is lockfile-pinned, only the layout differs from
  # the image (the git-hosted @penpot/svgo tarball ships its dist/ —
  # verified file-identical to the image's copy).
  PNPM_VERSION="11.5.3" # = the exporter package.json packageManager pin
  command -v npx >/dev/null \
    || die "npx (any host node install) is required to materialize the exporter node_modules"
  EXPORTER_SRC=""
  if [[ -f "$REPO_ROOT/runtime/exporter/VERSION" ]] \
    && [[ "$(cat "$REPO_ROOT/runtime/exporter/VERSION")" == "$PENPOT_VERSION" ]] \
    && [[ -s "$REPO_ROOT/runtime/exporter/app.js" ]]; then
    log "exporter/ (from runtime/exporter, VERSION matches $PENPOT_VERSION) ..."
    EXPORTER_SRC="$REPO_ROOT/runtime/exporter"
  elif [[ "$NO_DOCKER" -eq 0 ]]; then
    log "extracting exporter from ${EXPORTER_IMAGE} (docker create/cp) ..."
    CID="$(docker create "$EXPORTER_IMAGE")"; CIDS+=("$CID")
    docker cp -q "$CID:/opt/penpot/exporter" "$STAGING/.exporter-raw"
    docker rm "$CID" >/dev/null; CIDS=()
    EXPORTER_SRC="$STAGING/.exporter-raw"
  else
    CRANE="$CACHE_DIR/crane-$CRANE_VERSION"
    if [[ ! -x "$CRANE" ]]; then
      fetch_verified "$CRANE_URL" "$CRANE_SHA256" "$CACHE_DIR/crane.tar.gz"
      tar -xzf "$CACHE_DIR/crane.tar.gz" -C "$CACHE_DIR" crane
      mv "$CACHE_DIR/crane" "$CRANE"
      rm -f "$CACHE_DIR/crane.tar.gz"
    fi
    log "exporting ${EXPORTER_IMAGE} via crane (no docker daemon) ..."
    mkdir -p "$STAGING/.exporter-rootfs"
    # Exclude node_modules from the extraction: the image ships a pnpm layout
    # whose files are HARD-LINKS into opt/penpot/.local/share/pnpm/store (a path
    # OUTSIDE the `opt/penpot/exporter` filter), so `tar` would abort with
    # "Hard-link target ... does not exist" on the macOS runner's bsdtar. We
    # don't need it anyway — the node_modules tree is deleted immediately below
    # and re-materialized HOISTED from the pinned pnpm-lock.yaml. We only need
    # the app files (app.js, package.json, pnpm-lock.yaml) from the image.
    "$CRANE" export "$EXPORTER_IMAGE" - --platform linux/arm64 \
      | tar -xf - -C "$STAGING/.exporter-rootfs" \
          --exclude 'opt/penpot/exporter/node_modules/*' \
          --exclude 'opt/penpot/exporter/node_modules' \
          opt/penpot/exporter \
      || die "crane export of $EXPORTER_IMAGE failed"
    EXPORTER_SRC="$STAGING/.exporter-rootfs/opt/penpot/exporter"
  fi
  # App files without the pnpm node_modules, then the hoisted re-install.
  cp -R "$EXPORTER_SRC" "$STAGING/exporter" || die "copying the exporter app files failed"
  rm -rf "$STAGING/exporter/node_modules"
  [[ -f "$STAGING/exporter/pnpm-lock.yaml" ]] || die "exporter tree has no pnpm-lock.yaml"
  log "exporter/node_modules (pnpm@$PNPM_VERSION hoisted install from the pinned lockfile) ..."
  (cd "$STAGING/exporter" && npx -y "pnpm@$PNPM_VERSION" install --prod --frozen-lockfile \
      --ignore-scripts --config.node-linker=hoisted >/dev/null) \
    || die "pnpm hoisted install of the exporter node_modules failed"
  # .bin holds the only remaining (file) symlinks; nothing execs them at
  # runtime — drop them so the tree is 100% symlink-free.
  rm -rf "$STAGING/exporter/node_modules/.bin"
  # Prune fsevents — the ONE native binding a HOST hoisted install adds that the
  # upstream Linux image's node_modules never had. It is an OPTIONAL, macOS-only
  # transitive dep (chokidar → fsevents, `optional: true` in pnpm-lock.yaml); the
  # render service never watches files, so it is dead weight, and leaving its
  # fsevents.node in would trip the native-bindings guard below. (--no-optional
  # is not an option: it mutates pnpm's recorded overrides hash and fails the
  # --frozen-lockfile check against the exporter's pnpm-workspace.yaml.) The
  # pure-JS tree is proven end-to-end by P9's /readyz boot + the n2 live render.
  rm -rf "$STAGING/exporter/node_modules/fsevents"
  rm -rf "$STAGING/.exporter-raw" "$STAGING/.exporter-rootfs"
  printf '%s\n' "$PENPOT_VERSION" > "$STAGING/exporter/VERSION"
  LEFT_LINKS="$(find "$STAGING/exporter" -type l | head -1)"
  [[ -z "$LEFT_LINKS" ]] \
    || die "exporter tree still contains symlinks (e.g. $LEFT_LINKS) — tauri would drop them"
  [[ -s "$STAGING/exporter/app.js" && -d "$STAGING/exporter/node_modules" ]] \
    || die "exporter payload incomplete (app.js/node_modules missing)"
  for pkg in date-fns playwright playwright-core; do
    [[ -d "$STAGING/exporter/node_modules/$pkg" ]] \
      || die "hoisted exporter node_modules is missing $pkg as a real dir"
  done
  [[ -f "$STAGING/exporter/LICENSE" ]] \
    && cp "$STAGING/exporter/LICENSE" "$STAGING/licenses/penpot-exporter-MPL-2.0.txt"
  # No native bindings may sneak in on a version bump (the whole premise of
  # running the image's node_modules on macOS).
  NATIVE_NODES="$(find "$STAGING/exporter" -name '*.node' | head -1)"
  [[ -z "$NATIVE_NODES" ]] || die "exporter tree contains native bindings ($NATIVE_NODES) — the copy-from-image shortcut no longer holds"

  # ----- 4d. exporter-browsers/ (chromium HEADLESS SHELL only) -----------------
  # Playwright ≥1.49 serves default headless launches from the headless-shell
  # build, so the full chromium (~344 MB) and ffmpeg stay out of the bundle.
  # Prefer the repo's playwright-managed cache; otherwise download via the
  # exporter's own playwright CLI (needs network once).
  mkdir -p "$STAGING/exporter-browsers"
  REPO_SHELL="$(compgen -G "$REPO_ROOT/runtime/exporter-browsers/chromium_headless_shell-*" | head -1 || true)"
  if [[ -n "$REPO_SHELL" && -x "$REPO_SHELL/chrome-headless-shell-mac-arm64/chrome-headless-shell" ]]; then
    log "exporter-browsers/ (copying $(basename "$REPO_SHELL") from runtime/) ..."
    cp -R "$REPO_SHELL" "$STAGING/exporter-browsers/$(basename "$REPO_SHELL")"
  else
    log "exporter-browsers/ (playwright install chromium --only-shell via bundled node) ..."
    PLAYWRIGHT_BROWSERS_PATH="$STAGING/.browsers-dl" "$STAGING/bin/node" \
      "$STAGING/exporter/node_modules/playwright/cli.js" install chromium --only-shell \
      || die "playwright install chromium --only-shell failed"
    DL_SHELL="$(compgen -G "$STAGING/.browsers-dl/chromium_headless_shell-*" | head -1 || true)"
    [[ -n "$DL_SHELL" ]] || die "playwright install produced no chromium_headless_shell dir"
    mv "$DL_SHELL" "$STAGING/exporter-browsers/$(basename "$DL_SHELL")"
    rm -rf "$STAGING/.browsers-dl"
  fi
  bundle_headless_shell "$STAGING" || die "no headless shell binary in the staged bundle"
  SHELL_DIR="$(compgen -G "$STAGING/exporter-browsers/chromium_headless_shell-*" | head -1)"
  HEADLESS_SHELL_REV="$(basename "$SHELL_DIR" | sed 's/^chromium_headless_shell-//')"
  # Playwright validates INSTALLATION_COMPLETE + DEPENDENCIES_VALIDATED marker
  # files; the repo cache ships them, a fresh download too — but be safe.
  touch "$SHELL_DIR/INSTALLATION_COMPLETE" 2>/dev/null || true
  cp "$SHELL_DIR/chrome-headless-shell-mac-arm64/LICENSE.headless_shell" \
     "$STAGING/licenses/chromium-headless-shell-LICENSE.txt" 2>/dev/null || true
  log "OK exporter stack: node v$NODE_VERSION + exporter/ + headless shell rev $HEADLESS_SHELL_REV"

  # ----- 5. licenses/ + MANIFEST.json + VERSION -------------------------------
  log "licenses/ ..."
  # Penpot itself (MPL-2.0): ship the canonical text (fetched once, cached).
  fetch_verified "https://raw.githubusercontent.com/penpot/penpot/${PENPOT_VERSION}/LICENSE" "-" \
    "$CACHE_DIR/penpot-LICENSE-${PENPOT_VERSION}"
  cp "$CACHE_DIR/penpot-LICENSE-${PENPOT_VERSION}" "$STAGING/licenses/penpot-MPL-2.0.txt"
  # OpenJDK (GPLv2 + Classpath exception) — from the JDK's own legal tree.
  if [[ -d "$JLINK_HOME/libexec/openjdk.jdk/Contents/Home/legal/java.base" ]]; then
    cp "$JLINK_HOME/libexec/openjdk.jdk/Contents/Home/legal/java.base/LICENSE" \
       "$STAGING/licenses/openjdk-GPL-2.0-with-Classpath-exception.txt" 2>/dev/null || true
  fi
  # postgres
  cp "$STAGING/postgres/$POSTGRES_VERSION/LICENSE" "$STAGING/licenses/postgresql-LICENSE.txt" 2>/dev/null || true
  cp "$STAGING/postgres/$POSTGRES_VERSION/COPYRIGHT" "$STAGING/licenses/postgresql-COPYRIGHT.txt" 2>/dev/null || true
  # every homebrew keg that contributed a binary or dylib
  sort -u "$STAGING/.kegs" | while IFS=$'\t' read -r formula keg; do
    [[ -d "$keg" ]] || continue
    for lf in LICENSE LICENSE.txt LICENSE.md COPYING COPYING.LESSER LICENSE.LESSER NOTICE; do
      if [[ -f "$keg/$lf" ]]; then
        cp "$keg/$lf" "$STAGING/licenses/${formula}-${lf}"
      fi
    done
  done
  rm -f "$STAGING/.kegs"

  printf '%s\n' "$PENPOT_VERSION" > "$STAGING/VERSION"

  log "MANIFEST.json ..."
  WATCHDOG_REV="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  BUNDLE_JAVA_VERSION="$("$STAGING/jre/bin/java" --version | head -1 | awk '{print $2}')"
  python3 - "$STAGING" <<PYEOF
import json, os, sys, datetime
staging = sys.argv[1]
modules = open(os.path.join(staging, "jre", "MODULES")).read().split()
licenses = sorted(os.listdir(os.path.join(staging, "licenses")))
manifest = {
    "bundleLayoutVersion": 1,
    "platform": "darwin-arm64",
    "builtAt": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
    "components": {
        "penpot": {
            "version": "$PENPOT_VERSION",
            "license": "MPL-2.0",
            "source": "docker.io/penpotapp/{backend,frontend}:$PENPOT_VERSION",
            "paths": ["backend/", "frontend/"],
        },
        "jre": {
            "version": "$BUNDLE_JAVA_VERSION",
            "license": "GPL-2.0-only WITH Classpath-exception-2.0",
            "source": "jlink from $JLINK_HOME (module set = upstream backend image runtime)",
            "paths": ["jre/"],
            "modules": modules,
        },
        "valkey": {
            "version": "$VALKEY_VERSION",
            "license": "BSD-3-Clause",
            "source": "homebrew ($VALKEY_BIN), made relocatable",
            "paths": ["bin/valkey-server", "bin/lib/"],
        },
        "imagemagick": {
            "version": "$IM_VERSION",
            "license": "ImageMagick",
            "source": "homebrew ($IM_PREFIX), made relocatable",
            "paths": ["bin/identify", "bin/im/"],
            "note": "bin/identify is a wrapper that points the modules build at bin/im/",
        },
        "postgresql": {
            "version": "$POSTGRES_VERSION",
            "license": "PostgreSQL",
            "source": "theseus-rs/postgresql-binaries (postgresql_embedded-compatible layout)",
            "paths": ["postgres/$POSTGRES_VERSION/"],
            "note": "pre-seeded so the packaged app's first boot never downloads",
        },
        "penpot-watchdog": {
            "version": "git-$WATCHDOG_REV",
            "license": "same as this repository",
            "source": "cargo build --release -p supervisor --bin penpot-watchdog",
            "paths": ["bin/penpot-watchdog"],
        },
        "node": {
            "version": "$NODE_VERSION",
            "license": "MIT (plus bundled-deps licenses in the LICENSE file)",
            "source": "official nodejs.org darwin-arm64 tarball, sha256-pinned",
            "paths": ["bin/node"],
            "note": "exporter runtime ONLY (N2). The backend still never execs node: "
                    "app.svgo/optimize has no callers (verified live in M4 with node "
                    "hidden; re-verify on PENPOT_VERSION bumps).",
        },
        "penpot-exporter": {
            "version": "$PENPOT_VERSION",
            "license": "MPL-2.0",
            "source": "docker.io/penpotapp/exporter:$PENPOT_VERSION (pure-JS app tree)",
            "paths": ["exporter/"],
        },
        "chromium-headless-shell": {
            "version": "playwright build $HEADLESS_SHELL_REV",
            "license": "BSD-3-Clause (see licenses/chromium-headless-shell-LICENSE.txt)",
            "source": "playwright-managed chromium headless shell (PLAYWRIGHT_BROWSERS_PATH payload)",
            "paths": ["exporter-browsers/"],
            "note": "headless shell ONLY — no full chromium, no ffmpeg (P10 proves "
                    "playwright's default headless launch uses this build)",
        },
    },
    "licenses": ["licenses/" + f for f in licenses],
}
with open(os.path.join(staging, "MANIFEST.json"), "w") as f:
    json.dump(manifest, f, indent=2, sort_keys=True)
    f.write("\n")
PYEOF

  printf '%s' "$FINGERPRINT" > "$STAGING/.fingerprint"

  # ----- swap into place -------------------------------------------------------
  # (proofs below run against the staged tree first; swap only after they pass)
fi

# ---------------------------------------------------------------------------
# PROVE steps (always run — against staging on a fresh build, dest on skip)
# ---------------------------------------------------------------------------
log "verification against $BUNDLE"
PROOF_TMP="$(mktemp -d "${TMPDIR:-/tmp}/penpot-bundle-proof.XXXXXX")"
PROOF_FAILURES=0
proof_cleanup() {
  [[ -n "${PROOF_VALKEY_PID:-}" ]] && kill -9 "$PROOF_VALKEY_PID" 2>/dev/null || true
  [[ -n "${PROOF_JAVA_PID:-}" ]] && kill -9 "$PROOF_JAVA_PID" 2>/dev/null || true
  [[ -n "${PROOF_EXPORTER_PID:-}" ]] && kill -9 "$PROOF_EXPORTER_PID" 2>/dev/null || true
  [[ -n "${PROOF_VALKEY2_PID:-}" ]] && kill -9 "$PROOF_VALKEY2_PID" 2>/dev/null || true
  [[ -d "$PROOF_TMP/pgdata" ]] && env -i PATH=/usr/bin:/bin \
      "$BUNDLE/postgres/$POSTGRES_VERSION/bin/pg_ctl" -D "$PROOF_TMP/pgdata" -m immediate stop >/dev/null 2>&1 || true
  rm -rf "$PROOF_TMP"
  cleanup
}
trap proof_cleanup EXIT
ok()   { log "PROVE $1: OK $2"; }
bad()  { log "PROVE $1: FAIL $2"; PROOF_FAILURES=$((PROOF_FAILURES + 1)); }

PENPOT_JAVA_OPTS=(
  -Dim4java.useV7=true
  -Djava.util.logging.manager=org.apache.logging.log4j.jul.LogManager
  -Dlog4j2.configurationFile=log4j2.xml
  -XX:-OmitStackTraceInFastThrow
  --sun-misc-unsafe-memory-access=allow
  --enable-native-access=ALL-UNNAMED
  --enable-preview
)

# P1: jlink'd java boots the jar's Clojure runtime (namespace-miss sanity check)
P1_OUT="$(cd "$BUNDLE/backend" && env -i "$BUNDLE/jre/bin/java" "${PENPOT_JAVA_OPTS[@]}" \
    -jar penpot.jar -m app.bundle-sanity-check 2>&1 || true)"
if grep -q "Could not locate app/bundle_sanity_check" <<< "$P1_OUT"; then
  ok P1 "jre/bin/java boots penpot.jar's Clojure runtime"
else
  echo "$P1_OUT" | head -5 >&2
  bad P1 "expected the namespace-miss error from clojure.main"
fi

# P2: config-error-level boot — nonexistent DB URI must reach the backend's
# 'initialize connection pool' log line using ONLY the bundled JRE.
(cd "$BUNDLE/backend" && exec env -i \
    PENPOT_SECRET_KEY=bundle-proof PENPOT_PUBLIC_URI=http://localhost:1 \
    PENPOT_DATABASE_URI="postgresql://127.0.0.1:1/nonexistent" \
    PENPOT_DATABASE_USERNAME=postgres PENPOT_DATABASE_PASSWORD=x \
    PENPOT_REDIS_URI="redis://localhost:1/0" \
    PENPOT_FLAGS="disable-email-verification disable-secure-session-cookies disable-onboarding enable-access-tokens" \
    PENPOT_OBJECTS_STORAGE_BACKEND=fs PENPOT_OBJECTS_STORAGE_FS_DIRECTORY="$PROOF_TMP/assets" \
    PENPOT_TELEMETRY_ENABLED=false \
    "$BUNDLE/jre/bin/java" "${PENPOT_JAVA_OPTS[@]}" -jar penpot.jar \
    -e "(do (require 'app.main) (app.main/start) (deref (promise)))" \
    > "$PROOF_TMP/boot.log" 2>&1) &
PROOF_JAVA_PID=$!
P2_OK=0
for _ in $(seq 1 60); do
  grep -q 'initialize connection pool' "$PROOF_TMP/boot.log" 2>/dev/null && { P2_OK=1; break; }
  kill -0 "$PROOF_JAVA_PID" 2>/dev/null || break
  sleep 2
done
kill -9 "$PROOF_JAVA_PID" 2>/dev/null || true
wait "$PROOF_JAVA_PID" 2>/dev/null || true
PROOF_JAVA_PID=""
if [[ "$P2_OK" -eq 1 ]]; then
  ok P2 "config-error boot reached 'initialize connection pool' on the jlink'd JRE"
else
  tail -10 "$PROOF_TMP/boot.log" >&2 || true
  bad P2 "backend never logged 'initialize connection pool'"
fi

# P3: valkey-server relocatable — version + live PING round-trip, env -i
P3_VER="$(cd "$PROOF_TMP" && env -i PATH=/usr/bin:/bin "$BUNDLE/bin/valkey-server" --version 2>&1 || true)"
(cd "$PROOF_TMP" && exec env -i PATH=/usr/bin:/bin \
    "$BUNDLE/bin/valkey-server" --port "$TEST_VALKEY_PORT" --bind 127.0.0.1 \
    --save '' --appendonly no --dir "$PROOF_TMP" > "$PROOF_TMP/valkey.log" 2>&1) &
PROOF_VALKEY_PID=$!
P3_PONG=""
for _ in $(seq 1 20); do
  P3_PONG="$(python3 - "$TEST_VALKEY_PORT" <<'PYEOF' 2>/dev/null || true
import socket, sys
s = socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=1)
s.sendall(b"PING\r\n")
print(s.recv(16).decode().strip())
PYEOF
)"
  [[ "$P3_PONG" == "+PONG" ]] && break
  sleep 0.5
done
kill -9 "$PROOF_VALKEY_PID" 2>/dev/null || true
wait "$PROOF_VALKEY_PID" 2>/dev/null || true
PROOF_VALKEY_PID=""
if grep -q "v=$VALKEY_VERSION" <<< "$P3_VER" && [[ "$P3_PONG" == "+PONG" ]]; then
  ok P3 "valkey-server v=$VALKEY_VERSION runs from scratch dir (env -i) and answers PING"
else
  bad P3 "version [$P3_VER] ping [$P3_PONG]"
fi

# P4: identify relocatable — generated PNG + SVG, env -i from a scratch dir
python3 - "$PROOF_TMP/probe.png" <<'PYEOF'
import struct, sys, zlib
def chunk(t, d): return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d))
ihdr = chunk(b"IHDR", struct.pack(">IIBBBBB", 8, 8, 8, 0, 0, 0, 0))
raw = b"".join(b"\x00" + bytes((x * 30) % 256 for x in range(8)) for _ in range(8))
open(sys.argv[1], "wb").write(b"\x89PNG\r\n\x1a\n" + ihdr + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))
PYEOF
printf '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10"/></svg>' \
  > "$PROOF_TMP/probe.svg"
P4_PNG="$(cd "$PROOF_TMP" && env -i PATH=/usr/bin:/bin "$BUNDLE/bin/identify" probe.png 2>&1 || true)"
P4_SVG="$(cd "$PROOF_TMP" && env -i PATH=/usr/bin:/bin "$BUNDLE/bin/identify" probe.svg 2>&1 || true)"
if grep -q "PNG 8x8" <<< "$P4_PNG" && grep -q "SVG 10x10" <<< "$P4_SVG"; then
  ok P4 "identify decodes PNG and SVG from scratch dir (env -i)"
else
  bad P4 "png [$P4_PNG] svg [$P4_SVG]"
fi

# P5: postgres pre-seed — initdb + pg_ctl start + pg_isready + stop, env -i,
# binaries invoked from their bundle location
PGB="$BUNDLE/postgres/$POSTGRES_VERSION/bin"
P5_OK=1
env -i PATH=/usr/bin:/bin "$PGB/initdb" --no-locale -E UTF8 -U postgres \
    -D "$PROOF_TMP/pgdata" > "$PROOF_TMP/initdb.log" 2>&1 || { P5_OK=0; tail -5 "$PROOF_TMP/initdb.log" >&2; }
if [[ "$P5_OK" -eq 1 ]]; then
  env -i PATH=/usr/bin:/bin "$PGB/pg_ctl" -D "$PROOF_TMP/pgdata" \
      -o "-p $TEST_PG_PORT -h 127.0.0.1 -k ''" -w -l "$PROOF_TMP/pg.log" start \
      > /dev/null 2>&1 || { P5_OK=0; tail -5 "$PROOF_TMP/pg.log" >&2; }
fi
if [[ "$P5_OK" -eq 1 ]]; then
  env -i PATH=/usr/bin:/bin "$PGB/pg_isready" -h 127.0.0.1 -p "$TEST_PG_PORT" >/dev/null 2>&1 || P5_OK=0
  env -i PATH=/usr/bin:/bin "$PGB/pg_ctl" -D "$PROOF_TMP/pgdata" -w stop >/dev/null 2>&1 || P5_OK=0
fi
if [[ "$P5_OK" -eq 1 ]]; then
  ok P5 "postgres $POSTGRES_VERSION initdb/pg_ctl/pg_isready from bundle location (env -i)"
else
  bad P5 "postgres pre-seed is not functional"
fi

# P6: relocation audit — no Mach-O under bin/ may reference homebrew paths
if audit_no_local_refs "$BUNDLE/bin" "bin/"; then
  ok P6 "no /opt/homebrew|/usr/local install names anywhere under bin/"
else
  bad P6 "relocation audit"
fi

# P7 (best effort): re-run P3/P4 under sandbox-exec with /opt/homebrew unreadable
if command -v sandbox-exec >/dev/null 2>&1; then
  SB='(version 1)(allow default)(deny file-read* (subpath "/opt/homebrew"))'
  P7_V="$(cd "$PROOF_TMP" && sandbox-exec -p "$SB" "$BUNDLE/bin/valkey-server" --version 2>&1 || true)"
  P7_I="$(cd "$PROOF_TMP" && sandbox-exec -p "$SB" "$BUNDLE/bin/identify" probe.png 2>&1 || true)"
  if grep -q "v=$VALKEY_VERSION" <<< "$P7_V" && grep -q "PNG 8x8" <<< "$P7_I"; then
    ok P7 "valkey-server + identify still work with /opt/homebrew reads DENIED (sandbox-exec)"
  else
    bad P7 "sandboxed run failed: valkey [$P7_V] identify [$P7_I]"
  fi
else
  log "PROVE P7: SKIP (sandbox-exec unavailable)"
fi

# P8: bundled node runs from a scratch dir with env -i and reports the pin
P8_VER="$(cd "$PROOF_TMP" && env -i PATH=/usr/bin:/bin "$BUNDLE/bin/node" --version 2>&1 || true)"
if [[ "$P8_VER" == "v$NODE_VERSION" ]]; then
  ok P8 "bin/node reports v$NODE_VERSION from scratch dir (env -i)"
else
  bad P8 "bin/node reported [$P8_VER], expected v$NODE_VERSION"
fi

# P9: the exporter app boots on the BUNDLED node (own valkey, env -i,
# poisoned proxies — nothing may reach the network) and answers /readyz 200.
(cd "$PROOF_TMP" && exec env -i PATH=/usr/bin:/bin \
    "$BUNDLE/bin/valkey-server" --port "$TEST_VALKEY_PORT" --bind 127.0.0.1 \
    --save '' --appendonly no --dir "$PROOF_TMP" > "$PROOF_TMP/valkey-p9.log" 2>&1) &
PROOF_VALKEY2_PID=$!
mkdir -p "$PROOF_TMP/exporter-tmp"
(cd "$BUNDLE/exporter" && exec env -i PATH=/usr/bin:/bin HOME="$PROOF_TMP" \
    http_proxy="http://127.0.0.1:1" https_proxy="http://127.0.0.1:1" \
    PENPOT_SECRET_KEY=bundle-proof \
    PENPOT_PUBLIC_URI="http://127.0.0.1:1" \
    PENPOT_REDIS_URI="redis://127.0.0.1:${TEST_VALKEY_PORT}/0" \
    PENPOT_HTTP_SERVER_PORT="$TEST_EXPORTER_PORT" \
    PENPOT_TEMPDIR="$PROOF_TMP/exporter-tmp" \
    PLAYWRIGHT_BROWSERS_PATH="$BUNDLE/exporter-browsers" \
    "$BUNDLE/bin/node" app.js > "$PROOF_TMP/exporter.log" 2>&1) &
PROOF_EXPORTER_PID=$!
P9_CODE=""
for _ in $(seq 1 30); do
  P9_CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 \
      "http://127.0.0.1:${TEST_EXPORTER_PORT}/readyz" 2>/dev/null || true)"
  [[ "$P9_CODE" == "200" ]] && break
  kill -0 "$PROOF_EXPORTER_PID" 2>/dev/null || break
  sleep 1
done
if [[ "$P9_CODE" == "200" ]]; then
  ok P9 "exporter answers /readyz 200 on the bundled node (env -i, poisoned proxies)"
else
  tail -10 "$PROOF_TMP/exporter.log" >&2 || true
  bad P9 "exporter /readyz never returned 200 (last code: ${P9_CODE:-none})"
fi
kill -9 "$PROOF_EXPORTER_PID" 2>/dev/null || true
wait "$PROOF_EXPORTER_PID" 2>/dev/null || true
PROOF_EXPORTER_PID=""
kill -9 "$PROOF_VALKEY2_PID" 2>/dev/null || true
wait "$PROOF_VALKEY2_PID" 2>/dev/null || true
PROOF_VALKEY2_PID=""

# P10: the bundled playwright launches the bundled HEADLESS SHELL — proves
# the shell-only browsers dir is sufficient (no full chromium anywhere).
cat > "$PROOF_TMP/p10.cjs" <<'P10EOF'
const path = process.argv[2];
const { chromium } = require(path);
(async () => {
  const browser = await chromium.launch({ args: ["--allow-insecure-localhost"] });
  const page = await browser.newPage();
  await page.setContent("<h1>n2</h1>");
  const sum = await page.evaluate("40 + 2");
  const version = browser.version();
  await browser.close();
  if (sum !== 42) throw new Error("evaluate returned " + sum);
  console.log("HEADLESS_OK " + version);
})().catch((e) => { console.error(e); process.exit(1); });
P10EOF
P10_OUT="$(cd "$PROOF_TMP" && env -i PATH=/usr/bin:/bin HOME="$PROOF_TMP" TMPDIR="$PROOF_TMP" \
    http_proxy="http://127.0.0.1:1" https_proxy="http://127.0.0.1:1" \
    PLAYWRIGHT_BROWSERS_PATH="$BUNDLE/exporter-browsers" \
    "$BUNDLE/bin/node" "$PROOF_TMP/p10.cjs" "$BUNDLE/exporter/node_modules/playwright" 2>&1 || true)"
if grep -q "^HEADLESS_OK " <<< "$P10_OUT"; then
  ok P10 "playwright launched the bundled headless shell and evaluated JS ($P10_OUT)"
else
  bad P10 "headless-shell launch failed: $(echo "$P10_OUT" | head -3)"
fi

[[ "$PROOF_FAILURES" -eq 0 ]] || die "$PROOF_FAILURES verification step(s) failed — bundle NOT installed"

# ---------------------------------------------------------------------------
# Swap staging into place (fresh build only) + report
# ---------------------------------------------------------------------------
if [[ -n "$STAGING" ]]; then
  log "all proofs green — swapping into $DEST"
  rm -rf "$DEST"
  mv "$STAGING" "$DEST"
  STAGING=""
fi

log "bundle ready at $DEST"
log "component sizes:"
for c in backend frontend jre bin postgres exporter exporter-browsers licenses; do
  if [[ -e "$DEST/$c" ]]; then
    KB="$(du -sk "$DEST/$c" | cut -f1)"
    printf '  %-10s %8.1f MB\n' "$c" "$(python3 -c "print($KB/1024)")"
  fi
done
TOTAL_KB="$(du -sk "$DEST" | cut -f1)"
log "total: $(python3 -c "print(round($TOTAL_KB/1024, 1))") MB"
