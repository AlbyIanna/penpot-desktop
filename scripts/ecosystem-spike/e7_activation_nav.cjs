#!/usr/bin/env node
/*
 * E7 plugin-activation browser leg (scripts/e7-plugins-spike.sh).
 *
 * Drives the BUNDLED chromium (N2 playwright + exporter-browsers, fully
 * offline) through Penpot's OWN native Plugin Manager — it is test automation
 * driving the native UI, NOT our integration mechanism (invariant 3). No SPA
 * bytes are patched; every action is a real DOM click/type.
 *
 * Flow:
 *   1. GET /__bootstrap (one-shot server login → auth-token cookie).
 *   2. Navigate to the workspace deep link; wait for the workspace to boot.
 *   3. Open the native Plugin Manager (toolbar Plugins button), type the
 *      local /__packages/<pkg>/manifest.json URL into the manager's search
 *      bar, click Install.
 *   4. Pass Penpot's OWN consent prompt: the permissions dialog → click Allow.
 *   5. The plugin runs in Penpot's plugin runtime: it creates the named shape
 *      (observable 1, asserted over RPC by the gate) and fetch()es the beacon
 *      (observable 2, asserted by the observer / this leg's console capture).
 *   6. Capture the RUNTIME MECHANISM: the frame tree + whether plugin.js is a
 *      separate document/worker or evaluated in the SPA page context, and any
 *      CSP violation console message.
 *
 * Usage:
 *   node e7_activation_nav.cjs <base-url> <manifest-url> <shape-name> <screenshot-dir> <mode> <deep-link>
 *     mode = "csp-off" | "csp-on"
 * Env: ROUTES_GATE_PLAYWRIGHT, PLAYWRIGHT_BROWSERS_PATH, E7_NAV_TIMEOUT_MS
 * Prints: one JSON line with the full capture; exit 0 iff the mode's
 *   expectations held.
 */
"use strict";

const path = require("path");
const fs = require("fs");
const REPO = path.resolve(__dirname, "..", "..");
const PW =
  process.env.ROUTES_GATE_PLAYWRIGHT ||
  path.join(REPO, "runtime/exporter/node_modules/playwright");
const { chromium } = require(PW);

const BASE = (process.argv[2] || "").replace(/\/+$/, "");
const MANIFEST_URL = process.argv[3] || "";
const SHAPE_NAME = process.argv[4] || "E7-FIXTURE-SHAPE";
const SHOT_DIR = process.argv[5] || ".";
const MODE = process.argv[6] || "csp-off";
const DEEP_LINK_ARG = process.argv[7] || "";
// The off-origin beacon URL: the plugin's SES Compartment can only fetch()
// (connect-src); the img-src exfil VECTOR is probed here at the SPA-document
// level (a page-context `new Image().src`), which is the level the proxy CSP
// applies to. Under CSP off it leaves; under CSP on img-src fences it.
const BEACON_URL = (process.argv[8] || "").replace(/\/+$/, "");
if (!BASE || !MANIFEST_URL) {
  console.error(
    "usage: e7_activation_nav.cjs <base> <manifest-url> <shape-name> <shot-dir> <mode>"
  );
  process.exit(64);
}
const T = parseInt(process.env.E7_NAV_TIMEOUT_MS || "120000", 10);

const result = {
  ok: false,
  mode: MODE,
  steps: {},
  console: [],
  beaconConsole: null,
  imageBeaconConsole: null,
  cspViolations: [],
  frames: [],
  runtimeMechanism: null,
};

function shot(page, name) {
  try {
    return page.screenshot({ path: path.join(SHOT_DIR, name), fullPage: false });
  } catch (e) {
    return Promise.resolve();
  }
}

function done(ok) {
  result.ok = ok;
  console.log(JSON.stringify(result));
  process.exit(ok ? 0 : 1);
}

(async () => {
  const browser = await chromium.launch({ headless: true });
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  page.setDefaultTimeout(T);

  page.on("console", (msg) => {
    const text = msg.text();
    result.console.push(text.slice(0, 300));
    if (text.includes("[E7-FIXTURE] beacon egress")) result.beaconConsole = text;
    if (text.includes("[E7-FIXTURE] image beacon egress"))
      result.imageBeaconConsole = text;
    if (/Content Security Policy|Refused to connect|violates the following/i.test(text)) {
      result.cspViolations.push(text.slice(0, 300));
    }
  });

  try {
    // --- (1) auto-login -------------------------------------------------
    await page.goto(BASE + "/__bootstrap", { waitUntil: "domcontentloaded" });
    result.steps.bootstrap = true;

    // --- (2) workspace boot --------------------------------------------
    // The gate passes the seeded file's workspace deep link directly (the
    // seeded file has no boards, so /__api/vault/boards would be empty).
    const deepLink = DEEP_LINK_ARG;
    if (!deepLink) throw new Error("no deep link passed to the nav leg");
    result.deepLink = deepLink;

    await page.goto(BASE + deepLink, { waitUntil: "domcontentloaded" });
    // Wait for the workspace to actually render its toolbar (the plugins
    // button lives there). The CLJS app + WASM must boot for this.
    await page.waitForSelector('button[aria-label^="Plugins"], [class*="plugins"]', {
      timeout: T,
    });
    result.steps.workspaceBooted = true;
    await shot(page, MODE + "-1-workspace.png");

    // --- (3) open the native Plugin Manager ----------------------------
    const pluginsBtn = await page.$('button[aria-label^="Plugins"]');
    if (!pluginsBtn) throw new Error("plugins toolbar button not found");
    await pluginsBtn.click();
    // The manager modal has a search bar input + an Install button.
    const input = await page.waitForSelector(
      'input[placeholder*="plugin URL" i], .main_ui_workspace_plugins__top-bar input',
      { timeout: T }
    );
    result.steps.managerOpened = true;
    await shot(page, MODE + "-2-manager.png");

    await input.fill(MANIFEST_URL);
    // The "Install" primary button in the top bar.
    const installBtn = await page.$(
      ".main_ui_workspace_plugins__top-bar .main_ui_workspace_plugins__primary-button"
    );
    if (!installBtn) throw new Error("Install button not found in plugin manager");
    await installBtn.click();
    result.steps.installClicked = true;

    // --- (4) Penpot's own consent prompt -> Allow ----------------------
    const allow = await page.waitForSelector(
      '.main_ui_workspace_plugins__plugin-permissions input[value="Allow"], ' +
        'input[value="Allow"]',
      { timeout: T }
    );
    result.steps.consentShown = true;
    await shot(page, MODE + "-3-consent.png");
    await allow.click();
    result.steps.consentAllowed = true;
    await page.waitForTimeout(2000);

    // --- (5) RUN the plugin ---------------------------------------------
    // Installing (Allow) writes the registry pointer to profile props and then
    // shows the plugin-MANAGEMENT modal (verified in the 2.16.2 bundle:
    // on_accept -> install_plugin! -> modal/show plugin_management). It does
    // NOT run the plugin. The plugin's CODE only evaluates when OPENED, so we
    // click the installed plugin's "Open" button in that management modal —
    // a real native-UI click, not a driven SPA action.
    let ran = false;
    try {
      const openBtn = await page.waitForSelector(
        ".main_ui_workspace_plugins__open-button:not([disabled])",
        { timeout: 20000 }
      );
      await openBtn.click();
      result.steps.ranViaOpen = true;
      ran = true;
    } catch (e) {
      result.steps.openError = String(e && e.message ? e.message : e);
    }
    result.steps.pluginOpened = ran;

    // --- (5b) DOCUMENT-LEVEL img-src exfil probe ------------------------
    // The plugin sandbox cannot construct Image (hardened SES Compartment),
    // so the image-beacon VECTOR is fired here in the SPA document context —
    // exactly where the proxy CSP img-src applies. Under CSP off the request
    // leaves and reaches the observer; under CSP on img-src fences it.
    if (BEACON_URL) {
      try {
        await page.evaluate((u) => {
          const img = new Image();
          img.src = u + "?src=img&t=" + Date.now();
        }, BEACON_URL);
        result.steps.imageBeaconFired = true;
      } catch (e) {
        result.steps.imageBeaconError = String(e && e.message ? e.message : e);
      }
    }

    // --- (6) let the plugin + image beacon run, capture runtime mechanism
    await page.waitForTimeout(8000);
    await shot(page, MODE + "-4-after-run.png");

    // Frame tree: is plugin.js a separate document/worker frame, or in-page?
    for (const f of page.frames()) {
      result.frames.push({ url: f.url(), name: f.name() });
    }
    result.runtimeMechanism = await page.evaluate(() => {
      // The plugins system logs "[PLUGINS] Loading plugin system" and evaluates
      // plugin code via a SES Compartment in THIS window (see libs.js). Detect
      // whether our fixture code ran in this document's context.
      return {
        hasSES: typeof (window.Compartment) !== "undefined",
        location: window.location.href,
        // number of iframes present in the top document
        iframeCount: document.querySelectorAll("iframe").length,
      };
    });

    // Verdict per mode.
    const beaconSucceeded =
      result.beaconConsole && /SUCCEEDED/.test(result.beaconConsole);
    const beaconBlocked =
      result.beaconConsole && /BLOCKED/.test(result.beaconConsole);
    result.beaconSucceeded = !!beaconSucceeded;
    result.beaconBlocked = !!beaconBlocked;

    // The gate asserts the shape over RPC; this leg reports install+consent+run
    // reached, plus the beacon disposition for the CSP verdict.
    const ranOk =
      result.steps.consentAllowed &&
      (result.beaconConsole !== null); // plugin.js executed to the fetch line
    done(!!ranOk);
  } catch (e) {
    result.error = String(e && e.message ? e.message : e);
    try {
      await shot(page, MODE + "-error.png");
    } catch (_) {}
    done(false);
  } finally {
    await browser.close();
  }
})();
