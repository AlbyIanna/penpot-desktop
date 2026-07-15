#!/usr/bin/env python3
"""E2 package-home / lockfile / installer gate helper (scripts/e2-packages.sh).

Subcommands, each printing human PASS/FAIL lines and exiting 0/1. Reuses
roundtrip.py as a Penpot RPC client library (rt.Client, tree hashing, in-place
import) exactly as the N6 helper does.

  new_template <base> <vault_root> <template_id> <wait_s>
      Import a builtin template via /__api/templates/new and wait for the sync
      daemon to materialize its `.penpot` tree on disk. Prints, on the last
      line, JSON {fileId, relPath} — the gate then copies that tree into
      .penpot-packages/<id> to AUTHOR a package (a package is just a folder).

  blind_check <base> <vault_root> <wait_s>
      The daemon-blindness proof (PLAN3 E2 invariant 1). Snapshots the sync
      manifest (.penpot-sync.json) + the boards count, then EDITS a file inside
      every `.penpot` tree under .penpot-packages/, waits a full sync window,
      and asserts: the manifest is byte-unchanged (no package got an entry), the
      boards count is unchanged (nothing imported/indexed), lock.json still
      pins 0 packages (no auto-install), and NO `.conflict-*` copy appeared
      anywhere in the vault.

  install_verify <base> <backend> <token> <vault_root> <pkg_id> <wait_s>
      POST /__api/packages/install {id}. Asserts the response (fileId, 64-hex
      contentHash + contractHash, settleCycles under the cap), that lock.json
      gained a matching entry, that the file materialized on disk, and that the
      materialized tree ROUND-TRIPS A=B (two equal semantic hashes — the N6 P0
      fixpoint). Prints a JSON report on the last line.

  assert_reapply <base> <vault_root> <lock_snapshot>
      After a delete-DB + reboot: GET /__api/packages and assert EVERY locked
      package is live in the DB with its recorded fileId (M2 resurrect-by-id),
      and that lock.json is byte-identical to <lock_snapshot>.

  idempotent <base> <vault_root> <pkg_id>
      POST install a second time; assert it is a no-op (alreadyInstalled=true,
      settleCycles=0) — run-twice = no phantom diff.

Stdlib only + scripts/roundtrip.py.
"""

import hashlib
import json
import os
import re
import shutil
import sys
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
MANIFEST_NAME = ".penpot-sync.json"
LOCK_NAME = "lock.json"
PACKAGES_DIR = ".penpot-packages"
HEX64 = re.compile(r"^[0-9a-f]{64}$")
# Mirror apps/desktop/src/installer.rs MAX_SETTLE_CYCLES.
MAX_SETTLE_CYCLES = 3


# --------------------------------------------------------------- HTTP helpers

def http_get(url, timeout=60):
    req = urllib.request.Request(url, headers={"Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return resp.status, resp.read()


def http_post_json(url, payload, timeout=900):
    body = json.dumps(payload).encode()
    req = urllib.request.Request(
        url, data=body,
        headers={"Content-Type": "application/json", "Accept": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def boards_count(base):
    try:
        st, bd = http_get(base + "/__api/vault/boards")
        if st == 200:
            return len(json.loads(bd).get("boards", []))
    except Exception:
        pass
    return None


def resolve_rel_path(base, file_id, wait_s):
    """Wait until the daemon materializes + indexes `file_id`; return relPath."""
    deadline = time.time() + float(wait_s)
    while time.time() < deadline:
        try:
            st, bd = http_get(base + "/__api/vault/boards")
            if st == 200:
                for c in json.loads(bd).get("boards", []):
                    if c.get("fileId") == file_id:
                        return c.get("relPath")
        except Exception:
            pass
        time.sleep(2)
    return None


def read_lock(vault_root):
    p = os.path.join(vault_root, LOCK_NAME)
    if not os.path.exists(p):
        return None
    with open(p, "rb") as fh:
        return json.loads(fh.read())


def find_conflicts(vault_root):
    hits = []
    for dirpath, dirs, _files in os.walk(vault_root):
        for d in dirs:
            if ".conflict-" in d:
                hits.append(os.path.relpath(os.path.join(dirpath, d), vault_root))
    return hits


def penpot_trees_under(root):
    """Every `.penpot` dir directly beneath a package dir under .penpot-packages."""
    pkgs_root = os.path.join(root, PACKAGES_DIR)
    out = []
    if not os.path.isdir(pkgs_root):
        return out
    for pkg in sorted(os.listdir(pkgs_root)):
        pkg_dir = os.path.join(pkgs_root, pkg)
        if not os.path.isdir(pkg_dir):
            continue
        for child in sorted(os.listdir(pkg_dir)):
            if child.endswith(".penpot") and os.path.isdir(os.path.join(pkg_dir, child)):
                out.append(os.path.join(pkg_dir, child))
    return out


# ------------------------------------------------------------------ commands

def new_template(base, vault_root, template_id, wait_s):
    status, body = http_post_json(
        base + "/__api/templates/new", {"templateId": template_id},
        timeout=max(600.0, float(wait_s) * 4))
    if status != 200:
        print(f"FAIL: new-from-template {template_id} HTTP {status}: {body[:200]!r}")
        return 1
    resp = json.loads(body)
    file_id = resp.get("fileId")
    if not file_id:
        print(f"FAIL: no fileId in template response: {resp}")
        return 1
    rel = resolve_rel_path(base, file_id, wait_s)
    if not rel or not os.path.isdir(os.path.join(vault_root, rel)):
        print(f"FAIL: template {template_id} never materialized within {wait_s}s")
        return 1
    print(f"PASS: authored source tree from template {template_id} → {rel}")
    print(json.dumps({"fileId": file_id, "relPath": rel}))
    return 0


def blind_check(base, vault_root, wait_s):
    ok = True

    def check(cond, msg):
        nonlocal ok
        print(("PASS: " if cond else "FAIL: ") + msg)
        ok = ok and cond

    manifest_path = os.path.join(vault_root, MANIFEST_NAME)
    before = open(manifest_path, "rb").read() if os.path.exists(manifest_path) else b""
    before_boards = boards_count(base)

    trees = penpot_trees_under(vault_root)
    check(len(trees) >= 2,
          f"at least two package .penpot trees dropped under {PACKAGES_DIR}/ "
          f"(found {len(trees)})")
    # Edit a file INSIDE each package tree (a real fs event under the home).
    for t in trees:
        # modify an existing json if any, and drop a stray file.
        edited = False
        for dirpath, _dirs, files in os.walk(t):
            for fn in files:
                if fn.endswith(".json"):
                    with open(os.path.join(dirpath, fn), "ab") as fh:
                        fh.write(b"\n")  # byte-level edit inside the package
                    edited = True
                    break
            if edited:
                break
        with open(os.path.join(t, "EDITED-INSIDE-PACKAGE.txt"), "w") as fh:
            fh.write("the daemon must never see this\n")

    # Wait a full poll + debounce + would-be-export window.
    time.sleep(float(wait_s))

    after = open(manifest_path, "rb").read() if os.path.exists(manifest_path) else b""
    check(before == after,
          "sync manifest byte-unchanged after editing inside the package home "
          "(no package got a manifest entry)")
    if before:
        man = json.loads(before)
        under_home = [e["path"] for e in man.get("files", {}).values()
                      if PACKAGES_DIR in e.get("path", "")]
        check(not under_home,
              f"no manifest entry references {PACKAGES_DIR}/ (never enumerated)")

    after_boards = boards_count(base)
    check(before_boards == after_boards,
          f"boards count unchanged ({before_boards}→{after_boards}); "
          "no package imported/indexed")

    lock = read_lock(vault_root)
    check(lock is None or not lock.get("packages"),
          "lock.json pins 0 packages (no auto-install — install is explicit)")

    conflicts = find_conflicts(vault_root)
    check(not conflicts, f"no .conflict-* copy anywhere in the vault ({conflicts})")

    return 0 if ok else 1


def _import_roundtrip(rt, client, penpot_dir, file_id, project_id):
    """A=B fixpoint check: A = semantic hash of the on-disk tree; re-zip →
    import IN-PLACE (same file id) → re-export (embedAssets) → normalize → B."""
    files_a = rt.tree_files(penpot_dir)
    hash_a = rt.tree_hash(rt.semantic_files(files_a))
    zip_a = rt.zip_tree(penpot_dir)
    rt.import_binfile(client, project_id, zip_a, file_id=file_id, name="e2-settle")
    end = client.rpc_sse("export-binfile", {
        "fileId": file_id, "includeLibraries": False, "embedAssets": True})
    zip_b = client.download(rt.parse_transit_uri(end))
    work_b = penpot_dir + ".rtB"
    rt.unzip_to(zip_b, work_b)
    files_b = rt.tree_files(work_b)
    hash_b = rt.tree_hash(rt.semantic_files(files_b))
    shutil.rmtree(work_b, ignore_errors=True)
    return hash_a, hash_b


def install_verify(base, backend, token, vault_root, pkg_id, wait_s):
    ok = True

    def check(cond, msg):
        nonlocal ok
        print(("PASS: " if cond else "FAIL: ") + msg)
        ok = ok and cond

    status, body = http_post_json(
        base + "/__api/packages/install", {"id": pkg_id},
        timeout=max(600.0, float(wait_s) * 4))
    if status != 200:
        print(f"FAIL: install {pkg_id} HTTP {status}: {body[:300]!r}")
        return 1
    resp = json.loads(body)
    file_id = resp.get("fileId", "")
    check(resp.get("ok") is True and bool(file_id), f"install {pkg_id} ok (fileId={file_id})")
    check(not resp.get("alreadyInstalled"), "first install is a real import (not a no-op)")
    check(bool(HEX64.match(resp.get("contentHash", ""))), "contentHash is a 64-hex digest")
    check(bool(HEX64.match(resp.get("contractHash", ""))), "contractHash is a 64-hex digest")
    cycles = resp.get("settleCycles", 99)
    check(0 <= cycles <= MAX_SETTLE_CYCLES,
          f"settled within the cap (settleCycles={cycles} ≤ {MAX_SETTLE_CYCLES})")

    # Lock entry written + consistent with the response.
    lock = read_lock(vault_root) or {}
    entry = lock.get("packages", {}).get(pkg_id)
    check(entry is not None, f"lock.json gained an entry for {pkg_id}")
    if entry:
        check(entry.get("fileId") == file_id, "lock fileId matches the installed file")
        check(entry.get("contentHash") == resp.get("contentHash"), "lock contentHash matches")
        check(entry.get("contractHash") == resp.get("contractHash"), "lock contractHash matches")
        check("version" in entry and "sourceGitUrl" in entry,
              "lock entry records version + sourceGitUrl")

    # Materialize on disk, then prove the fixpoint (two equal semantic hashes).
    rel = resolve_rel_path(base, file_id, wait_s)
    check(bool(rel) and os.path.isdir(os.path.join(vault_root, rel)),
          f"installed package materialized as an ordinary vault file ({rel})")
    ab = None
    if rel:
        sys.path.insert(0, HERE)
        os.environ["PENPOT_BACKEND"] = backend
        os.environ["PENPOT_FRONTEND"] = base
        os.environ["PENPOT_TOKEN"] = token
        import roundtrip as rt
        rt.BACKEND, rt.FRONTEND, rt.TOKEN = backend, base, token
        client = rt.Client()
        finfo = client.rpc("get-file", {"id": file_id})
        project_id = finfo.get("projectId") if isinstance(finfo, dict) else None
        if project_id:
            penpot_dir = os.path.join(vault_root, rel)
            hash_a, hash_b = _import_roundtrip(rt, client, penpot_dir, file_id, project_id)
            ab = hash_a == hash_b
            check(ab, f"materialized tree round-trips A=B (fixpoint) "
                      f"A={hash_a[:12]} B={hash_b[:12]}")
        else:
            check(False, "could not resolve projectId for the fixpoint check")

    print(json.dumps({"id": pkg_id, "fileId": file_id, "relPath": rel,
                      "settleCycles": cycles, "roundTripAB": ab,
                      "sourceGitUrl": resp.get("sourceGitUrl")}))
    return 0 if ok else 1


def assert_reapply(base, vault_root, lock_snapshot):
    ok = True

    def check(cond, msg):
        nonlocal ok
        print(("PASS: " if cond else "FAIL: ") + msg)
        ok = ok and cond

    status, body = http_get(base + "/__api/packages")
    if status != 200:
        print(f"FAIL: /__api/packages HTTP {status}")
        return 1
    data = json.loads(body)
    pkgs = data.get("packages", [])
    check(len(pkgs) >= 1, f"lockfile pins {len(pkgs)} package(s) after reboot")
    for p in pkgs:
        check(p.get("live") is True,
              f"{p['id']}: fileId {p['fileId']} live in the DB after wipe "
              "(M2 resurrect-by-id, invariant 1)")

    # lock.json byte-identical to the pre-wipe snapshot.
    now = open(os.path.join(vault_root, LOCK_NAME), "rb").read()
    snap = open(lock_snapshot, "rb").read()
    check(now == snap, "lock.json byte-identical across the delete-DB + reboot")
    return 0 if ok else 1


def idempotent(base, vault_root, pkg_id):
    ok = True

    def check(cond, msg):
        nonlocal ok
        print(("PASS: " if cond else "FAIL: ") + msg)
        ok = ok and cond

    status, body = http_post_json(base + "/__api/packages/install", {"id": pkg_id})
    if status != 200:
        print(f"FAIL: re-install {pkg_id} HTTP {status}: {body[:200]!r}")
        return 1
    resp = json.loads(body)
    check(resp.get("ok") is True, f"re-install {pkg_id} ok")
    check(resp.get("alreadyInstalled") is True,
          "second install is a no-op (alreadyInstalled=true)")
    check(resp.get("settleCycles") == 0, "no settle cycles on the no-op re-install")
    return 0 if ok else 1


def main():
    if len(sys.argv) < 2:
        print("usage: e2_packages_helper.py <subcommand> ...", file=sys.stderr)
        return 2
    cmd, a = sys.argv[1], sys.argv[2:]
    if cmd == "new_template":
        return new_template(a[0], a[1], a[2], a[3])
    if cmd == "blind_check":
        return blind_check(a[0], a[1], a[2])
    if cmd == "install_verify":
        return install_verify(a[0], a[1], a[2], a[3], a[4], a[5])
    if cmd == "assert_reapply":
        return assert_reapply(a[0], a[1], a[2])
    if cmd == "idempotent":
        return idempotent(a[0], a[1], a[2])
    print(f"unknown subcommand {cmd!r}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
