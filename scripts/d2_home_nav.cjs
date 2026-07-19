/**
 * D2: the browser leg of the front-door gate (scripts/d2-home.sh).
 *
 * Read-only apart from clicking/reading OUR OWN controls on /__home
 * (invariant 3 — nothing under runtime/frontend/ changes, no injected
 * scripts). One authenticated session for the whole run: /__bootstrap is
 * one-shot per boot (403 on a second call), so every check below that needs
 * real session data shares this single page/context rather than each
 * re-authenticating.
 *
 * Two things this proves, in order:
 *
 *  1. PROOF OF LOOKING, then D2's escape-hatch assertions on /__home
 *     (mirrors scripts/d1_surfaces.cjs's tri-state + proof-of-render idiom,
 *     scripts/routes_gate_nav.cjs's #grid/#escape-hatch checks): an empty or
 *     broken page must never read as "no dashboard". `#escape-hatch` must be
 *     ABSENT (the upstream dashboard link was removed, not just hidden) and
 *     the action controls (`button#new-project`, `button#new-file`,
 *     `button.card-action[data-action][data-file-id]`) must be PRESENT.
 *
 *  2. D0's deferred caveat, discharged: D0 (scripts/d0-navigation-spike.sh)
 *     proved the vault survives a mid-session #/dashboard redirect, but only
 *     ever measured a hand-seeded canary directory with NO workspace open.
 *     This opens a REAL file — the exact deep link read off a live board
 *     card's `data-deeplink` attribute, never hand-constructed — lets it
 *     FULLY render (a route-identifying CSS marker + a generous settle; a
 *     short settle silently lies, the D1 finding), then attempts a
 *     #/dashboard navigation and re-opens the same file afterward.
 *
 *     NOTE on scope: this leg runs in the BUNDLED BROWSER (Playwright), per
 *     the brief ("open a real file ... in the bundled browser"), not the
 *     Tauri webview. navwatch's cancel-and-redirect policy
 *     (apps/desktop/src/navwatch.rs) is Rust code wired to Tauri's
 *     `on_navigation` hook — a bare Playwright browser never exercises it.
 *     That policy is asserted separately, with the D0 mechanism, by the GUI
 *     leg of scripts/d2-home.sh (PENPOT_LOCAL_NAVWATCH_LOG). What THIS leg
 *     proves is the thing D0 couldn't: with a real workspace actually open,
 *     an attempted #/dashboard navigation doesn't touch the vault on disk
 *     (scripts/d2-home.sh hashes the vault tree before/after this whole
 *     script with n5_vaults_helper.py's tree_hash) and the file still opens.
 */
"use strict";

const path = require("path");
const REPO = process.env.REPO_ROOT || process.cwd();
const BASE = (process.env.BASE || "").replace(/\/+$/, "");

const SETTLE = Number(process.env.D2_NAV_SETTLE_MS || 12000);
const NAV_TIMEOUT = Number(process.env.D2_NAV_TIMEOUT_MS || 20000);

const HOME_GRID_MARKER = "#grid";
// Verified against the bundled build (runtime/frontend/js/*.js): the
// workspace's top-level ClojureScript component emits this literal
// CSS-module class, no build-time hash suffix — mirrors d1_surfaces.cjs's
// AUTH_PAGE_MARKER/DASHBOARD_MARKER technique for main-auth.js/main-dashboard.js.
const WORKSPACE_MARKER = ".main_ui_workspace__workspace";

const WORKSPACE_DEEPLINK_RE = /^\/#\/workspace\?team-id=[^&]+&file-id=[^&]+&page-id=[^&]+/;

function isWellFormedWorkspaceDeepLink(s) {
  return typeof s === "string" && WORKSPACE_DEEPLINK_RE.test(s);
}

// Self-check for the deep-link shape validator — no browser, no stack.
// Run as: node scripts/d2_home_nav.cjs selftest
function _selftest() {
  const cases = [
    ["/#/workspace?team-id=a&file-id=b&page-id=c", true],
    ["/#/workspace?team-id=a&file-id=b&page-id=c&extra=1", true],
    ["/#/workspace?team-id=a&file-id=b", false], // missing page-id
    ["/#/workspace?file-id=b&page-id=c", false], // missing team-id
    ["/#/dashboard/recent?team-id=a", false],
    ["not a link", false],
    ["", false],
    [null, false],
    [undefined, false],
  ];
  let failed = 0;
  for (const [s, expected] of cases) {
    const got = isWellFormedWorkspaceDeepLink(s);
    if (got !== expected) {
      failed++;
      console.error(`FAIL isWellFormedWorkspaceDeepLink(${JSON.stringify(s)}) = ${got}, expected ${expected}`);
    }
  }
  if (failed > 0) {
    console.error(`selftest FAILED: ${failed} case(s)`);
    process.exit(1);
  }
  console.log("selftest OK");
  process.exit(0);
}

if (process.argv[2] === "selftest") {
  _selftest();
} else {
  main();
}

// PROOF-OF-RENDER, waited for rather than assumed (the D0/D1 lesson: an
// absence assertion is worthless unless you first prove you were looking at
// a rendered page).
async function waitForMarker(page, selector, timeoutMs) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      if (await page.$(selector)) return true;
    } catch {
      // Mid-navigation query errors are expected while the route swaps out.
    }
    await page.waitForTimeout(500);
  }
  try {
    return (await page.$(selector)) !== null;
  } catch {
    return false;
  }
}

async function main() {
  const result = { ok: false };
  if (!BASE) {
    result.error = "BASE env var is required";
    console.log(JSON.stringify(result));
    process.exit(1);
  }

  const PW =
    process.env.PLAYWRIGHT_MODULE || path.join(REPO, "runtime/exporter/node_modules/playwright");
  const { chromium } = require(PW);
  const browser = await chromium.launch({ headless: true });

  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    page.setDefaultTimeout(NAV_TIMEOUT);

    // /__bootstrap is one-shot per boot — the ONLY call to it in this run.
    await page.goto(`${BASE}/__bootstrap`, { waitUntil: "domcontentloaded" });

    // --- (1) /__home: proof of looking, then the escape-hatch assertions --
    await page.goto(`${BASE}/__home`, { waitUntil: "domcontentloaded" });
    const homeRendered = await waitForMarker(page, HOME_GRID_MARKER, SETTLE);
    if (!homeRendered) {
      result.escapeHatch = "inconclusive";
      result.actionsPresent = "inconclusive";
      result.error =
        "/__home never rendered its board grid (" + HOME_GRID_MARKER +
        ") within the settle budget — INFRASTRUCTURE FAILURE, absence proves nothing";
      console.log(JSON.stringify(result));
      process.exit(1);
    }

    const hatchCount = await page.locator("#escape-hatch").count();
    result.escapeHatch = hatchCount === 0 ? "gone" : "present";

    const newProjectCount = await page.locator("button#new-project").count();
    const newFileCount = await page.locator("button#new-file").count();
    const cardActionCount = await page.locator("button.card-action[data-action][data-file-id]").count();
    result.actionCounts = { newProjectCount, newFileCount, cardActionCount };
    result.actionsPresent =
      newProjectCount >= 1 && newFileCount >= 1 && cardActionCount >= 1 ? "yes" : "no";

    // --- read the deep link off a REAL board card (never constructed, and
    // never a D2 placeholder) -----------------------------------------------
    // D2's `/__home` grid also renders a placeholder `a.board-card.file-card`
    // for every manifest file with zero indexed boards yet (vault-index's
    // `CardKind::File` — apps/desktop/src/home.html tags it with the extra
    // `file-card` class specifically so callers like this one can tell the
    // two apart). Excluding that class is required: a placeholder's
    // `boardId` is a synthetic `file:<uuid>` with no real board behind it, so
    // picking one here would silently weaken this leg.
    const REAL_BOARD_CARD_SELECTOR = "a.board-card:not(.file-card)[data-deeplink]";
    const realBoardCount = await page.locator(REAL_BOARD_CARD_SELECTOR).count();
    if (realBoardCount === 0) {
      result.workspaceRendered = "inconclusive";
      result.reopenRendered = "inconclusive";
      result.error =
        "no real board card on /__home (only placeholder file cards, if any) — " +
        "we could not find a board to test with";
      console.log(JSON.stringify(result));
      process.exit(1);
    }
    await page.waitForSelector(REAL_BOARD_CARD_SELECTOR, { timeout: NAV_TIMEOUT });
    const card = await page.$(REAL_BOARD_CARD_SELECTOR);
    const deepLink = card ? await card.getAttribute("data-deeplink") : null;
    result.deepLink = deepLink || null;
    if (!isWellFormedWorkspaceDeepLink(deepLink)) {
      result.workspaceRendered = "inconclusive";
      result.reopenRendered = "inconclusive";
      result.error = "no well-formed workspace deep link on any board card: " + JSON.stringify(deepLink);
      console.log(JSON.stringify(result));
      process.exit(1);
    }

    // --- (2) D0's deferred caveat: open a REAL file, let it fully render --
    await page.goto(BASE + deepLink, { waitUntil: "domcontentloaded" });
    const workspaceRendered = await waitForMarker(page, WORKSPACE_MARKER, SETTLE);
    result.workspaceRendered = workspaceRendered ? "yes" : "inconclusive";
    if (!workspaceRendered) {
      result.error =
        "workspace deep link never rendered its route marker (" + WORKSPACE_MARKER +
        ") within the settle budget";
      console.log(JSON.stringify(result));
      process.exit(1);
    }

    // --- trigger a #/dashboard navigation (same-document hash change, the
    // exact technique D0's own /__navprobe used — see the module doc above
    // for what this bundled-browser leg does and does not prove).
    await page.evaluate(() => {
      window.location.hash = "#/dashboard";
    });
    await page.waitForTimeout(1500);
    result.dashboardHashTried = true;
    result.urlAfterDashboardNav = page.url();

    // --- "the file still opens": re-navigate to the SAME deep link --------
    await page.goto(BASE + deepLink, { waitUntil: "domcontentloaded" });
    const reopenRendered = await waitForMarker(page, WORKSPACE_MARKER, SETTLE);
    result.reopenRendered = reopenRendered ? "yes" : "inconclusive";

    result.ok =
      result.escapeHatch === "gone" &&
      result.actionsPresent === "yes" &&
      result.workspaceRendered === "yes" &&
      result.reopenRendered === "yes";
    console.log(JSON.stringify(result));
    process.exit(result.ok ? 0 : 1);
  } catch (e) {
    result.error = String(e && e.message ? e.message : e);
    console.log(JSON.stringify(result));
    process.exit(1);
  } finally {
    await browser.close();
  }
}
