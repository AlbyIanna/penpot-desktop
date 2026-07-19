/**
 * Deterministic web-surface capture for milestone docs (PLAN4 "Definition of
 * done"). Drives the BUNDLED offline chromium — same module resolution as
 * scripts/routes_gate_nav.cjs. Read-only: navigates and screenshots, never
 * mutates the page. Fixed viewport so re-runs are comparable, which is what
 * makes honest before/after possible.
 *
 * argv: <base> <outDir> <name=path> [<name=path> ...]
 */
const path = require("path");
const REPO = process.env.REPO_ROOT || process.cwd();
const PW = process.env.PLAYWRIGHT_MODULE ||
  path.join(REPO, "runtime/exporter/node_modules/playwright");
const { chromium } = require(PW);

const VIEWPORT = { width: 1280, height: 800 };
// Both docs/milestones/d1/README.md and baseline.md tell readers to
// re-capture with `SETTLE_MS=15000` — but this used to read ONLY
// `SHOTS_SETTLE_MS`, so the documented command silently re-captured at the
// 3.5s default baseline.md itself documents as producing WRONG screenshots
// (the dashboard renders as Penpot's login-card placeholder at 3.5s). Accept
// SETTLE_MS (checked first, so it wins when both are set) and keep
// SHOTS_SETTLE_MS working for anything already relying on it (Task 6 finding 4).
const SETTLE_MS = Number(process.env.SETTLE_MS || process.env.SHOTS_SETTLE_MS || 3500);

(async () => {
  const [base, outDir, ...pairs] = process.argv.slice(2);
  if (!base || !outDir || pairs.length === 0) {
    console.error("usage: shots_capture.cjs <base> <outDir> <name=path>...");
    process.exit(2);
  }
  const browser = await chromium.launch({ headless: true });
  let failed = 0;
  try {
    const page = await browser.newPage({ viewport: VIEWPORT });
    // Auto-login once so authenticated surfaces render.
    await page.goto(`${base}/__bootstrap`, { waitUntil: "domcontentloaded" });
    await page.waitForTimeout(SETTLE_MS);

    for (const pair of pairs) {
      const idx = pair.indexOf("=");
      const name = pair.slice(0, idx);
      const rel = pair.slice(idx + 1);
      const file = path.join(outDir, `${name}.png`);
      try {
        await page.goto(`${base}${rel}`, { waitUntil: "domcontentloaded" });
        await page.waitForTimeout(SETTLE_MS);
        await page.screenshot({ path: file });
        console.log(`captured ${name} <- ${rel}`);
      } catch (e) {
        console.error(`FAILED ${name} <- ${rel}: ${e}`);
        failed++;
      }
    }
  } finally {
    await browser.close();
  }
  process.exit(failed === 0 ? 0 : 1);
})();
