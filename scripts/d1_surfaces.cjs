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

const BASE = process.env.BASE || "http://localhost:9046";
// Default bumped from a naive 3.5s to a generous >=12s: a short settle
// silently lies. Live runs showed the dashboard screenshotting as Penpot's
// own login card at 3.5s, because the SPA renders that placeholder while it
// resolves the session — a page in that transient state has no registration
// form or templates section either, so a short settle would report both
// surfaces "gone" and turn the gate green for the wrong reason.
const SETTLE = Number(process.env.SHOTS_SETTLE_MS || 12000);

// Non-network URL schemes (in-memory data, never touch a socket). These are
// deliberately excluded from egress reporting BY NAME — not left to fall
// through the loopback predicate and happen to land on the safe side.
const NON_NETWORK_SCHEMES = new Set(["data:", "blob:", "about:"]);

function isNonNetworkUrl(u) {
  for (const scheme of NON_NETWORK_SCHEMES) {
    if (u.startsWith(scheme)) return true;
  }
  return false;
}

// A dotted-quad IPv4 literal in 127.0.0.0/8, e.g. "127.0.0.1" or "127.1.2.3".
// Each octet must be a real 0-255 value with no extra characters — this must
// NOT match a hostname that merely starts with "127." (e.g.
// "127.0.0.1.evil.com" has a trailing label; "1270.0.0.1" has no dots after
// the "127").
const IPV4_127_RE =
  /^127\.(25[0-5]|2[0-4]\d|1?\d{1,2})\.(25[0-5]|2[0-4]\d|1?\d{1,2})\.(25[0-5]|2[0-4]\d|1?\d{1,2})$/;

function isLoopback(u) {
  // Non-network schemes are handled explicitly by the caller — this
  // predicate only ever answers the network-loopback question.
  let h;
  try {
    h = new URL(u).hostname;
  } catch {
    // "Could not parse" must never mean "safe". A malformed URL (e.g.
    // "http://1270.0.0.1/steal") is surfaced as non-loopback so it shows up
    // in the evidence rather than silently vanishing.
    return false;
  }
  if (h === "localhost") return true;
  // IPv6 loopback: Node's URL().hostname keeps the brackets for IPv6
  // literals (e.g. "[::1]"), so match both the bracketed and bare forms,
  // plus the fully expanded form.
  if (h === "::1" || h === "[::1]" || h === "0:0:0:0:0:0:0:1" ||
      h === "[0:0:0:0:0:0:0:1]") return true;
  return IPV4_127_RE.test(h);
}

// PROOF-OF-RENDER, waited for rather than assumed.
//
// This is the D0 lesson applied: an absence assertion is worthless unless
// you first prove you were looking at a rendered page. A page that failed
// to load has no signup form either — so "no form found" would report the
// surface as GONE and turn the gate green while the flag did nothing.
//
// A bare ">20 elements under <body>" count is NOT proof of render: Penpot's
// login-card placeholder — shown while the SPA resolves the session — is
// itself a large SVG with far more than 20 nodes, so a generic element-count
// check is satisfied by the very placeholder this proof exists to wait out.
// That let templatesSection get measured against the placeholder, find no
// "templates" text on it, and report "gone" — a vacuous PASS of the one leg
// that must fail when the flag stops working (Task 6 finding 1).
//
// So this polls for a ROUTE-IDENTIFYING marker instead: a CSS class emitted
// only by the top-level component of the specific route under test (the
// auth pages' shared "auth-content" wrapper; the dashboard's top-level
// "dashboard" wrapper, which contains the sidebar/projects grid). Penpot's
// placeholder never carries these classes, so satisfying this check is proof
// that route — not just "some DOM" — actually rendered. If the marker never
// appears within the budget, this returns false and the caller reports
// "inconclusive": a generic "the DOM has nodes" signal must never be
// sufficient to conclude a surface is absent.
async function waitForMarker(page, selector, timeoutMs) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      if (await page.$(selector)) return true;
    } catch {
      // Mid-navigation query errors are expected while the route swaps out
      // — keep polling rather than treating this as "marker absent".
    }
    await page.waitForTimeout(500);
  }
  try {
    return (await page.$(selector)) !== null;
  } catch {
    return false;
  }
}

// Route-identifying markers, one per surface under test. Verified (Task 6
// finding 1) against the actual bundled build: main-auth.js emits
// `className:"main_ui_auth__auth-content"` on the shared wrapper rendered by
// every `#/auth/*` sub-route (login, register, recovery); main-dashboard.js
// emits `className:"main_ui_dashboard__dashboard"` on the top-level wrapper
// that contains the sidebar and the projects grid. Both are literal
// CSS-module class names with no build-time hash suffix, and neither is
// present on the transient login-card placeholder.
const AUTH_PAGE_MARKER = ".main_ui_auth__auth-content";
const DASHBOARD_MARKER = ".main_ui_dashboard__dashboard";

// A fixed sleep followed by a single generic check "silently lies" (see
// docs/milestones/d1/baseline.md) — this polls the same generic predicate
// (>20 elements under <body>) so a fast render doesn't waste the whole
// budget. It is ONLY used before the one-shot `/__bootstrap` navigation
// below, where no gone/present/inconclusive verdict is derived from the
// result — it exists solely to give auto-login a generous, signal-driven
// head start before navigating onward. Every verdict-bearing check below
// uses waitForMarker(), never this.
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

// Self-check for isLoopback that needs neither a browser nor a live stack.
// Run as: node scripts/d1_surfaces.cjs selftest
function _selftest() {
  const cases = [
    // [url, expectedLoopback]
    ["http://127.0.0.1/", true],
    ["http://127.1.2.3/", true],
    ["http://localhost/", true],
    ["http://[::1]/", true],
    ["http://127.0.0.1.evil.com/steal", false],
    ["http://127.evil.com/", false],
    ["http://1270.0.0.1/", false],
    ["http://12.7.0.0.1/", false],
    ["http://example.com/", false],
    ["http://0.0.0.0/", false],
    ["http://169.254.169.254/", false],
    ["not a url at all", false],
  ];
  let failed = 0;
  for (const [url, expected] of cases) {
    const got = isLoopback(url);
    if (got !== expected) {
      failed++;
      console.error(`FAIL isLoopback(${JSON.stringify(url)}) = ${got}, expected ${expected}`);
    }
  }
  // data:/blob:/about: are excluded by name, not by isLoopback().
  const nonNetworkCases = ["data:text/plain,hi", "blob:http://x/y", "about:blank"];
  for (const url of nonNetworkCases) {
    if (!isNonNetworkUrl(url)) {
      failed++;
      console.error(`FAIL isNonNetworkUrl(${JSON.stringify(url)}) = false, expected true`);
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

async function main() {
  const PW = process.env.PLAYWRIGHT_MODULE ||
    path.join(REPO, "runtime/exporter/node_modules/playwright");
  const { chromium } = require(PW);
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
    if (!(await waitForMarker(page, AUTH_PAGE_MARKER, SETTLE))) {
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
    if (!(await waitForMarker(page, DASHBOARD_MARKER, SETTLE))) {
      templatesSection = "inconclusive";
    } else {
      const hasTemplates = await page.$$eval("*", (els) =>
        els.some((e) => /templates/i.test(e.getAttribute?.("class") || "") ||
                        /view all templates|templates/i.test(
                          (e.children.length === 0 && e.textContent) || "")));
      templatesSection = hasTemplates ? "present" : "gone";
    }

    // Behavioural: the login-with-password surface must not render a
    // password field or a submit control. D1 task 4 live-audited this once
    // by hand (`.superpowers/sdd/d1-login-audit.md`); this is that audit's
    // automated, repeatable re-check (Task 6 finding 3) using the SAME
    // proof-of-render + tri-state idiom as registration/templatesSection
    // above, not a second mechanism.
    await page.goto(`${BASE}/#/auth/login`, { waitUntil: "domcontentloaded" });
    let loginForm;
    if (!(await waitForMarker(page, AUTH_PAGE_MARKER, SETTLE))) {
      loginForm = "inconclusive";
    } else {
      const pw = await page.$$eval("input[type='password']", (e) => e.length);
      const submitControls = await page.$$eval(
        "button[type='submit'], input[type='submit'], form button, form input[type='button']",
        (e) => e.length);
      loginForm = pw === 0 && submitControls === 0 ? "gone" : "present";
    }

    const nonLoopbackRequests = [...new Set(
      requests.filter((u) => !isNonNetworkUrl(u) && !isLoopback(u)))];
    console.log(JSON.stringify({
      ok: true,
      requests: [...new Set(requests)].length,
      nonLoopbackRequests,
      registration,      // "gone" | "present" | "inconclusive"
      templatesSection,  // "gone" | "present" | "inconclusive"
      loginForm,         // "gone" | "present" | "inconclusive"
    }));
    process.exit(0);
  } catch (e) {
    console.log(JSON.stringify({ ok: false, error: String(e) }));
    process.exit(1);
  } finally {
    await browser.close();
  }
}
