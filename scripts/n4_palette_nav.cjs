#!/usr/bin/env node
/*
 * N4 palette + grid-fix live navigation legs (PLAN2.md milestone N4).
 *
 * Drives the BUNDLED chromium (N2's playwright + exporter-browsers, offline)
 * against a running stack to prove the two headless-gateable N4 behaviours:
 *
 *   1. PALETTE ENTER → DEEP LINK. Load /__palette, type NEEDLE, wait for the
 *      ranked list, press Enter, assert the committed URL is the exact
 *      /#/workspace deep link the top hit advertised (== the /__api/vault/
 *      palette first-hit deepLink).
 *   2. GRID FIX HOLDS SCROLL + FOCUS. Load /__home (1000-board fixture),
 *      scroll down, focus a specific card, record (scrollY, board-id), wait
 *      out one periodic refresh (the setInterval(loadBoards) that used to
 *      innerHTML="" the grid), and assert scrollY is unchanged AND the same
 *      card still holds focus. This is the N3-debt diff/patch fix.
 *
 * Usage:  node n4_palette_nav.cjs <base-url> <needle>
 * Env:    ROUTES_GATE_PLAYWRIGHT, PLAYWRIGHT_BROWSERS_PATH  (as routes-gate)
 * Prints: one JSON line {ok, palette, grid}; exit 0 on all-pass.
 */
"use strict";

const path = require("path");
const REPO = path.resolve(__dirname, "..");
const PW =
  process.env.ROUTES_GATE_PLAYWRIGHT ||
  path.join(REPO, "runtime/exporter/node_modules/playwright");
const { chromium } = require(PW);

const BASE = (process.argv[2] || "").replace(/\/+$/, "");
const NEEDLE = process.argv[3] || "";
if (!BASE || !NEEDLE) {
  console.error("usage: n4_palette_nav.cjs <base-url> <needle>");
  process.exit(64);
}
const T = parseInt(process.env.ROUTES_GATE_TIMEOUT_MS || "20000", 10);
// The home page refreshes the grid on this interval; wait a hair past it.
const REFRESH_MS = parseInt(process.env.N4_GRID_REFRESH_MS || "5000", 10);

function fail(msg) {
  console.log(JSON.stringify({ ok: false, error: msg }));
  process.exit(1);
}

(async () => {
  const browser = await chromium.launch({ headless: true });
  const result = { ok: false };
  try {
    const page = await browser.newPage();
    page.setDefaultTimeout(T);

    // --- (1) palette Enter -> exact deep link ----------------------------
    const expected = await page.evaluate(async (needle) => {
      const r = await fetch("/__api/vault/palette?q=" + encodeURIComponent(needle) + "&limit=40");
      if (!r.ok) return null;
      const j = await r.json();
      return j.hits && j.hits[0] ? j.hits[0].deepLink : null;
    }, NEEDLE).catch(() => null);
    // ^ runs in about:blank; redo against the origin below if null.

    await page.goto(BASE + "/__palette", { waitUntil: "domcontentloaded" });
    const expected2 = expected || (await page.evaluate(async (needle) => {
      const r = await fetch("/__api/vault/palette?q=" + encodeURIComponent(needle) + "&limit=40");
      const j = await r.json();
      return j.hits && j.hits[0] ? j.hits[0].deepLink : null;
    }, NEEDLE));
    if (!expected2 || !/^\/#\/workspace\?team-id=/.test(expected2)) {
      fail("palette API first hit is not a workspace deep link: " + expected2);
    }
    await page.fill("#q", NEEDLE);
    // Wait until the top result row carries the expected deep link.
    await page.waitForFunction(
      (dl) => {
        const li = document.querySelector("#results li.hit");
        return li && li.getAttribute("data-deeplink") === dl;
      },
      expected2,
      { timeout: T }
    );
    const expectedUrl = BASE + expected2;
    await Promise.all([
      page.waitForURL((u) => u.toString().includes("#/workspace"), {
        waitUntil: "commit",
        timeout: T,
      }),
      page.press("#q", "Enter"),
    ]);
    const landed = page.url();
    result.palette = { expected: expectedUrl, landed, pass: landed === expectedUrl };
    if (!result.palette.pass) {
      fail("palette Enter landed on " + landed + " but expected " + expectedUrl);
    }

    // --- (2) grid refresh preserves scroll + focus -----------------------
    await page.goto(BASE + "/__home", { waitUntil: "domcontentloaded" });
    await page.waitForSelector("a.board-card[data-board-id]", { timeout: T });
    // Need enough cards to scroll; the gate seeds the 1000-board fixture.
    const before = await page.evaluate(() => {
      // Scroll well down the grid.
      window.scrollTo(0, Math.floor(document.body.scrollHeight * 0.5));
      const cards = document.querySelectorAll("a.board-card[data-board-id]");
      // Pick a card near the current viewport to focus.
      const idx = Math.min(cards.length - 1, Math.floor(cards.length * 0.5));
      const card = cards[idx];
      card.focus();
      return {
        scrollY: window.scrollY,
        boardId: card.getAttribute("data-board-id"),
        activeId: document.activeElement && document.activeElement.getAttribute
          ? document.activeElement.getAttribute("data-board-id")
          : null,
        cardCount: cards.length,
      };
    });
    if (before.scrollY < 50) {
      fail("grid did not scroll (need the 1000-board fixture); scrollY=" + before.scrollY);
    }
    if (before.activeId !== before.boardId) {
      fail("focus did not land on the chosen card");
    }
    // Wait out at least one periodic refresh cycle.
    await page.waitForTimeout(REFRESH_MS + 1200);
    const after = await page.evaluate(() => ({
      scrollY: window.scrollY,
      activeId: document.activeElement && document.activeElement.getAttribute
        ? document.activeElement.getAttribute("data-board-id")
        : null,
      cardCount: document.querySelectorAll("a.board-card[data-board-id]").length,
    }));
    const scrollHeld = Math.abs(after.scrollY - before.scrollY) <= 2;
    const focusHeld = after.activeId === before.boardId;
    result.grid = {
      scrollBefore: before.scrollY,
      scrollAfter: after.scrollY,
      focusBoard: before.boardId,
      focusAfter: after.activeId,
      cardCount: after.cardCount,
      scrollHeld,
      focusHeld,
      pass: scrollHeld && focusHeld && after.cardCount === before.cardCount,
    };
    if (!result.grid.pass) {
      fail(
        "grid refresh did not preserve scroll+focus: " + JSON.stringify(result.grid)
      );
    }

    result.ok = true;
    console.log(JSON.stringify(result));
  } catch (e) {
    fail(String(e && e.message ? e.message : e));
  } finally {
    await browser.close();
  }
})();
