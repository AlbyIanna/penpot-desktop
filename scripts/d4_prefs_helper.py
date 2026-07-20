#!/usr/bin/env python3
"""HTTP helper for scripts/d4-preferences.sh (the D4 Preferences gate).

Talks to the D4 Preferences routes (`apps/desktop/src/prefs_http.rs`) THROUGH
THE PROXY — same-origin, no auth cookie/token needed (these routes are mounted
unauthenticated, like `/__home` etc.). No RPC client here: the exports
positive/negative legs of assertion (c) are driven by REUSING
`scripts/m5_features_helper.py`'s own `seed`/`check`/`exports_check`/
`edit_alpha` subcommands (set `M5_DESIGNS_DIR` and call that script directly)
rather than writing a third hasher — CLAUDE.md's normalization spec already
has one implementation (`roundtrip.py`) and `m5_features_helper.py` is the
second; this file must not become a third. Assertion (e)'s zero-cross-vault-
spill check is the same reuse story, against `scripts/n5_vaults_helper.py`.

Env:
  PENPOT_BACKEND   proxy base url (http://localhost:<proxy-port>)

Subcommands:
  prefs_file <data_dir>
      Print the raw JSON contents of <data_dir>/preferences.json, or the
      literal string "MISSING" if the file does not exist (used for
      assertion (a)'s FILE leg).
  get
      GET /__api/prefs. Prints the JSON response (compact, one line).
  post <json-body>
      POST /__api/prefs with <json-body> (a JSON object string — the FULL
      Preferences shape; the page always resends everything it last read,
      never a partial patch, same contract prefs.rs itself keeps). Prints
      the JSON response.
  reboot [timeout-s]
      POST /__api/prefs/reboot. The route itself ACKs immediately with a 202
      (`{"ok":true,"accepted":true}`) and runs `reboot_in_place` in a
      detached task that outlives the request — it tears down the very
      proxy serving the request, so a synchronous response can never arrive
      (see apps/desktop/src/prefs_http.rs's module doc). This subcommand
      hides that: it records the `lastOp` in effect before the POST, sends
      it, then polls GET /__api/prefs (tolerating connection failures —
      expected while the stack is down) until `lastOp` changes, and prints a
      JSON object shaped like the old synchronous response
      (`{"ok": <real outcome>, "accepted": true, "lastOp": {...}, "vault":
      {...}}`) so callers keep grepping `"ok": true`/`"ok": false` for the
      REAL result, not just the 202 ack.
  vault <path> [timeout-s]
      POST /__api/prefs/vault {"path": path}. Same ack-then-poll shape as
      `reboot` above, delegating to `switch_to` in the detached task.
  config_js_has <token>
      GET /js/config.js, parse the `penpotFlags` string, and check whether
      <token> is present as a WHOLE token (space-split membership — the same
      method `lib.rs`'s own `plugins_enabled` unit tests use; a bare
      substring match would false-positive on a flag that merely CONTAINS
      <token>). Prints "yes" or "no".
  csp_present
      GET /index.html and check for a `Content-Security-Policy` response
      header (case-insensitive, per RFC — `http.client`'s header object
      already does this). Prints "yes|<value>" or "no|".
  wait_bool <field-path> <true|false> <timeout-s>
      Poll GET /__api/prefs until the dotted <field-path> (e.g. "syncPaused",
      "rendersRunning", "preferences.syncEnabled") equals the given bool.
      Prints "OK" on success; exits non-zero with the last-seen value on
      timeout — a live-effect assertion must be able to time out and FAIL,
      never spin forever and get killed by the shell's own timeout as a
      false pass.
"""

import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

BASE = os.environ.get("PENPOT_BACKEND", "")


def die(msg, code=2):
    print(f"HELPER-FAIL: {msg}", file=sys.stderr)
    sys.exit(code)


def _get(path, timeout=30):
    req = urllib.request.Request(BASE + path, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return json.loads(resp.read())
    except urllib.error.HTTPError as e:
        die(f"GET {path} -> HTTP {e.code}: {e.read().decode(errors='replace')}")


def _get_text(path, timeout=30):
    req = urllib.request.Request(BASE + path, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.read().decode("utf-8", errors="replace"), resp.headers
    except urllib.error.HTTPError as e:
        die(f"GET {path} -> HTTP {e.code}: {e.read().decode(errors='replace')}")


def _post(path, body, timeout=60):
    data = json.dumps(body).encode() if body is not None else b""
    req = urllib.request.Request(
        BASE + path, data=data, headers={"Content-Type": "application/json"}, method="POST"
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return json.loads(resp.read())
    except urllib.error.HTTPError as e:
        die(f"POST {path} -> HTTP {e.code}: {e.read().decode(errors='replace')}")


def _dotted_get(d, path):
    cur = d
    for part in path.split("."):
        if not isinstance(cur, dict) or part not in cur:
            return None
        cur = cur[part]
    return cur


def _get_lenient(path, per_call_timeout=5):
    """Like `_get` but returns `None` (never dies) on ANY failure — while a
    detached switch/reboot is tearing the stack down or the new one is still
    booting, connection-refused is EXPECTED here, not an error worth a fatal
    HELPER-FAIL."""
    req = urllib.request.Request(BASE + path, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=per_call_timeout) as resp:
            return json.loads(resp.read())
    except Exception:
        return None


def _last_op_key(data):
    """A value that changes iff a NEW detached switch/reboot has finished and
    been recorded — `(op, at)`, or `None` before anything has ever finished.
    See `_wait_for_op_outcome` for why this, not just "a GET succeeded", is
    the readiness signal."""
    op = (data or {}).get("lastOp")
    return (op.get("op"), op.get("at")) if op else None


def _wait_for_op_outcome(baseline_key, timeout):
    """Poll GET /__api/prefs until `lastOp` differs from `baseline_key` (the
    op in effect right before the switch/reboot was kicked off) — NOT just
    until any GET succeeds. A GET can succeed against the OLD stack too:
    there is a brief window before the detached task has torn anything down
    yet, and some failures (e.g. an unusable target path) are caught before
    the stack is touched at all, so the origin never actually goes away
    either. `lastOp` changing is the one signal that holds in every case,
    because only the detached task itself writes it, and only once it is
    completely done — success or failure alike."""
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        data = _get_lenient("/__api/prefs")
        if data is not None:
            last = data
            if _last_op_key(data) != baseline_key:
                return data
        time.sleep(1)
    die(
        f"timed out after {timeout}s waiting for the detached switch/reboot to "
        f"finish (last successful GET /__api/prefs: "
        f"{json.dumps(last) if last is not None else 'none succeeded'})",
        1,
    )


def _post_then_wait(path, body, timeout):
    """`POST <path>` against an ack-then-detach route (`/reboot`, `/vault`):
    captures the `lastOp` baseline, sends the request (which should ACK with
    a 202 almost immediately), then waits for the detached task's outcome
    via `_wait_for_op_outcome` and reprints it in the OLD synchronous shape —
    `ok` reflects the REAL result (`lastOp.ok`), not just whether the 202 was
    accepted, so every existing `grep '"ok": true'`/`'"ok": false'` caller
    keeps working unchanged."""
    baseline_key = _last_op_key(_get_lenient("/__api/prefs"))
    ack = _post(path, body, timeout=15)
    if not ack.get("accepted"):
        # Rejected synchronously (e.g. empty path, runner not ready yet) —
        # nothing was kicked off, nothing to poll for.
        return ack
    final = _wait_for_op_outcome(baseline_key, timeout)
    last_op = final.get("lastOp") or {}
    return {
        "ok": bool(last_op.get("ok")),
        "accepted": True,
        "lastOp": last_op,
        "vault": final.get("vault"),
    }


def cmd_prefs_file(data_dir):
    path = os.path.join(data_dir, "preferences.json")
    if not os.path.isfile(path):
        print("MISSING")
        return
    with open(path) as fh:
        raw = fh.read()
    try:
        print(json.dumps(json.loads(raw), sort_keys=True))
    except json.JSONDecodeError as e:
        die(f"preferences.json is not valid JSON: {e}")


def cmd_get():
    print(json.dumps(_get("/__api/prefs"), sort_keys=True))


def cmd_post(body_json):
    try:
        body = json.loads(body_json)
    except json.JSONDecodeError as e:
        die(f"bad --json-body: {e}")
    print(json.dumps(_post("/__api/prefs", body), sort_keys=True))


def cmd_reboot(timeout="300"):
    print(json.dumps(_post_then_wait("/__api/prefs/reboot", None, float(timeout)), sort_keys=True))


def cmd_vault(path, timeout="600"):
    print(json.dumps(_post_then_wait("/__api/prefs/vault", {"path": path}, float(timeout)), sort_keys=True))


def cmd_config_js_has(token):
    body, _ = _get_text("/js/config.js")
    m = re.search(r'penpotFlags\s*=\s*"([^"]*)"', body)
    flags = m.group(1).split() if m else []
    print("yes" if token in flags else "no")


def cmd_csp_present():
    _, headers = _get_text("/index.html")
    val = headers.get("Content-Security-Policy")
    print(f"yes|{val}" if val else "no|")


def cmd_wait_bool(field, expected, timeout):
    want = expected.strip().lower() == "true"
    deadline = time.time() + float(timeout)
    last = "?"
    while time.time() < deadline:
        d = _get("/__api/prefs")
        cur = _dotted_get(d, field)
        last = cur
        if cur == want:
            print("OK")
            return
        time.sleep(1)
    die(f"wait_bool({field}) timed out after {timeout}s: last={last!r} want={want}", 1)


def main():
    if len(sys.argv) < 2:
        die("usage: d4_prefs_helper.py <cmd> ...", 64)
    cmd, args = sys.argv[1], sys.argv[2:]
    fn = {
        "prefs_file": cmd_prefs_file,
        "get": cmd_get,
        "post": cmd_post,
        "reboot": cmd_reboot,
        "vault": cmd_vault,
        "config_js_has": cmd_config_js_has,
        "csp_present": cmd_csp_present,
        "wait_bool": cmd_wait_bool,
    }.get(cmd)
    if fn is None:
        die(f"unknown subcommand {cmd}", 64)
    fn(*args)


if __name__ == "__main__":
    main()
