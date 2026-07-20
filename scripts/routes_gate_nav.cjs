#!/usr/bin/env node
/*
 * routes-gate live navigation leg (PLAN2.md risk 2 / milestone N3).
 *
 * Drives the BUNDLED chromium (N2's playwright + exporter-browsers, fully
 * offline) against a running stack to prove the two upstream hash-route
 * couplings the lighttable depends on still resolve as real navigations:
 *
 *   1. Load /__home, click ONE board card, assert the landed URL is the exact
 *      /#/workspace?team-id&file-id&page-id deep link the card advertised
 *      (read from its data-deeplink attribute — same string the
 *      /__api/vault/boards payload emitted).
 *   2. (D2) Assert the escape hatch to the upstream dashboard is ABSENT from
 *      /__home. navwatch.rs now cancels #/dashboard unconditionally and
 *      redirects back here, so the link was removed rather than left as a
 *      visible control the navigation policy silently cancels. This leg used
 *      to click the hatch and assert it landed on /#/dashboard/recent; it now
 *      asserts the inverse so the hatch cannot be reintroduced unnoticed.
 *   3. (N4) Build the viewer deep link for the first board from the
 *      /__api/vault/boards payload (file-id, page-id, frame-id=board-id,
 *      section=interactions — the Peek "Present" route) and assert it commits
 *      as a /#/view navigation. This covers the ONE new upstream coupling N4
 *      adds beyond N3's workspace/dashboard shapes.
 *
 * We assert on navigation COMMIT (not full SPA load) so the heavy ClojureScript
 * app never has to finish booting — the URL is what the gate is about.
 *
 * Usage:  node routes_gate_nav.cjs <base-url>
 * Env:    ROUTES_GATE_PLAYWRIGHT  path to the playwright module
 *         PLAYWRIGHT_BROWSERS_PATH  the bundled exporter-browsers dir
 * Prints: one JSON line {ok, workspace, dashboard, ...}; exit 0 on all-pass.
 */
"use strict";

const path = require("path");
const REPO = path.resolve(__dirname, "..");
const PW =
  process.env.ROUTES_GATE_PLAYWRIGHT ||
  path.join(REPO, "runtime/exporter/node_modules/playwright");
const { chromium } = require(PW);

const BASE = (process.argv[2] || "").replace(/\/+$/, "");
if (!BASE) {
  console.error("usage: routes_gate_nav.cjs <base-url>");
  process.exit(64);
}
const NAV_TIMEOUT = parseInt(process.env.ROUTES_GATE_TIMEOUT_MS || "20000", 10);

function fail(msg) {
  console.log(JSON.stringify({ ok: false, error: msg }));
  process.exit(1);
}

(async () => {
  const browser = await chromium.launch({ headless: true });
  const result = { ok: false };
  try {
    const page = await browser.newPage();
    page.setDefaultTimeout(NAV_TIMEOUT);

    // --- (1) card click -> /#/workspace deep link ------------------------
    await page.goto(BASE + "/__home", { waitUntil: "domcontentloaded" });
    // Wait for the grid to render at least one card with a deep link.
    await page.waitForSelector("a.board-card[data-deeplink]", { timeout: NAV_TIMEOUT });
    const card = await page.$("a.board-card[data-deeplink]");
    const deepLink = await card.getAttribute("data-deeplink");
    if (!deepLink || !/^\/#\/workspace\?team-id=[^&]+&file-id=[^&]+/.test(deepLink)) {
      fail("first card data-deeplink is not a well-formed workspace deep link: " + deepLink);
    }
    const expectedWorkspace = BASE + deepLink;
    // Click and wait only for navigation COMMIT (no SPA boot).
    await Promise.all([
      page.waitForURL((u) => u.toString().includes("#/workspace"), {
        waitUntil: "commit",
        timeout: NAV_TIMEOUT,
      }),
      card.click(),
    ]);
    const landedWorkspace = page.url();
    result.workspace = {
      expected: expectedWorkspace,
      landed: landedWorkspace,
      pass: landedWorkspace === expectedWorkspace,
    };
    if (!result.workspace.pass) {
      fail(
        "card click landed on " +
          landedWorkspace +
          " but the deep link was " +
          expectedWorkspace
      );
    }

    // --- (2) escape hatch is ABSENT (D2 closed the dashboard) ------------
    await page.goto(BASE + "/__home", { waitUntil: "domcontentloaded" });
    // Give the page a moment to finish its initial render before asserting
    // absence — otherwise a slow-to-render page could false-pass.
    await page.waitForSelector("#grid", { timeout: NAV_TIMEOUT });
    const hatchCount = await page.locator("#escape-hatch").count();
    result.dashboard = { hatchCount: hatchCount, pass: hatchCount === 0 };
    if (!result.dashboard.pass) {
      fail("#escape-hatch is present on /__home (" + hatchCount + " match(es)) — it must stay removed now that #/dashboard is cancelled unconditionally");
    }

    // --- (3) N4 viewer route -> /#/view (Peek "Present") -----------------
    // MUST be a real board (kind === "board"), never D2's placeholder file
    // card (kind === "file", boardId = "file:<uuid>", vault-index's
    // `CardKind::File` — see crates/vault-index/src/boards.rs). frame-id in
    // the viewer URL below is that boardId: a placeholder's synthetic
    // "file:<uuid>" still commits as a /#/view navigation (Penpot doesn't
    // validate frame-id exists before routing), which would make this leg
    // pass while pointing at a frame that does not exist — "could not find a
    // board to test with" must never read as "the test passed", so this
    // fails loudly instead of silently falling back to the first card.
    await page.goto(BASE + "/__home", { waitUntil: "domcontentloaded" });
    const board = await page.evaluate(async () => {
      const r = await fetch("/__api/vault/boards");
      if (!r.ok) return null;
      const j = await r.json();
      const boards = (j.boards || []).filter((b) => b && b.kind === "board");
      return boards[0] || null;
    });
    if (!board || !board.fileId || !board.pageId || !board.boardId) {
      fail("could not find a real board to test with (only placeholder file cards, if any, were indexed)");
    }
    const viewerLink =
      "/#/view?file-id=" + encodeURIComponent(board.fileId) +
      "&page-id=" + encodeURIComponent(board.pageId) +
      "&frame-id=" + encodeURIComponent(board.boardId) +
      "&section=interactions";
    const expectedViewer = BASE + viewerLink;
    await Promise.all([
      page.waitForURL((u) => u.toString().includes("#/view"), {
        waitUntil: "commit",
        timeout: NAV_TIMEOUT,
      }),
      page.evaluate((href) => { window.location.assign(href); }, viewerLink),
    ]);
    const landedViewer = page.url();
    result.viewer = {
      expected: expectedViewer,
      landed: landedViewer,
      pass: landedViewer === expectedViewer,
    };
    if (!result.viewer.pass) {
      fail("viewer link landed on " + landedViewer + " but expected " + expectedViewer);
    }

    result.ok = true;
    console.log(JSON.stringify(result));
  } catch (e) {
    fail(String(e && e.message ? e.message : e));
  } finally {
    await browser.close();
  }
})();
