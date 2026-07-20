#!/usr/bin/env python3
"""Helper for scripts/d3-menus.sh (the D3 menu-bar gate, PLAN4 milestone D3).

D3's menu commands New File / New Project go through the backend by RPC
(`menubar/mod.rs`'s `create_new_file`/`create_new_project` doc comments say so
explicitly: "the SAME RPC `manage.rs`'s `/__api/vault/manage/{file,project}`
route calls"), so driving those routes exercises the identical backend call
the menu item would make — same technique scripts/d2_home_helper.py already
established for the D2 lifecycle. This helper is deliberately NOT a copy of
that one: it only needs "create a project, create a file in it, prove it
lands on disk", not the full rename/duplicate/move/delete lifecycle, so it
carries its own small `manage_post`/`poll`/disk-wait functions rather than
importing d2_home_helper (which pulls in roundtrip.py's authenticated RPC
client and n5_vaults_helper's board-seeding — machinery this gate doesn't
need). Every other gate helper in this repo (d2/e2/e3/e4/n5...) makes the
same call: small, gate-local duplicates of `manage_post`/`poll`, not a shared
library.

Talks to the stack THROUGH THE PROXY. The mutation routes under
/__api/vault/manage/* need NO caller auth (they run through the proxy's own
server-side token, manage.rs::ManageState) — no Authorization header needed.

Subcommand:
  new-file-and-project <workdir> <vault_root> <timeout>
      Creates a project (POST manage/project), creates a file in it (POST
      manage/file — same RPC New File's menu handler calls), then polls the
      live folder tree + `.penpot-sync.json` manifest (never trusts the
      first read — the sync daemon polls on its own ~2s cadence, CLAUDE.md
      gotcha) until the file is a real directory on disk under its project.
      Prints one JSON summary {"ok": bool, "steps": [...], "ids": {...}} and
      exits non-zero if any step failed.
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request

MANIFEST_NAME = ".penpot-sync.json"
POLL_INTERVAL = 2.0  # matches the sync daemon's own poll cadence


def die(msg, code=2):
    print(f"HELPER-FAIL: {msg}", file=sys.stderr)
    sys.exit(code)


def manage_post(base, path, body):
    data = json.dumps(body).encode()
    req = urllib.request.Request(
        f"{base}/__api/vault/manage/{path}",
        data=data,
        headers={"Content-Type": "application/json", "Accept": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req) as resp:
            raw = resp.read()
            return json.loads(raw) if raw else {}
    except urllib.error.HTTPError as e:
        detail = e.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"POST /__api/vault/manage/{path} -> HTTP {e.code}: {detail}") from e


def poll(check, timeout, interval=POLL_INTERVAL):
    """Call check() -> (ok, info) until True or the timeout elapses. Never
    trusts the first read — always sleeps between the first failure and a
    retry. Returns the LAST (ok, info) observed."""
    deadline = time.time() + float(timeout)
    ok, info = check()
    if ok:
        return ok, info
    while time.time() < deadline:
        time.sleep(interval)
        ok, info = check()
        if ok:
            return ok, info
    return ok, info


def load_manifest(vault_root):
    path = os.path.join(vault_root, MANIFEST_NAME)
    if not os.path.exists(path):
        return None
    with open(path) as fh:
        return json.load(fh)


def manifest_entry(vault_root, file_id):
    m = load_manifest(vault_root)
    if not m:
        return None
    return m.get("files", {}).get(file_id)


def dir_exists(vault_root, rel_path):
    return bool(rel_path) and os.path.isdir(os.path.join(vault_root, rel_path))


def wait_file_on_disk(vault_root, file_id, expect_project_id, timeout):
    def check():
        e = manifest_entry(vault_root, file_id)
        if e is None:
            return False, "not yet in the manifest"
        if expect_project_id and e.get("projectId") != expect_project_id:
            return False, f"manifest projectId={e.get('projectId')!r} != expected {expect_project_id!r}"
        path = e.get("path", "")
        if "/" not in path:
            return False, f"path {path!r} is not nested under a project folder"
        if not dir_exists(vault_root, path):
            return False, f"manifest path {path!r} is not yet a directory on disk"
        return True, path

    return poll(check, timeout)


def cmd_new_file_and_project(workdir, vault_root, timeout):
    base = os.environ["PENPOT_BACKEND"]
    steps = []
    ids = {}

    def record(name, ok, detail=""):
        steps.append({"name": name, "ok": bool(ok), "detail": detail})
        return ok

    try:
        proj = manage_post(base, "project", {"name": "D3 Menu-Gate Project"})
        ids["projectId"] = proj["projectId"]
        record("new-project-rpc", bool(ids["projectId"]), ids["projectId"])

        f = manage_post(base, "file", {"projectId": ids["projectId"], "name": "D3 Menu-Gate File"})
        ids["fileId"] = f["fileId"]
        record("new-file-rpc", bool(ids["fileId"]), ids["fileId"])

        ok, info = wait_file_on_disk(vault_root, ids["fileId"], ids["projectId"], timeout)
        record("disk-after-new-file", ok, info)
        if not ok:
            raise RuntimeError(f"disk-after-new-file failed: {info}")
        ids["filePathOnDisk"] = info
    except Exception as e:  # noqa: BLE001 — report partial progress, never crash silently
        record("EXCEPTION", False, str(e))

    overall_ok = all(s["ok"] for s in steps) and len(steps) > 0
    out = {"ok": overall_ok, "steps": steps, "ids": ids}
    with open(os.path.join(workdir, "d3-new-file-state.json"), "w") as fh:
        json.dump(out, fh, indent=2, sort_keys=True)
    print(json.dumps(out, indent=2, sort_keys=True))
    sys.exit(0 if overall_ok else 1)


def main():
    if len(sys.argv) < 2:
        die("usage: d3_menus_helper.py <cmd> ...", 64)
    cmd = sys.argv[1]
    if cmd == "new-file-and-project":
        if len(sys.argv) != 5:
            die("usage: d3_menus_helper.py new-file-and-project <workdir> <vault_root> <timeout>", 64)
        cmd_new_file_and_project(sys.argv[2], sys.argv[3], sys.argv[4])
    else:
        die(f"unknown subcommand {cmd}", 64)


if __name__ == "__main__":
    main()
