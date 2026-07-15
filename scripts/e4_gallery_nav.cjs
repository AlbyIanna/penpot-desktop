#!/usr/bin/env node
/*
 * E4 package-gallery live navigation leg (PLAN3.md milestone E4).
 *
 * Drives the BUNDLED chromium (N2's playwright + exporter-browsers, fully
 * offline) against a running stack to prove the package gallery deep-links a
 * real package file to its EXACT /#/workspace URL — the same coupling the N3
 * routes-gate proves for the board grid, now for the E4 gallery card:
 *
 *   1. Load /__packages, read the target package's deepLink from the
 *      /__api/packages/search PAYLOAD (the authoritative string), find that
 *      package's card, assert the card's data-deeplink === the payload string,
 *      click it, and assert the landed URL is exactly BASE + deepLink.
 *
 * We assert on navigation COMMIT (not full SPA load) so the heavy ClojureScript
 * app never has to finish booting — the URL is what the gate is about, exactly
 * as scripts/routes_gate_nav.cjs does for /__home.
 *
 * Usage:  node e4_gallery_nav.cjs <base-url> <target-package-id>
 * Env:    ROUTES_GATE_PLAYWRIGHT  path to the playwright module
 *         PLAYWRIGHT_BROWSERS_PATH  the bundled exporter-browsers dir
 * Prints: one JSON line {ok, package}; exit 0 on pass.
 */
"use strict";

const path = require("path");
const REPO = path.resolve(__dirname, "..");
const PW =
  process.env.ROUTES_GATE_PLAYWRIGHT ||
  path.join(REPO, "runtime/exporter/node_modules/playwright");
const { chromium } = require(PW);

const BASE = (process.argv[2] || "").replace(/\/+$/, "");
const TARGET = process.argv[3] || "";
if (!BASE || !TARGET) {
  console.error("usage: e4_gallery_nav.cjs <base-url> <target-package-id>");
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

    await page.goto(BASE + "/__packages", { waitUntil: "domcontentloaded" });
    // Wait for the flat gallery to render at least one deep-linked card.
    await page.waitForSelector("a.package-card[data-deeplink]", { timeout: NAV_TIMEOUT });

    // The authoritative deep link comes from the API payload, not the DOM.
    const payload = await page.evaluate(async (target) => {
      const r = await fetch("/__api/packages/search?q=&limit=1000");
      if (!r.ok) return null;
      const j = await r.json();
      const hit = (j.packages || []).find((p) => p.id === target);
      return hit ? { deepLink: hit.deepLink, id: hit.id } : null;
    }, TARGET);
    if (!payload || !payload.deepLink) {
      fail("target package " + TARGET + " not found in /__api/packages/search payload");
    }
    if (!/^\/#\/workspace\?team-id=[^&]+&file-id=[^&]+/.test(payload.deepLink)) {
      fail("payload deepLink is not a well-formed workspace deep link: " + payload.deepLink);
    }

    // Find that package's card and assert the DOM string equals the payload.
    const card = await page.$(
      'a.package-card[data-deeplink="' + payload.deepLink.replace(/"/g, '\\"') + '"]'
    );
    if (!card) {
      fail("no gallery card carries the payload deepLink " + payload.deepLink);
    }
    const domDeepLink = await card.getAttribute("data-deeplink");
    if (domDeepLink !== payload.deepLink) {
      fail("card data-deeplink " + domDeepLink + " != payload " + payload.deepLink);
    }

    const expected = BASE + payload.deepLink;
    await Promise.all([
      page.waitForURL((u) => u.toString().includes("#/workspace"), {
        waitUntil: "commit",
        timeout: NAV_TIMEOUT,
      }),
      card.click(),
    ]);
    const landed = page.url();
    result.package = { id: payload.id, expected, landed, pass: landed === expected };
    if (!result.package.pass) {
      fail("card click landed on " + landed + " but the deep link was " + expected);
    }

    result.ok = true;
    console.log(JSON.stringify(result));
  } catch (e) {
    fail(String(e && e.message ? e.message : e));
  } finally {
    await browser.close();
  }
})();
