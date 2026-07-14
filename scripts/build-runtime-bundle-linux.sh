#!/usr/bin/env bash
#
# build-runtime-bundle-linux.sh — produce the self-contained `penpot-runtime/`
# bundle for Linux x86_64 (the AppImage payload). Parallel implementation of
# scripts/build-runtime-bundle.sh (macOS arm64); SAME layout contract, SAME
# proof suite, ELF/patchelf relocation instead of Mach-O/install_name_tool.
#
#   penpot-runtime/
#     backend/            penpot.jar, … (penpotapp/backend:$PENPOT_VERSION)
#     frontend/           static SPA    (penpotapp/frontend:$PENPOT_VERSION)
#     jre/                jlink output (bin/java), module set = upstream image
#     bin/
#       valkey-server     official noble x86_64 build, non-glibc dylib closure
#                         bundled in bin/lib with $ORIGIN rpaths (patchelf)
#       identify, magick  wrappers -> bin/im/ (extracted official ImageMagick
#                         AppImage; relies on host system libs per the AppImage
#                         excludelist: fontconfig/X11/glib — like any AppImage)
#       penpot-watchdog   cargo build --release -p supervisor
#     postgres/$POSTGRES_VERSION/   theseus postgresql_embedded pre-seed
#     licenses/  MANIFEST.json  VERSION  .fingerprint
#
# N2: the bundle now ALSO ships the render stack (same contract as macOS —
# see build-runtime-bundle.sh):
#     bin/node            official node v24.16.0 linux-x64 (sha256-pinned) —
#                         the exporter's runtime (the backend still never execs
#                         node; that M4 proof is unchanged)
#     exporter/           penpotapp/exporter:$PENPOT_VERSION app tree (pure JS)
#     exporter-browsers/  playwright chromium HEADLESS SHELL only
#
# Sources: docker create/cp (default) or --no-docker via pinned static crane.
# JDK: $JLINK_HOME if set (CI: actions/setup-java temurin 26), else a pinned
# temurin 26.0.1+8 download (sha256 from the Adoptium API, hardcoded here).
#
# Proofs (always run, also on fingerprint-skip): P1 clojure namespace-miss
# sanity, P2 config-error backend boot reaches 'initialize connection pool',
# P3 valkey PING/PONG env -i, P4 identify PNG+SVG env -i, P5 postgres
# initdb/pg_ctl/pg_isready env -i, P6 ELF relocation audit (no 'not found'
# in bin/valkey-server + bin/lib, rpath=$ORIGIN…).
#
# Usage: scripts/build-runtime-bundle-linux.sh [--dest DIR] [--force] [--no-docker]

set -euo pipefail

# ---------------------------------------------------------------------------
# Pins & configuration
# ---------------------------------------------------------------------------
PENPOT_VERSION="${PENPOT_VERSION:-2.16.2}"
BACKEND_IMAGE="penpotapp/backend:${PENPOT_VERSION}"
FRONTEND_IMAGE="penpotapp/frontend:${PENPOT_VERSION}"
EXPORTER_IMAGE="penpotapp/exporter:${PENPOT_VERSION}"

# node for the exporter child (N2): upstream exporter image pins v24.16.0;
# official nodejs.org build, sha256 from SHASUMS256.txt (verified 2026-07-14).
NODE_VERSION="24.16.0"
NODE_URL="https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.gz"
NODE_SHA256="2faf6a387e9b62b888e21c54f01249fb27537ffecf1842f29f4c919d0a59a0ff"

POSTGRES_VERSION="15.18.0" # MUST match crates/supervisor DEFAULT_POSTGRES_VERSION
PG_ARCHIVE_URL="https://github.com/theseus-rs/postgresql-binaries/releases/download/${POSTGRES_VERSION}/postgresql-${POSTGRES_VERSION}-x86_64-unknown-linux-gnu.tar.gz"
PG_SHA256="b51101a3382b8a99583c7eef1e940ff2880e21275ef7ff519098b0b13ff4af50"

EXPECTED_JDK_MAJOR=26
# Pinned temurin (sha256 from api.adoptium.net for jdk-26.0.1+8 linux x64).
TEMURIN_URL="https://github.com/adoptium/temurin26-binaries/releases/download/jdk-26.0.1%2B8/OpenJDK26U-jdk_x64_linux_hotspot_26.0.1_8.tar.gz"
TEMURIN_SHA256="8e512f13e575a43655fc92319436c94890c137b9035cc6bd6f9cf24239704d3a"

# Same pin as the macOS script (module set is platform-independent); when
# docker is available it is re-derived live and MUST match.
EXPECTED_MODULES="java.base,java.compiler,java.datatransfer,java.desktop,java.instrument,java.logging,java.management,java.management.rmi,java.naming,java.net.http,java.prefs,java.rmi,java.scripting,java.se,java.security.jgss,java.security.sasl,java.sql,java.sql.rowset,java.transaction.xa,java.xml,java.xml.crypto,jdk.attach,jdk.compiler,jdk.internal.jvmstat,jdk.internal.md,jdk.internal.opt,jdk.javadoc,jdk.jcmd,jdk.jfr,jdk.management.agent,jdk.net,jdk.unsupported,jdk.zipfs"

VALKEY_VERSION="9.1.0"
VALKEY_URL="https://download.valkey.io/releases/valkey-${VALKEY_VERSION}-noble-x86_64.tar.gz"
VALKEY_SHA256="bf2269ad6913e72338f9caa8639a197010e168e3338cc5393a4d2a172a6c6d21"

IM_VERSION="7.1.2-27"
IM_URL="https://github.com/ImageMagick/ImageMagick/releases/download/${IM_VERSION}/ImageMagick-${IM_VERSION}-gcc-x86_64.AppImage"
IM_SHA256="b2feb70e39f0b3ae474a0bb1ce8123811cb82f7fb80275bfb4e74018fb6cabdd"

CRANE_VERSION="v0.20.2"
CRANE_URL="https://github.com/google/go-containerregistry/releases/download/${CRANE_VERSION}/go-containerregistry_Linux_x86_64.tar.gz"
CRANE_SHA256="c14340087103ba9dadf61d45acd20675490fd0ccbd56ac7901fc1b502137f44b"

TEST_VALKEY_PORT="${BUNDLE_TEST_VALKEY_PORT:-6414}"
TEST_PG_PORT="${BUNDLE_TEST_PG_PORT:-5466}"

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

log() { echo "[build-runtime-bundle-linux] $*"; }
die() { echo "[build-runtime-bundle-linux] ERROR: $*" >&2; exit 1; }

[[ "$(uname -s)/$(uname -m)" == "Linux/x86_64" ]] \
  || die "this script targets Linux x86_64 only (got $(uname -s)/$(uname -m)); macOS uses build-runtime-bundle.sh"
for tool in python3 curl tar patchelf; do
  command -v "$tool" >/dev/null || die "$tool is required (apt-get install $tool)"
done

CACHE_DIR="$(dirname "$DEST")/.cache"
mkdir -p "$CACHE_DIR"

# ---------------------------------------------------------------------------
# JDK (JLINK_HOME env, else pinned temurin download)
# ---------------------------------------------------------------------------
if [[ -z "${JLINK_HOME:-}" ]]; then
  JDK_DIR="$CACHE_DIR/jdk-temurin-26.0.1+8"
  if [[ ! -x "$JDK_DIR/bin/jlink" ]]; then
    log "downloading pinned temurin JDK 26.0.1+8 ..."
    curl -fsSL --retry 3 -o "$CACHE_DIR/jdk.tar.gz" "$TEMURIN_URL"
    echo "$TEMURIN_SHA256  $CACHE_DIR/jdk.tar.gz" | sha256sum -c - >/dev/null \
      || die "temurin sha256 mismatch"
    mkdir -p "$JDK_DIR.extract"
    tar -xzf "$CACHE_DIR/jdk.tar.gz" -C "$JDK_DIR.extract" --strip-components 1
    mv "$JDK_DIR.extract" "$JDK_DIR"
    rm -f "$CACHE_DIR/jdk.tar.gz"
  fi
  JLINK_HOME="$JDK_DIR"
fi
JAVA_VERSION="$("$JLINK_HOME/bin/java" --version 2>/dev/null | head -1 | awk '{print $2}')" \
  || die "no java at $JLINK_HOME/bin/java"
[[ "${JAVA_VERSION%%.*}" == "$EXPECTED_JDK_MAJOR" ]] \
  || die "JDK at $JLINK_HOME is $JAVA_VERSION; need major $EXPECTED_JDK_MAJOR exactly (--enable-preview)"
[[ -x "$JLINK_HOME/bin/jlink" ]] || die "no jlink at $JLINK_HOME/bin/jlink"

FINGERPRINT="layout=2 platform=linux-x86_64 penpot=${PENPOT_VERSION} jdk=${JAVA_VERSION} valkey=${VALKEY_VERSION} imagemagick=${IM_VERSION} postgres=${POSTGRES_VERSION} node=${NODE_VERSION} exporter=${PENPOT_VERSION}-hoisted browsers=headless-shell-only"

# The headless-shell payload binary (chromium_headless_shell-<rev>/…), if present.
bundle_headless_shell() { # bundle_headless_shell <bundle-root>
  compgen -G "$1/exporter-browsers/chromium_headless_shell-*/chrome-headless-shell-linux64/chrome-headless-shell" >/dev/null 2>&1 \
    || compgen -G "$1/exporter-browsers/chromium_headless_shell-*/*/headless_shell" >/dev/null 2>&1
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

fetch_verified() { # fetch_verified <url> <sha256> <out>
  local url="$1" sha="$2" out="$3"
  if [[ -f "$out" ]] && echo "$sha  $out" | sha256sum -c - >/dev/null 2>&1; then
    return 0
  fi
  rm -f "$out"
  log "downloading $(basename "$out") ..."
  curl -fsSL --retry 3 -o "$out.part" "$url" || die "download failed: $url"
  echo "$sha  $out.part" | sha256sum -c - >/dev/null \
    || die "sha256 mismatch for $url (got $(sha256sum "$out.part" | awk '{print $1}'))"
  mv "$out.part" "$out"
}

# Bundle the non-glibc shared-lib closure of an ELF binary into <libdir> and
# set $ORIGIN rpaths (patchelf). glibc itself (libc/libm/ld-linux/…) is host-
# provided, like every AppImage (the excludelist convention).
relocate_elf() { # relocate_elf <binary> <libdir> <rpath-to-libdir>
  local bin="$1" libdir="$2" rpath="$3"
  mkdir -p "$libdir"
  local skip='^(linux-vdso|ld-linux|libc\.so|libm\.so|libpthread\.so|libdl\.so|librt\.so|libresolv\.so|libnsl\.so|libutil\.so)'
  # closure: iterate until no new libs appear
  local changed=1
  while [[ "$changed" == 1 ]]; do
    changed=0
    local targets=("$bin")
    for f in "$libdir"/*.so*; do [[ -f "$f" ]] && targets+=("$f"); done
    for t in "${targets[@]}"; do
      while read -r name path; do
        [[ -n "$path" && -f "$path" ]] || continue
        [[ "$name" =~ $skip ]] && continue
        if [[ ! -f "$libdir/$name" ]]; then
          cp "$(readlink -f "$path")" "$libdir/$name"
          chmod 755 "$libdir/$name"
          changed=1
        fi
      done < <(ldd "$t" 2>/dev/null | awk '/=>/ {print $1, $3}')
    done
  done
  patchelf --set-rpath "$rpath" "$bin"
  for f in "$libdir"/*.so*; do
    [[ -f "$f" ]] && patchelf --set-rpath '$ORIGIN' "$f"
  done
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
    command -v docker >/dev/null || die "docker CLI not found (use --no-docker)"
    docker info >/dev/null 2>&1 || die "docker daemon is not running (use --no-docker)"

    log "extracting backend from ${BACKEND_IMAGE} (docker create/cp) ..."
    CID="$(docker create --platform linux/amd64 "$BACKEND_IMAGE")"; CIDS+=("$CID")
    docker cp -q "$CID:/opt/penpot/backend" "$STAGING/backend"
    docker rm "$CID" >/dev/null; CIDS=()

    log "extracting frontend from ${FRONTEND_IMAGE} (docker create/cp) ..."
    CID="$(docker create --platform linux/amd64 "$FRONTEND_IMAGE")"; CIDS+=("$CID")
    docker cp -q "$CID:/var/www/app" "$STAGING/frontend"
    docker rm "$CID" >/dev/null; CIDS=()
  else
    CRANE="$CACHE_DIR/crane-$CRANE_VERSION"
    if [[ ! -x "$CRANE" ]]; then
      fetch_verified "$CRANE_URL" "$CRANE_SHA256" "$CACHE_DIR/crane.tar.gz"
      tar -xzf "$CACHE_DIR/crane.tar.gz" -C "$CACHE_DIR" crane
      mv "$CACHE_DIR/crane" "$CRANE"
      rm -f "$CACHE_DIR/crane.tar.gz"
    fi
    log "exporting ${BACKEND_IMAGE} via crane (no docker daemon) ..."
    mkdir -p "$STAGING/.backend-rootfs"
    "$CRANE" export "$BACKEND_IMAGE" - --platform linux/amd64 \
      | tar -xf - -C "$STAGING/.backend-rootfs" opt/penpot/backend \
      || die "crane export of $BACKEND_IMAGE failed"
    mv "$STAGING/.backend-rootfs/opt/penpot/backend" "$STAGING/backend"
    rm -rf "$STAGING/.backend-rootfs"
    log "exporting ${FRONTEND_IMAGE} via crane (no docker daemon) ..."
    mkdir -p "$STAGING/.frontend-rootfs"
    "$CRANE" export "$FRONTEND_IMAGE" - --platform linux/amd64 \
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
    LIVE_MODULES="$(docker run --rm --platform linux/amd64 --entrypoint sh "$BACKEND_IMAGE" -c "java --list-modules" \
      | sed 's/@.*//' | paste -sd, -)"
    [[ "$LIVE_MODULES" == "$EXPECTED_MODULES" ]] \
      || die "upstream module set drifted from the pin — update EXPECTED_MODULES deliberately.
live:   $LIVE_MODULES
pinned: $EXPECTED_MODULES"
    MODULES="$LIVE_MODULES"
  else
    log "no docker: using the pinned upstream module set"
  fi

  log "jdeps sanity ..."
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

  log "jlink ($JLINK_HOME) ..."
  rm -rf "$STAGING/jre"
  "$JLINK_HOME/bin/jlink" \
    --add-modules "$MODULES" \
    --strip-debug --no-man-pages --no-header-files --compress zip-6 \
    --output "$STAGING/jre"
  "$STAGING/jre/bin/java" --version >/dev/null || die "jlink output does not run"
  echo "$MODULES" | tr ',' '\n' > "$STAGING/jre/MODULES"
  log "OK jre/ ($(du -sh "$STAGING/jre" | awk '{print $1}'))"

  # ----- 3. bin/ --------------------------------------------------------------
  mkdir -p "$STAGING/bin/lib" "$STAGING/licenses"

  # valkey-server (official noble x86_64 build + bundled non-glibc closure)
  log "bin/valkey-server (valkey ${VALKEY_VERSION}, noble x86_64) ..."
  fetch_verified "$VALKEY_URL" "$VALKEY_SHA256" "$CACHE_DIR/valkey.tar.gz"
  mkdir -p "$STAGING/.valkey"
  tar -xzf "$CACHE_DIR/valkey.tar.gz" -C "$STAGING/.valkey" --strip-components 1
  cp "$STAGING/.valkey/bin/valkey-server" "$STAGING/bin/valkey-server"
  chmod 755 "$STAGING/bin/valkey-server"
  cp "$STAGING/.valkey/share/LICENSE" "$STAGING/licenses/valkey-LICENSE.txt" 2>/dev/null || true
  rm -rf "$STAGING/.valkey"
  relocate_elf "$STAGING/bin/valkey-server" "$STAGING/bin/lib" '$ORIGIN/lib'

  # identify + magick (official ImageMagick AppImage, extracted)
  log "bin/im (ImageMagick ${IM_VERSION} AppImage, extracted) ..."
  fetch_verified "$IM_URL" "$IM_SHA256" "$CACHE_DIR/imagemagick.AppImage"
  chmod +x "$CACHE_DIR/imagemagick.AppImage"
  rm -rf "$STAGING/.im-extract" && mkdir -p "$STAGING/.im-extract"
  if (cd "$STAGING/.im-extract" && "$CACHE_DIR/imagemagick.AppImage" --appimage-extract >/dev/null 2>&1); then
    :
  else
    # fallback: unsquashfs at the computed ELF-end offset (no exec needed)
    command -v unsquashfs >/dev/null || die "AppImage exec failed and no unsquashfs (apt-get install squashfs-tools)"
    OFF="$(readelf -h "$CACHE_DIR/imagemagick.AppImage" | awk '/Start of section headers/{o=$5} /Size of section headers/{s=$5} /Number of section headers/{n=$5} END{print o+s*n}')"
    unsquashfs -q -d "$STAGING/.im-extract/squashfs-root" -o "$OFF" "$CACHE_DIR/imagemagick.AppImage" >/dev/null \
      || die "unsquashfs of the ImageMagick AppImage failed"
  fi
  [[ -x "$STAGING/.im-extract/squashfs-root/AppRun" ]] || die "extracted AppImage has no AppRun"
  mv "$STAGING/.im-extract/squashfs-root" "$STAGING/bin/im"
  rm -rf "$STAGING/.im-extract"
  for l in LICENSE LICENSE.txt; do
    [[ -f "$STAGING/bin/im/usr/share/doc/ImageMagick-7/$l" ]] \
      && cp "$STAGING/bin/im/usr/share/doc/ImageMagick-7/$l" "$STAGING/licenses/imagemagick-$l"
  done
  cat > "$STAGING/bin/identify" <<'WRAPEOF'
#!/bin/sh
# penpot-runtime relocatable ImageMagick `identify` (extracted AppImage).
# AppRun sets MAGICK_HOME/MAGICK_CONFIGURE_PATH/LD_LIBRARY_PATH and execs
# usr/bin/magick with our args ("identify" first = `magick identify`).
d="$(cd "$(dirname "$0")" && pwd)"
exec "$d/im/AppRun" identify "$@"
WRAPEOF
  cat > "$STAGING/bin/magick" <<'WRAPEOF'
#!/bin/sh
d="$(cd "$(dirname "$0")" && pwd)"
exec "$d/im/AppRun" "$@"
WRAPEOF
  chmod +x "$STAGING/bin/identify" "$STAGING/bin/magick"

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

  # ----- 4. postgres/ ---------------------------------------------------------
  mkdir -p "$STAGING/postgres"
  log "postgres/ (theseus ${POSTGRES_VERSION} x86_64-unknown-linux-gnu) ..."
  fetch_verified "$PG_ARCHIVE_URL" "$PG_SHA256" "$CACHE_DIR/pg.tar.gz"
  mkdir -p "$STAGING/postgres/.extract"
  tar -xzf "$CACHE_DIR/pg.tar.gz" -C "$STAGING/postgres/.extract"
  if [[ -d "$STAGING/postgres/.extract/bin" ]]; then
    mv "$STAGING/postgres/.extract" "$STAGING/postgres/$POSTGRES_VERSION"
  else
    INNER="$(find "$STAGING/postgres/.extract" -mindepth 1 -maxdepth 1 -type d | head -1)"
    [[ -d "$INNER/bin" ]] || die "unexpected postgres archive layout"
    mv "$INNER" "$STAGING/postgres/$POSTGRES_VERSION"
    rm -rf "$STAGING/postgres/.extract"
  fi
  [[ -x "$STAGING/postgres/$POSTGRES_VERSION/bin/initdb" ]] || die "postgres payload has no initdb"

  # The theseus archive's lib/uuid-ossp.so NEEDs libossp-uuid.so.16 but does
  # NOT ship it, and carries a dead build-time RUNPATH (/opt/postgresql/lib).
  # Penpot's very first migration (0001-add-extensions) CREATEs the
  # uuid-ossp extension, so first boot hard-fails without the lib (proven:
  # CI run 29271265611). Bundle it from the build host and normalize every
  # shared object's rpath to \$ORIGIN. Other host libs the binaries use
  # (libgssapi_krb5, libxml2, libreadline) follow the AppImage excludelist
  # convention: system-provided, present on the runner and desktop distros.
  OSSP_SRC=""
  for c in /usr/lib/x86_64-linux-gnu/libossp-uuid.so.16 /usr/lib/libossp-uuid.so.16; do
    [[ -e "$c" ]] && { OSSP_SRC="$c"; break; }
  done
  [[ -n "$OSSP_SRC" ]] || die "libossp-uuid.so.16 not found on the build host (apt-get install libossp-uuid16)"
  cp "$(readlink -f "$OSSP_SRC")" "$STAGING/postgres/$POSTGRES_VERSION/lib/libossp-uuid.so.16"
  chmod 755 "$STAGING/postgres/$POSTGRES_VERSION/lib/libossp-uuid.so.16"
  while IFS= read -r -d '' so; do
    patchelf --set-rpath '$ORIGIN' "$so" 2>/dev/null || true
  done < <(find "$STAGING/postgres/$POSTGRES_VERSION/lib" -name '*.so*' -type f -print0)

  # ----- 4b. bin/node + exporter/ + exporter-browsers/ (N2) --------------------
  log "bin/node (official v$NODE_VERSION linux-x64, sha256-pinned) ..."
  fetch_verified "$NODE_URL" "$NODE_SHA256" "$CACHE_DIR/node-v$NODE_VERSION-linux-x64.tar.gz"
  NODE_EXTRACT="$STAGING/.node-extract"
  mkdir -p "$NODE_EXTRACT"
  tar -xzf "$CACHE_DIR/node-v$NODE_VERSION-linux-x64.tar.gz" -C "$NODE_EXTRACT" \
    "node-v$NODE_VERSION-linux-x64/bin/node" \
    "node-v$NODE_VERSION-linux-x64/LICENSE"
  cp "$NODE_EXTRACT/node-v$NODE_VERSION-linux-x64/bin/node" "$STAGING/bin/node"
  cp "$NODE_EXTRACT/node-v$NODE_VERSION-linux-x64/LICENSE" "$STAGING/licenses/node-LICENSE.txt"
  rm -rf "$NODE_EXTRACT"
  chmod +x "$STAGING/bin/node"
  NODE_REPORTED="$("$STAGING/bin/node" --version)"
  [[ "$NODE_REPORTED" == "v$NODE_VERSION" ]] \
    || die "bundled node reports $NODE_REPORTED, expected v$NODE_VERSION"

  # exporter/: app tree from the pinned image; node_modules rebuilt HOISTED
  # and symlink-free from the upstream lockfile — same rationale + recipe as
  # the macOS script (bundlers drop directory symlinks; naive dereferencing
  # breaks pnpm sibling resolution).
  PNPM_VERSION="11.5.3" # = the exporter package.json packageManager pin
  command -v npx >/dev/null \
    || die "npx (any host node install) is required to materialize the exporter node_modules"
  if [[ "$NO_DOCKER" -eq 0 ]]; then
    log "extracting exporter from ${EXPORTER_IMAGE} (docker create/cp) ..."
    CID="$(docker create --platform linux/amd64 "$EXPORTER_IMAGE")"; CIDS+=("$CID")
    docker cp -q "$CID:/opt/penpot/exporter" "$STAGING/.exporter-raw"
    docker rm "$CID" >/dev/null; CIDS=()
    EXPORTER_SRC="$STAGING/.exporter-raw"
  else
    log "exporting ${EXPORTER_IMAGE} via crane (no docker daemon) ..."
    CRANE="$CACHE_DIR/crane-$CRANE_VERSION"
    if [[ ! -x "$CRANE" ]]; then
      fetch_verified "$CRANE_URL" "$CRANE_SHA256" "$CACHE_DIR/crane.tar.gz"
      tar -xzf "$CACHE_DIR/crane.tar.gz" -C "$CACHE_DIR" crane
      mv "$CACHE_DIR/crane" "$CRANE"
      rm -f "$CACHE_DIR/crane.tar.gz"
    fi
    mkdir -p "$STAGING/.exporter-rootfs"
    "$CRANE" export "$EXPORTER_IMAGE" - --platform linux/amd64 \
      | tar -xf - -C "$STAGING/.exporter-rootfs" opt/penpot/exporter \
      || die "crane export of $EXPORTER_IMAGE failed"
    EXPORTER_SRC="$STAGING/.exporter-rootfs/opt/penpot/exporter"
  fi
  cp -R "$EXPORTER_SRC" "$STAGING/exporter" || die "copying the exporter app files failed"
  rm -rf "$STAGING/exporter/node_modules"
  [[ -f "$STAGING/exporter/pnpm-lock.yaml" ]] || die "exporter tree has no pnpm-lock.yaml"
  log "exporter/node_modules (pnpm@$PNPM_VERSION hoisted install from the pinned lockfile) ..."
  (cd "$STAGING/exporter" && npx -y "pnpm@$PNPM_VERSION" install --prod --frozen-lockfile \
      --ignore-scripts --config.node-linker=hoisted >/dev/null) \
    || die "pnpm hoisted install of the exporter node_modules failed"
  rm -rf "$STAGING/exporter/node_modules/.bin"
  # Prune fsevents (optional, macOS-only native dep) for parity with the macOS
  # bundle script; on Linux it is normally absent, so this is a no-op guard.
  rm -rf "$STAGING/exporter/node_modules/fsevents"
  rm -rf "$STAGING/.exporter-raw" "$STAGING/.exporter-rootfs"
  printf '%s\n' "$PENPOT_VERSION" > "$STAGING/exporter/VERSION"
  LEFT_LINKS="$(find "$STAGING/exporter" -type l | head -1)"
  [[ -z "$LEFT_LINKS" ]] \
    || die "exporter tree still contains symlinks (e.g. $LEFT_LINKS)"
  [[ -s "$STAGING/exporter/app.js" && -d "$STAGING/exporter/node_modules" ]] \
    || die "exporter payload incomplete (app.js/node_modules missing)"
  for pkg in date-fns playwright playwright-core; do
    [[ -d "$STAGING/exporter/node_modules/$pkg" ]] \
      || die "hoisted exporter node_modules is missing $pkg as a real dir"
  done
  [[ -f "$STAGING/exporter/LICENSE" ]] \
    && cp "$STAGING/exporter/LICENSE" "$STAGING/licenses/penpot-exporter-MPL-2.0.txt"

  log "exporter-browsers/ (playwright install chromium --only-shell via bundled node) ..."
  PLAYWRIGHT_BROWSERS_PATH="$STAGING/.browsers-dl" "$STAGING/bin/node" \
    "$STAGING/exporter/node_modules/playwright/cli.js" install chromium --only-shell \
    || die "playwright install chromium --only-shell failed"
  mkdir -p "$STAGING/exporter-browsers"
  DL_SHELL="$(compgen -G "$STAGING/.browsers-dl/chromium_headless_shell-*" | head -1 || true)"
  [[ -n "$DL_SHELL" ]] || die "playwright install produced no chromium_headless_shell dir"
  mv "$DL_SHELL" "$STAGING/exporter-browsers/$(basename "$DL_SHELL")"
  rm -rf "$STAGING/.browsers-dl"
  bundle_headless_shell "$STAGING" || die "no headless shell binary in the staged bundle"
  SHELL_DIR="$(compgen -G "$STAGING/exporter-browsers/chromium_headless_shell-*" | head -1)"
  HEADLESS_SHELL_REV="$(basename "$SHELL_DIR" | sed 's/^chromium_headless_shell-//')"
  touch "$SHELL_DIR/INSTALLATION_COMPLETE" 2>/dev/null || true
  find "$SHELL_DIR" -name 'LICENSE.headless_shell' -exec cp {} "$STAGING/licenses/chromium-headless-shell-LICENSE.txt" \; 2>/dev/null || true
  log "OK exporter stack: node v$NODE_VERSION + exporter/ + headless shell rev $HEADLESS_SHELL_REV"

  # ----- 5. licenses/ + MANIFEST.json + VERSION -------------------------------
  log "licenses/ ..."
  if [[ ! -f "$CACHE_DIR/penpot-LICENSE-${PENPOT_VERSION}" ]]; then
    curl -fsSL --retry 3 -o "$CACHE_DIR/penpot-LICENSE-${PENPOT_VERSION}" \
      "https://raw.githubusercontent.com/penpot/penpot/${PENPOT_VERSION}/LICENSE" \
      || die "cannot fetch the Penpot license text"
  fi
  cp "$CACHE_DIR/penpot-LICENSE-${PENPOT_VERSION}" "$STAGING/licenses/penpot-MPL-2.0.txt"
  [[ -f "$JLINK_HOME/legal/java.base/LICENSE" ]] \
    && cp "$JLINK_HOME/legal/java.base/LICENSE" "$STAGING/licenses/openjdk-GPL-2.0-with-Classpath-exception.txt"
  cp "$STAGING/postgres/$POSTGRES_VERSION/LICENSE" "$STAGING/licenses/postgresql-LICENSE.txt" 2>/dev/null || true
  cp "$STAGING/postgres/$POSTGRES_VERSION/COPYRIGHT" "$STAGING/licenses/postgresql-COPYRIGHT.txt" 2>/dev/null || true

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
    "platform": "linux-x86_64",
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
            "source": "jlink from temurin 26 (module set = upstream backend image runtime)",
            "paths": ["jre/"],
            "modules": modules,
        },
        "valkey": {
            "version": "$VALKEY_VERSION",
            "license": "BSD-3-Clause",
            "source": "download.valkey.io noble x86_64 build, non-glibc closure bundled + \$ORIGIN rpath",
            "paths": ["bin/valkey-server", "bin/lib/"],
        },
        "imagemagick": {
            "version": "$IM_VERSION",
            "license": "ImageMagick",
            "source": "official ImageMagick AppImage (gcc-x86_64), extracted",
            "paths": ["bin/identify", "bin/magick", "bin/im/"],
            "note": "relies on host system libs per the AppImage excludelist (fontconfig/X11/glib)",
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
            "source": "official nodejs.org linux-x64 tarball, sha256-pinned",
            "paths": ["bin/node"],
            "note": "exporter runtime ONLY (N2); the backend never execs node",
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
            "source": "playwright-managed chromium headless shell",
            "paths": ["exporter-browsers/"],
            "note": "headless shell ONLY; relies on host X11-adjacent system libs like any chromium",
        },
    },
    "licenses": ["licenses/" + f for f in licenses],
}
with open(os.path.join(staging, "MANIFEST.json"), "w") as f:
    json.dump(manifest, f, indent=2, sort_keys=True)
    f.write("\n")
PYEOF

  # Docker/tar extraction leaves some files read-only (jre legal docs,
  # license texts). Downstream tooling must be able to re-copy and patch
  # them (tauri-build resource re-copy; linuxdeploy's patchelf pass over
  # every ELF in the AppDir) — normalize to user-writable.
  chmod -R u+w "$STAGING"

  printf '%s' "$FINGERPRINT" > "$STAGING/.fingerprint"
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
  [[ -d "$PROOF_TMP/pgdata" ]] && env -i PATH=/usr/bin:/bin \
      "$BUNDLE/postgres/$POSTGRES_VERSION/bin/pg_ctl" -D "$PROOF_TMP/pgdata" -m immediate stop >/dev/null 2>&1 || true
  rm -rf "$PROOF_TMP"
  cleanup
}
trap proof_cleanup EXIT
ok()  { log "PROVE $1: OK $2"; }
bad() { log "PROVE $1: FAIL $2"; PROOF_FAILURES=$((PROOF_FAILURES + 1)); }

PENPOT_JAVA_OPTS=(
  -Dim4java.useV7=true
  -Djava.util.logging.manager=org.apache.logging.log4j.jul.LogManager
  -Dlog4j2.configurationFile=log4j2.xml
  -XX:-OmitStackTraceInFastThrow
  --sun-misc-unsafe-memory-access=allow
  --enable-native-access=ALL-UNNAMED
  --enable-preview
)

# P1: jlink'd java boots the jar's Clojure runtime (namespace-miss sanity)
P1_OUT="$(cd "$BUNDLE/backend" && env -i "$BUNDLE/jre/bin/java" "${PENPOT_JAVA_OPTS[@]}" \
    -jar penpot.jar -m app.bundle-sanity-check 2>&1 || true)"
if grep -q "Could not locate app/bundle_sanity_check" <<< "$P1_OUT"; then
  ok P1 "jre/bin/java boots penpot.jar's Clojure runtime"
else
  echo "$P1_OUT" | head -5 >&2
  bad P1 "expected the namespace-miss error from clojure.main"
fi

# P2: config-error-level boot reaches 'initialize connection pool'
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

# P4: identify — generated PNG + SVG, env -i from a scratch dir
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

# P5: postgres pre-seed — initdb + pg_ctl start + pg_isready + stop, env -i
PGB="$BUNDLE/postgres/$POSTGRES_VERSION/bin"
P5_OK=1
env -i PATH=/usr/bin:/bin "$PGB/initdb" --no-locale -E UTF8 -U postgres \
    -D "$PROOF_TMP/pgdata" > "$PROOF_TMP/initdb.log" 2>&1 || { P5_OK=0; tail -5 "$PROOF_TMP/initdb.log" >&2; }
if [[ "$P5_OK" -eq 1 ]]; then
  env -i PATH=/usr/bin:/bin "$PGB/pg_ctl" -D "$PROOF_TMP/pgdata" \
      -o "-p $TEST_PG_PORT -h 127.0.0.1 -k ''" -w -l "$PROOF_TMP/pg.log" start \
      > /dev/null 2>&1 || { P5_OK=0; tail -5 "$PROOF_TMP/pg.log" >&2; }
fi
P5_UUID=""
if [[ "$P5_OK" -eq 1 ]]; then
  env -i PATH=/usr/bin:/bin "$PGB/pg_isready" -h 127.0.0.1 -p "$TEST_PG_PORT" >/dev/null 2>&1 || P5_OK=0
  # penpot's first migration loads the uuid-ossp extension — prove it live
  # (this is what caught the missing libossp-uuid.so.16).
  P5_UUID="$(env -i PATH=/usr/bin:/bin "$PGB/psql" -h 127.0.0.1 -p "$TEST_PG_PORT" -U postgres -d postgres \
      -tAc 'CREATE EXTENSION "uuid-ossp"; SELECT uuid_generate_v4();' 2>&1 || true)"
  grep -qE '^[0-9a-f-]{36}$' <<< "$P5_UUID" || { echo "uuid-ossp probe: $P5_UUID" >&2; P5_OK=0; }
  env -i PATH=/usr/bin:/bin "$PGB/pg_ctl" -D "$PROOF_TMP/pgdata" -w stop >/dev/null 2>&1 || P5_OK=0
fi
if [[ "$P5_OK" -eq 1 ]]; then
  ok P5 "postgres $POSTGRES_VERSION initdb/pg_ctl/pg_isready + uuid-ossp extension (env -i)"
else
  bad P5 "postgres pre-seed is not functional"
fi

# P6: ELF relocation audit — valkey-server + bin/lib fully resolve and use
# $ORIGIN rpaths (im/ is host-lib-dependent by design; P4 is its runtime proof)
P6_OK=1
if ldd "$BUNDLE/bin/valkey-server" 2>/dev/null | grep -q "not found"; then
  ldd "$BUNDLE/bin/valkey-server" | grep "not found" >&2; P6_OK=0
fi
RPATH="$(patchelf --print-rpath "$BUNDLE/bin/valkey-server" 2>/dev/null || true)"
[[ "$RPATH" == *'$ORIGIN/lib'* ]] || { echo "valkey-server rpath=[$RPATH]" >&2; P6_OK=0; }
for f in "$BUNDLE/bin/lib"/*.so*; do
  [[ -f "$f" ]] || continue
  if ldd "$f" 2>/dev/null | grep -q "not found"; then
    echo "$f:"; ldd "$f" | grep "not found" >&2; P6_OK=0
  fi
done
if [[ "$P6_OK" -eq 1 ]]; then
  ok P6 "bin/valkey-server + bin/lib fully resolve with \$ORIGIN rpaths"
else
  bad P6 "ELF relocation audit"
fi

# P8: bundled node runs from a scratch dir with env -i and reports the pin
P8_VER="$(cd "$PROOF_TMP" && env -i PATH=/usr/bin:/bin "$BUNDLE/bin/node" --version 2>&1 || true)"
if [[ "$P8_VER" == "v$NODE_VERSION" ]]; then
  ok P8 "bin/node reports v$NODE_VERSION from scratch dir (env -i)"
else
  bad P8 "bin/node reported [$P8_VER], expected v$NODE_VERSION"
fi

# P9: exporter app boots on the BUNDLED node (own valkey, env -i, poisoned
# proxies) and answers /readyz 200
TEST_EXPORTER_PORT="${BUNDLE_TEST_EXPORTER_PORT:-6473}"
(cd "$PROOF_TMP" && exec env -i PATH=/usr/bin:/bin \
    "$BUNDLE/bin/valkey-server" --port "$TEST_VALKEY_PORT" --bind 127.0.0.1 \
    --save '' --appendonly no --dir "$PROOF_TMP" > "$PROOF_TMP/valkey-p9.log" 2>&1) &
PROOF_VALKEY_PID=$!
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
kill -9 "$PROOF_VALKEY_PID" 2>/dev/null || true
wait "$PROOF_VALKEY_PID" 2>/dev/null || true
PROOF_VALKEY_PID=""

# P10: bundled playwright launches the bundled headless shell. Chromium needs
# the usual host system libs (nss, atk, …) — present on GitHub runners and
# desktop distros; a missing-lib failure prints loudly here.
cat > "$PROOF_TMP/p10.cjs" <<'P10EOF'
const path = process.argv[2];
const { chromium } = require(path);
(async () => {
  const browser = await chromium.launch({ args: ["--allow-insecure-localhost", "--no-sandbox"] });
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
PROOF_EXPORTER_PID=""

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
