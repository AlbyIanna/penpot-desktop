#!/usr/bin/env bash
#
# fetch-penpot.sh — materialize the pinned Penpot runtime into runtime/
#
# Extracts files from the pinned docker images WITHOUT running any container
# (docker create + docker cp + docker rm only):
#
#   runtime/backend/              <- penpotapp/backend:$PENPOT_VERSION /opt/penpot/backend
#                                    (penpot.jar, run.sh, log4j2.xml, manage.py,
#                                     builtin-templates/, scripts/, version.txt)
#   runtime/frontend/             <- penpotapp/frontend:$PENPOT_VERSION /var/www/app
#                                    (static SPA; js/config.js is patched at container
#                                     boot upstream — our proxy must do the equivalent)
#   runtime/nginx-reference.conf  <- the image's /tmp/nginx.conf.template + override
#                                    snippets, concatenated as reference docs for the proxy
#   runtime/VERSION               <- the pinned tag
#   runtime/exporter/             <- penpotapp/exporter:$PENPOT_VERSION /opt/penpot/exporter
#                                    (app.js single bundle + node_modules — pure JS,
#                                     zero native .node bindings, so the linux image's
#                                     node_modules run on macOS as-is; M5 exporter spike)
#   runtime/exporter/VERSION      <- the pinned tag (separate idempotency check, so an
#                                    existing runtime/ gains the exporter without --force)
#   runtime/exporter-browsers/    <- OPTIONAL (--with-browsers): playwright-managed
#                                    chromium for the exporter (~500 MB on disk; the
#                                    exporter launches playwright's bundled chromium —
#                                    the system Chrome is NOT used). Needs a host node.
#
# Idempotent: if runtime/VERSION already matches $PENPOT_VERSION and the key
# artifacts exist, extraction is skipped (pass --force to re-extract). The
# exporter extraction has its own VERSION check with the same semantics.
# Extraction goes to a staging dir and is swapped in only on success, so an
# interrupted run never leaves a half-populated runtime/.
#
# The final verification step ALWAYS runs (even on skip): jar size, frontend
# index.html, and a host-JVM launch sanity check of penpot.jar.
#
# Usage: scripts/fetch-penpot.sh [--force] [--no-java-check] [--no-exporter] [--with-browsers]
# Env:   PENPOT_VERSION (default 2.16.2), JAVA_CMD (default /opt/homebrew/opt/openjdk/bin/java),
#        NODE_CMD (default /opt/homebrew/bin/node — used only by --with-browsers)

set -euo pipefail

PENPOT_VERSION="${PENPOT_VERSION:-2.16.2}"
BACKEND_IMAGE="penpotapp/backend:${PENPOT_VERSION}"
FRONTEND_IMAGE="penpotapp/frontend:${PENPOT_VERSION}"
EXPORTER_IMAGE="penpotapp/exporter:${PENPOT_VERSION}"
JAVA_CMD="${JAVA_CMD:-/opt/homebrew/opt/openjdk/bin/java}"
NODE_CMD="${NODE_CMD:-/opt/homebrew/bin/node}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNTIME_DIR="${REPO_ROOT}/runtime"

# The exact JVM flags the backend image's run.sh exports as JAVA_OPTS.
# This is the contract the supervisor must replicate when launching penpot.jar.
# (Source: /opt/penpot/backend/run.sh in penpotapp/backend:2.16.2; the image runs
#  `exec $JAVA_CMD $JAVA_OPTS -jar penpot.jar -m app.main` with cwd /opt/penpot/backend.)
PENPOT_JAVA_OPTS=(
  -Dim4java.useV7=true
  -Djava.util.logging.manager=org.apache.logging.log4j.jul.LogManager
  -Dlog4j2.configurationFile=log4j2.xml
  -XX:-OmitStackTraceInFastThrow
  --sun-misc-unsafe-memory-access=allow
  --enable-native-access=ALL-UNNAMED
  --enable-preview
)

FORCE=0
JAVA_CHECK=1
EXPORTER=1
BROWSERS=0
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=1 ;;
    --no-java-check) JAVA_CHECK=0 ;;
    --no-exporter) EXPORTER=0 ;;
    --with-browsers) BROWSERS=1 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

log() { echo "[fetch-penpot] $*"; }
die() { echo "[fetch-penpot] ERROR: $*" >&2; exit 1; }

command -v docker >/dev/null 2>&1 || die "docker CLI not found on PATH"
docker info >/dev/null 2>&1 || die "docker daemon is not running"

# ---------------------------------------------------------------------------
# Skip check (idempotency)
# ---------------------------------------------------------------------------
runtime_is_current() {
  [[ -f "${RUNTIME_DIR}/VERSION" ]] \
    && [[ "$(cat "${RUNTIME_DIR}/VERSION")" == "${PENPOT_VERSION}" ]] \
    && [[ -s "${RUNTIME_DIR}/backend/penpot.jar" ]] \
    && [[ -f "${RUNTIME_DIR}/frontend/index.html" ]] \
    && [[ -f "${RUNTIME_DIR}/nginx-reference.conf" ]]
}

if [[ "$FORCE" -eq 0 ]] && runtime_is_current; then
  log "runtime/ already at ${PENPOT_VERSION} — skipping extraction (use --force to re-extract)"
else
  # -------------------------------------------------------------------------
  # Extraction (docker create / docker cp / docker rm — containers never run)
  # -------------------------------------------------------------------------
  mkdir -p "${RUNTIME_DIR}"
  STAGING="$(mktemp -d "${RUNTIME_DIR}/.staging.XXXXXX")"

  CIDS=()
  cleanup() {
    for cid in "${CIDS[@]-}"; do
      [[ -n "$cid" ]] && docker rm -f "$cid" >/dev/null 2>&1 || true
    done
    rm -rf "${STAGING}"
  }
  trap cleanup EXIT

  log "extracting backend from ${BACKEND_IMAGE} ..."
  BACKEND_CID="$(docker create "${BACKEND_IMAGE}")"
  CIDS+=("${BACKEND_CID}")
  docker cp -q "${BACKEND_CID}:/opt/penpot/backend" "${STAGING}/backend"
  docker rm "${BACKEND_CID}" >/dev/null
  CIDS=()

  log "extracting frontend from ${FRONTEND_IMAGE} ..."
  FRONTEND_CID="$(docker create "${FRONTEND_IMAGE}")"
  CIDS+=("${FRONTEND_CID}")
  docker cp -q "${FRONTEND_CID}:/var/www/app" "${STAGING}/frontend"
  # nginx reference material: the pre-envsubst template is the authoritative
  # upstream config (what /entrypoint.sh renders into /etc/nginx/nginx.conf),
  # plus the override snippets it includes.
  docker cp -q "${FRONTEND_CID}:/tmp/nginx.conf.template" "${STAGING}/nginx.conf.template"
  docker cp -q "${FRONTEND_CID}:/etc/nginx/overrides" "${STAGING}/nginx-overrides"
  docker cp -q "${FRONTEND_CID}:/etc/nginx/nginx-security-headers.conf" "${STAGING}/nginx-security-headers.conf" 2>/dev/null || true
  docker rm "${FRONTEND_CID}" >/dev/null
  CIDS=()

  # Assemble the single-file nginx reference document.
  {
    echo "# ============================================================================"
    echo "# nginx-reference.conf — REFERENCE ONLY, never loaded by anything."
    echo "#"
    echo "# Extracted from ${FRONTEND_IMAGE} by scripts/fetch-penpot.sh."
    echo "# This is the upstream nginx config the desktop proxy (crates/proxy) must"
    echo "# functionally replicate: SPA serving, /api and /ws proxying, and the"
    echo "# X-Accel internal asset-serving contract (see docs/m0/asset-serving.md)."
    echo "#"
    echo "# Section 1 is /tmp/nginx.conf.template (pre-envsubst; \${PENPOT_*} vars are"
    echo "# substituted by the image's /entrypoint.sh at container boot)."
    echo "# Subsequent sections are the include snippets under /etc/nginx/overrides/"
    echo "# and /etc/nginx/nginx-security-headers.conf."
    echo "# ============================================================================"
    echo
    echo "# ===== /tmp/nginx.conf.template ============================================="
    cat "${STAGING}/nginx.conf.template"
    if [[ -f "${STAGING}/nginx-security-headers.conf" ]]; then
      echo
      echo "# ===== /etc/nginx/nginx-security-headers.conf ==============================="
      cat "${STAGING}/nginx-security-headers.conf"
    fi
    while IFS= read -r -d '' snippet; do
      rel="${snippet#"${STAGING}/nginx-overrides/"}"
      echo
      echo "# ===== /etc/nginx/overrides/${rel} ========================================="
      cat "${snippet}"
    done < <(find "${STAGING}/nginx-overrides" -type f -name '*.conf' -print0 | sort -z)
  } > "${STAGING}/nginx-reference.conf"
  rm -rf "${STAGING}/nginx-overrides" "${STAGING}/nginx.conf.template" "${STAGING}/nginx-security-headers.conf"

  printf '%s\n' "${PENPOT_VERSION}" > "${STAGING}/VERSION"

  # Swap staging into place (per-entry: keep runtime/README.md etc. intact).
  log "swapping extracted artifacts into ${RUNTIME_DIR} ..."
  for entry in backend frontend nginx-reference.conf VERSION; do
    rm -rf "${RUNTIME_DIR:?}/${entry}"
    mv "${STAGING}/${entry}" "${RUNTIME_DIR}/${entry}"
  done

  trap - EXIT
  cleanup 2>/dev/null || true
  log "extraction complete"
fi

# ---------------------------------------------------------------------------
# Exporter extraction (M5; separately versioned so an existing runtime/ gains
# it without --force). Same docker create/cp/rm dance — no container runs.
# ---------------------------------------------------------------------------
exporter_is_current() {
  [[ -f "${RUNTIME_DIR}/exporter/VERSION" ]] \
    && [[ "$(cat "${RUNTIME_DIR}/exporter/VERSION")" == "${PENPOT_VERSION}" ]] \
    && [[ -s "${RUNTIME_DIR}/exporter/app.js" ]] \
    && [[ -d "${RUNTIME_DIR}/exporter/node_modules" ]]
}

if [[ "$EXPORTER" -eq 1 ]]; then
  if [[ "$FORCE" -eq 0 ]] && exporter_is_current; then
    log "runtime/exporter already at ${PENPOT_VERSION} — skipping extraction"
  else
    mkdir -p "${RUNTIME_DIR}"
    EXP_STAGING="$(mktemp -d "${RUNTIME_DIR}/.staging-exporter.XXXXXX")"
    EXP_CID=""
    exp_cleanup() {
      [[ -n "${EXP_CID}" ]] && docker rm -f "${EXP_CID}" >/dev/null 2>&1 || true
      rm -rf "${EXP_STAGING}"
      return 0
    }
    trap exp_cleanup EXIT
    log "extracting exporter from ${EXPORTER_IMAGE} ..."
    EXP_CID="$(docker create "${EXPORTER_IMAGE}")"
    docker cp -q "${EXP_CID}:/opt/penpot/exporter" "${EXP_STAGING}/exporter"
    docker rm "${EXP_CID}" >/dev/null
    EXP_CID=""
    printf '%s\n' "${PENPOT_VERSION}" > "${EXP_STAGING}/exporter/VERSION"
    rm -rf "${RUNTIME_DIR:?}/exporter"
    mv "${EXP_STAGING}/exporter" "${RUNTIME_DIR}/exporter"
    trap - EXIT
    exp_cleanup 2>/dev/null || true
    log "exporter extraction complete"
  fi

  [[ -s "${RUNTIME_DIR}/exporter/app.js" ]] \
    || die "verification failed: runtime/exporter/app.js missing"
  log "OK exporter/app.js"

  # Playwright chromium for the exporter (~500 MB on disk after download).
  # The compiled exporter calls playwright's chromium.launch() with no
  # executablePath/channel, so the playwright-managed browser is the only
  # no-code-change option (system Chrome is NOT used). Opt-in because of the
  # download size; the app's boot pre-flight points here when it's missing.
  BROWSERS_DIR="${RUNTIME_DIR}/exporter-browsers"
  browsers_present() {
    compgen -G "${BROWSERS_DIR}/chromium*" >/dev/null 2>&1
  }
  if [[ "$BROWSERS" -eq 1 ]]; then
    if browsers_present && [[ "$FORCE" -eq 0 ]]; then
      log "exporter browsers already present under ${BROWSERS_DIR} — skipping"
    else
      [[ -x "${NODE_CMD}" ]] || die "node not found at ${NODE_CMD} (set NODE_CMD; needed for --with-browsers)"
      log "installing playwright chromium into ${BROWSERS_DIR} (large download) ..."
      (cd "${RUNTIME_DIR}/exporter" \
        && PLAYWRIGHT_BROWSERS_PATH="${BROWSERS_DIR}" \
           "${NODE_CMD}" node_modules/playwright/cli.js install chromium)
      browsers_present || die "playwright install finished but no chromium* under ${BROWSERS_DIR}"
    fi
    log "OK exporter-browsers"
  elif ! browsers_present; then
    log "NOTE: exporter browsers not installed (run with --with-browsers to enable server-side renders)"
  fi
fi

# ---------------------------------------------------------------------------
# Verification (always runs)
# ---------------------------------------------------------------------------
JAR="${RUNTIME_DIR}/backend/penpot.jar"
[[ -f "${JAR}" ]] || die "verification failed: ${JAR} does not exist"
JAR_SIZE="$(wc -c < "${JAR}" | tr -d ' ')"
[[ "${JAR_SIZE}" -gt 50000000 ]] \
  || die "verification failed: penpot.jar is suspiciously small (${JAR_SIZE} bytes)"
log "OK backend/penpot.jar (${JAR_SIZE} bytes)"

[[ -f "${RUNTIME_DIR}/frontend/index.html" ]] \
  || die "verification failed: frontend/index.html missing"
log "OK frontend/index.html"

IMAGE_VERSION="$(tr -d '[:space:]' < "${RUNTIME_DIR}/backend/version.txt" 2>/dev/null || true)"
if [[ -n "${IMAGE_VERSION}" && "${IMAGE_VERSION}" != "${PENPOT_VERSION}" ]]; then
  die "backend/version.txt says '${IMAGE_VERSION}' but pinned version is '${PENPOT_VERSION}'"
fi
log "OK backend/version.txt matches pin (${PENPOT_VERSION})"

if [[ "${JAVA_CHECK}" -eq 1 ]]; then
  [[ -x "${JAVA_CMD}" ]] || die "java not found at ${JAVA_CMD} (set JAVA_CMD to override)"
  # Launch sanity check that needs no postgres/valkey: the jar's Main-Class is
  # clojure.main and '-m <ns>' selects the entry namespace. Pointing it at a
  # namespace that does not exist makes clojure.main boot the full Clojure
  # runtime from the jar (which requires the JVM to accept its preview-versioned
  # class files) and then exit fast with "Could not locate ...". Seeing that
  # message proves the host JVM + jar pair launches.
  log "java launch sanity check (${JAVA_CMD}) ..."
  set +e
  SANITY_OUT="$(cd "${RUNTIME_DIR}/backend" && "${JAVA_CMD}" "${PENPOT_JAVA_OPTS[@]}" \
                  -jar penpot.jar -m app.fetch-penpot-sanity-check 2>&1)"
  set -e
  if grep -q "Could not locate app/fetch_penpot_sanity_check" <<< "${SANITY_OUT}"; then
    log "OK host JVM launches penpot.jar (clojure runtime booted, expected namespace-miss error)"
  else
    echo "${SANITY_OUT}" >&2
    die "java launch sanity check failed — output above (JDK major must exactly match the jar's --enable-preview class version)"
  fi
fi

# ---------------------------------------------------------------------------
# README (regenerated every run — runtime/ is gitignored, so this is the only
# way the README survives a fresh checkout)
# ---------------------------------------------------------------------------
cat > "${RUNTIME_DIR}/README.md" <<EOF
# runtime/ — extracted Penpot ${PENPOT_VERSION} artifacts

Everything in this directory is machine-generated by \`scripts/fetch-penpot.sh\`
(including this README). It is **gitignored** — never commit it, never hand-edit
it. To refresh:

\`\`\`sh
scripts/fetch-penpot.sh            # no-op if VERSION already matches the pin
scripts/fetch-penpot.sh --force    # re-extract even if current
PENPOT_VERSION=x.y.z scripts/fetch-penpot.sh   # different pin (deliberate, tested step —
                                               # re-run the M0 round-trip script first, see PLAN.md risk 3)
\`\`\`

The script only needs the docker daemon (docker create/cp/rm — it never runs a
container) and, for the launch sanity check, a host JDK whose major version
exactly matches the jar's \`--enable-preview\` build (JDK 26 for Penpot 2.16.2;
override the binary with \`JAVA_CMD=...\`, skip with \`--no-java-check\`).

## Contents

| Path | From | What |
|---|---|---|
| \`backend/\` | \`penpotapp/backend:${PENPOT_VERSION}\` \`/opt/penpot/backend\` | \`penpot.jar\` (~110 MB uberjar), \`run.sh\` (upstream launcher — the JVM-flags contract), \`log4j2.xml\`, \`manage.py\` (user provisioning via prepl), \`builtin-templates/\`, \`scripts/\`, \`version.txt\` |
| \`frontend/\` | \`penpotapp/frontend:${PENPOT_VERSION}\` \`/var/www/app\` | static SPA. Upstream's entrypoint rewrites \`js/config.js\` at boot (injects \`penpotFlags\` / \`penpotPublicURI\`); our proxy must do the equivalent when serving it. |
| \`nginx-reference.conf\` | frontend image \`/tmp/nginx.conf.template\` + \`/etc/nginx/overrides/*\` | reference-only concatenation of the upstream nginx config the proxy crate must functionally replicate (SPA, \`/api\`, \`/ws/notifications\`, X-Accel \`/internal/assets\`). Never loaded by anything. |
| \`VERSION\` | pin | the extracted tag; the idempotency check compares against it |
| \`exporter/\` | \`penpotapp/exporter:${PENPOT_VERSION}\` \`/opt/penpot/exporter\` | \`app.js\` compiled bundle + \`node_modules\` (pure JS — the linux modules run on macOS as-is). Dev-mode only (M5): run with a host node (upstream image pins v24.16.0; v25 verified working). Skip with \`--no-exporter\`. |
| \`exporter-browsers/\` | playwright download (\`--with-browsers\`) | playwright-managed chromium for the exporter (\`PLAYWRIGHT_BROWSERS_PATH\`). The exporter never uses the system Chrome. |

## JVM launch contract (what the supervisor must replicate)

From \`backend/run.sh\` (cwd must be \`runtime/backend/\` — \`log4j2.configurationFile\`
and the builtin-templates are resolved relative to it):

\`\`\`
java -Dim4java.useV7=true \\
     -Djava.util.logging.manager=org.apache.logging.log4j.jul.LogManager \\
     -Dlog4j2.configurationFile=log4j2.xml \\
     -XX:-OmitStackTraceInFastThrow \\
     --sun-misc-unsafe-memory-access=allow \\
     --enable-native-access=ALL-UNNAMED \\
     --enable-preview \\
     \$JVM_OPTS \$JAVA_OPTS \\
     -jar penpot.jar -m app.main
\`\`\`

The jar's Main-Class is \`clojure.main\`; \`-m app.main\` selects the entry
namespace. \`--enable-preview\` hard-fails unless the JVM major exactly matches
the JDK the jar was compiled on (26). Configuration is entirely via
\`PENPOT_*\` env vars (see \`m0/docker/docker-compose.yaml\` for the known-good
set). Note: \`app.main\` starts an nrepl server on port 6064 by default.
EOF
log "wrote ${RUNTIME_DIR}/README.md"

log "runtime/ ready at version ${PENPOT_VERSION}"
