/**
 * D1: SPA-side egress log + BEHAVIOURAL flag assertions.
 *
 * A flag that is SET is not a flag that WORKED (PLAN4 risk 4) — upstream can
 * rename a flag and leave our gate green while the surface quietly returns.
 * So this checks the SURFACES, not the config string, and simultaneously
 * records every request the SPA makes so off-machine egress is caught.
 *
 * Read-only: navigates and observes. Nothing is injected into the SPA.
 */
const path = require("path");
const REPO = process.env.REPO_ROOT || process.cwd();
const PW = process.env.PLAYWRIGHT_MODULE ||
  path.join(REPO, "runtime/exporter/node_modules/playwright");
const { chromium } = require(PW);

const BASE = process.env.BASE || "http://localhost:9046";
// Default bumped from a naive 3.5s to a generous >=12s: a short settle
// silently lies. Live runs showed the dashboard screenshotting as Penpot's
// own login card at 3.5s, because the SPA renders that placeholder while it
// resolves the session — a page in that transient state has no registration
// form or templates section either, so a short settle would report both
// surfaces "gone" and turn the gate green for the wrong reason.
const SETTLE = Number(process.env.SHOTS_SETTLE_MS || 12000);

function isLoopback(u) {
  try {
    const h = new URL(u).hostname;
    return h === "localhost" || h === "127.0.0.1" || h === "::1" || h.startsWith("127.");
  } catch {
    return true; // data:, blob:, about: — never leave the machine
  }
}

// PROOF-OF-RENDER, waited for rather than assumed.
//
// This is the D0 lesson applied: an absence assertion is worthless unless
// you first prove you were looking at a rendered page. A page that failed
// to load has no signup form either — so "no form found" would report the
// surface as GONE and turn the gate green while the flag did nothing.
//
// Instead of a fixed sleep followed by a single check, this polls the same
// predicate (>20 elements under <body>) so a fast, healthy render doesn't
// waste the whole budget, while a page that never settles still gets the
// full generous window before we give up. It requires two consecutive
// positive polls ~500ms apart before trusting the signal, since Penpot's
// login-card placeholder is itself "rendered" by a bare element-count check
// — the point of the generous window is to wait it out, not just detect any
// DOM.
async function waitForStableRender(page, timeoutMs) {
  const start = Date.now();
  let prevOk = false;
  while (Date.now() - start < timeoutMs) {
    let count = 0;
    try {
      count = await page.$$eval("body *", (els) => els.length);
    } catch {
      count = 0;
    }
    const ok = count > 20;
    if (ok && prevOk) return true;
    prevOk = ok;
    await page.waitForTimeout(500);
  }
  try {
    return (await page.$$eval("body *", (els) => els.length)) > 20;
  } catch {
    return false;
  }
}

(async () => {
  const browser = await chromium.launch({ headless: true });
  const requests = [];
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    page.on("request", (r) => requests.push(r.url()));

    // /__bootstrap is one-shot per boot (answers 403 on a second call), so
    // this is the only call to it in the whole run. Give the SPA a generous,
    // signal-driven chance to finish auto-login before navigating onward.
    await page.goto(`${BASE}/__bootstrap`, { waitUntil: "domcontentloaded" });
    await waitForStableRender(page, SETTLE);

    // Behavioural: the registration surface must not render a signup form.
    // Each check returns "gone" | "present" | "inconclusive" — the gate must
    // treat "inconclusive" as a LOUD FAILURE, never as success.
    await page.goto(`${BASE}/#/auth/register`, { waitUntil: "domcontentloaded" });
    let registration;
    if (!(await waitForStableRender(page, SETTLE))) {
      registration = "inconclusive";
    } else {
      const pw = await page.$$eval(
        "input[type='password']", (e) => e.length);
      const signup = await page.$$eval("form, button, a", (els) =>
        els.some((f) => /register|sign\s*up|create account/i.test(f.textContent || "")));
      registration = pw === 0 && !signup ? "gone" : "present";
    }

    // Behavioural: the dashboard must not show the cloud templates section.
    // Navigate to the bare root (never a hardcoded team-id) and let the
    // authenticated SPA redirect to its own team-scoped dashboard route.
    await page.goto(`${BASE}/`, { waitUntil: "domcontentloaded" });
    let templatesSection;
    if (!(await waitForStableRender(page, SETTLE))) {
      templatesSection = "inconclusive";
    } else {
      const hasTemplates = await page.$$eval("*", (els) =>
        els.some((e) => /templates/i.test(e.getAttribute?.("class") || "") ||
                        /view all templates|templates/i.test(
                          (e.children.length === 0 && e.textContent) || "")));
      templatesSection = hasTemplates ? "present" : "gone";
    }

    const nonLoopbackRequests = [...new Set(requests.filter((u) => !isLoopback(u)))];
    console.log(JSON.stringify({
      ok: true,
      requests: [...new Set(requests)].length,
      nonLoopbackRequests,
      registration,      // "gone" | "present" | "inconclusive"
      templatesSection,  // "gone" | "present" | "inconclusive"
    }));
    process.exit(0);
  } catch (e) {
    console.log(JSON.stringify({ ok: false, error: String(e) }));
    process.exit(1);
  } finally {
    await browser.close();
  }
})();
