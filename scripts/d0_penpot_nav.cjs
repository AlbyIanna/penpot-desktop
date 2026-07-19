/**
 * D0: read-only inspection of how Penpot's SPA links to its web routes.
 *
 * Drives the BUNDLED offline chromium (same pattern as routes_gate_nav.cjs).
 * We only READ the DOM — nothing is injected into our own webview and the SPA
 * is not modified, so invariant 3 is untouched. Chromium is not WKWebView, so
 * this answers "how does Penpot navigate?", never "does our handler fire?".
 */
const PW = process.env.PLAYWRIGHT_MODULE ||
  `${process.env.REPO_ROOT}/runtime/exporter/node_modules/playwright`;
const { chromium } = require(PW);

const BASE = process.env.BASE || "http://localhost:9034";

(async () => {
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    // Auto-login, then land wherever the app normally lands.
    await page.goto(`${BASE}/__bootstrap`, { waitUntil: "domcontentloaded" });
    await page.waitForTimeout(4000);

    // Collect every anchor whose href targets a hash route.
    const anchors = await page.$$eval("a[href]", (els) =>
      els
        .map((e) => ({ href: e.getAttribute("href") || "" }))
        .filter((a) => a.href.includes("#/"))
    );
    const usesAnchorHref = anchors.some(
      (a) => a.href.includes("#/dashboard") || a.href.includes("#/settings")
    );
    console.log(JSON.stringify({ ok: true, anchors, usesAnchorHref }));
  } catch (e) {
    console.log(JSON.stringify({ ok: false, error: String(e) }));
  } finally {
    await browser.close();
  }
})();
