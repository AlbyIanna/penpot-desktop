#!/usr/bin/env python3
"""RPC + HTTP helper for scripts/d2-home.sh (the D2 front-door gate).

Reuses scripts/roundtrip.py (M0-verified RPC client, token auth) and borrows
scripts/n5_vaults_helper.py's `frame_obj`/`geometry` shape builders (a single
top-level "frame" object is exactly what `crates/vault-index/src/extract.rs`
treats as a board — see its `DocKind::Board` doc comment). Tree hashing is
NOT duplicated here: the shell calls `n5_vaults_helper.py tree_hash` directly
(brief instruction: reuse rather than write a third hasher).

Talks to the stack THROUGH THE PROXY:
  PENPOT_BACKEND   proxy base url (http://localhost:<proxy-port>)
  PENPOT_TOKEN     access token from <data_dir>/credentials.json — needed
                   for the direct RPC calls this script makes (seeding a
                   board, reading get-project-files). The mutation routes
                   under /__api/vault/manage/* need NO caller auth at all:
                   they run through the proxy's own server-side token
                   (manage.rs::ManageState), so the POSTs below carry no
                   Authorization header.

Subcommands:
  lifecycle <workdir> <vault_root> <timeout>
      Drives the whole D2 lifecycle through /__api/vault/manage/* ONLY:
      create project A, create project B (the move target), create a file in
      A, seed a real board into it (direct RPC — so it has content and later
      shows up as a board card), rename the file, duplicate it (staying in
      A), move the duplicate to B, delete the original. After EVERY
      mutating call this polls the live folder tree + `.penpot-sync.json`
      manifest (never trusts the first read — the sync daemon polls on its
      own ~2s cadence) until the expected on-disk state appears, within
      <timeout> seconds per step. Writes <workdir>/d2-state.json (ids +
      recorded paths, consumed by the other subcommands) and prints one JSON
      summary: {"ok": bool, "steps": [{"name","ok","detail"}], "ids": {...}}.
      Exits non-zero if any step failed.
  wait-present <workdir> <vault_root> <timeout>
      Post-restart: poll until the SURVIVING file (the moved duplicate) is
      back in the DB (get-project-files on its new project) AND back on disk
      at its manifest path. Mirrors n5_vaults_helper.py's cmd_wait_present —
      same "startup reconciliation runs async after boot() returns" reason
      for polling instead of asserting on the first read. This is also the
      proof-of-looking gate for `assert-disk`: if the RPC/disk plumbing
      can't even see the file that's SUPPOSED to still be there, an absence
      reading for the deleted file would be vacuous, not evidence.
  assert-disk <workdir> <vault_root> <window_secs> deleted-stays-deleted
      THE load-bearing check. Samples, every ~2s for <window_secs>, that the
      DELETED original file has NOT come back: not on disk (its last live
      path), not in the manifest, not in the DB (get-project-files on its
      original project). Any single positive sighting during the window is
      an immediate FAIL — "came back a few seconds after boot" is exactly
      the bug this encodes against. Run `wait-present` first so a silent
      RPC/disk outage can't masquerade as "still gone".
  wait-board-indexed <workdir> <timeout>
      Poll /__api/vault/boards until the surviving file's seeded board is
      indexed (the vault index lags the daemon's own poll by one further
      cycle — CLAUDE.md gotcha) — run before the browser leg so the board
      card it needs to click/read actually exists in the grid.
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import roundtrip as rt  # noqa: E402  (reads PENPOT_BACKEND/PENPOT_TOKEN env)
import n5_vaults_helper as n5  # noqa: E402  (reuses frame_obj/geometry — also reads PENPOT_BACKEND at import)

MANIFEST_NAME = ".penpot-sync.json"
POLL_INTERVAL = 2.0  # matches the sync daemon's own poll cadence


def die(msg, code=2):
    print(f"HELPER-FAIL: {msg}", file=sys.stderr)
    sys.exit(code)


# --------------------------------------------------------------------- state


def state_path(workdir):
    return os.path.join(workdir, "d2-state.json")


def save_state(workdir, state):
    with open(state_path(workdir), "w") as fh:
        json.dump(state, fh, indent=2, sort_keys=True)


def load_state(workdir):
    with open(state_path(workdir)) as fh:
        return json.load(fh)


# ---------------------------------------------------------------- disk / DB


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


def poll(check, timeout, interval=POLL_INTERVAL):
    """Call check() -> (ok, info) until it's True or the timeout elapses.
    Never trusts the first read — always sleeps between the first failure and
    a retry. Returns the LAST (ok, info) observed, including one final
    read right at the deadline."""
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


# -------------------------------------------------------------- manage API


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


def http_get_json(url):
    with urllib.request.urlopen(url) as resp:
        return json.loads(resp.read())


def get_project_file_ids(client, project_id):
    files = client.rpc("get-project-files", {"projectId": project_id})
    return {f["id"] for f in files}


# --------------------------------------------------------------- seed board


def seed_board(client, file_id):
    """Add one top-level frame ("board") to file_id's first page via direct
    RPC, so it has real content: a board card on /__home, and content that
    survives export/import when it's later duplicated."""
    g = client.rpc("get-file", {"id": file_id})
    page_id = g["data"]["pages"][0]
    board_id = str(uuid.uuid4())
    board = n5.frame_obj(board_id, "D2 Board", 0, 0, 400, 300)
    client.rpc(
        "update-file",
        {
            "id": file_id,
            "sessionId": str(uuid.uuid4()),
            "revn": g["revn"],
            "vern": g["vern"],
            "changes": [
                {
                    "type": "add-obj",
                    "id": board_id,
                    "pageId": page_id,
                    "frameId": rt.ROOT_FRAME,
                    "parentId": rt.ROOT_FRAME,
                    "obj": board,
                }
            ],
        },
    )
    return board_id, page_id


# ------------------------------------------------------------ disk waiters


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


def wait_rename_on_disk(vault_root, file_id, old_path, timeout):
    def check():
        e = manifest_entry(vault_root, file_id)
        if e is None:
            return False, "missing from the manifest"
        new_path = e.get("path", "")
        if new_path == old_path:
            return False, "manifest path has not changed yet"
        if not dir_exists(vault_root, new_path):
            return False, f"new path {new_path!r} is not yet a directory on disk"
        if os.path.isdir(os.path.join(vault_root, old_path)):
            return False, f"OLD path {old_path!r} STILL EXISTS on disk (rename did not move it)"
        return True, new_path

    return poll(check, timeout)


def wait_move_on_disk(vault_root, file_id, target_project_id, old_path, timeout):
    def check():
        e = manifest_entry(vault_root, file_id)
        if e is None:
            return False, "missing from the manifest"
        if e.get("projectId") != target_project_id:
            return False, f"manifest projectId still {e.get('projectId')!r}"
        new_path = e.get("path", "")
        if not dir_exists(vault_root, new_path):
            return False, f"new path {new_path!r} is not yet a directory on disk"
        if new_path == old_path:
            return False, "manifest path unchanged even though projectId updated"
        if os.path.isdir(os.path.join(vault_root, old_path)):
            return False, f"OLD path {old_path!r} STILL EXISTS on disk (move did not relocate it)"
        return True, new_path

    return poll(check, timeout)


def assert_delete_on_disk(vault_root, file_id, old_path, trashed_rel):
    """Delete is synchronous (manage.rs's delete_inner awaits the trash move
    and the manifest save before the HTTP response returns), so this is a
    single read, not a poll — a poll would silently forgive a handler that
    responded before finishing its own work."""
    problems = []
    if os.path.isdir(os.path.join(vault_root, old_path)):
        problems.append(f"live path {old_path!r} still exists after delete")
    norm = trashed_rel.replace("\\", "/")
    if not norm.startswith(".trash/"):
        problems.append(f"trashedPath {trashed_rel!r} is not under .trash/")
    if not os.path.isdir(os.path.join(vault_root, trashed_rel)):
        problems.append(f"trashedPath {trashed_rel!r} is not a directory on disk")
    if manifest_entry(vault_root, file_id) is not None:
        problems.append("manifest still has an entry for the deleted file")
    return (len(problems) == 0), problems


# --------------------------------------------------------------- lifecycle


def cmd_lifecycle(workdir, vault_root, timeout):
    base = os.environ["PENPOT_BACKEND"]
    client = rt.Client()
    client.login()  # token path: just a get-profile sanity ping

    steps = []
    ids = {}

    def record(name, ok, detail=""):
        steps.append({"name": name, "ok": bool(ok), "detail": detail})
        return ok

    try:
        pa = manage_post(base, "project", {"name": "D2 Project A"})
        ids["projectAId"] = pa["projectId"]
        record("create-project-a", True, ids["projectAId"])

        pb = manage_post(base, "project", {"name": "D2 Project B"})
        ids["projectBId"] = pb["projectId"]
        record("create-project-b", True, ids["projectBId"])

        fa = manage_post(base, "file", {"projectId": ids["projectAId"], "name": "D2 Original File"})
        ids["fileAId"] = fa["fileId"]
        record("create-file", True, ids["fileAId"])

        ok, info = wait_file_on_disk(vault_root, ids["fileAId"], ids["projectAId"], timeout)
        record("disk-after-create", ok, info)
        if not ok:
            raise RuntimeError(f"disk-after-create failed: {info}")
        ids["fileAPathAfterCreate"] = info

        board_id, page_id = seed_board(client, ids["fileAId"])
        ids["boardId"] = board_id
        ids["pageId"] = page_id
        record("seed-board", True, board_id)

        manage_post(base, "rename", {"kind": "file", "id": ids["fileAId"], "name": "D2 Original File Renamed"})
        ok, info = wait_rename_on_disk(vault_root, ids["fileAId"], ids["fileAPathAfterCreate"], timeout)
        record("disk-after-rename", ok, info)
        if not ok:
            raise RuntimeError(f"disk-after-rename failed: {info}")
        ids["fileAPathAfterRename"] = info

        dup = manage_post(
            base, "duplicate",
            {"fileId": ids["fileAId"], "name": "D2 Duplicate File", "projectId": ids["projectAId"]},
        )
        ids["fileBId"] = dup["fileId"]
        record("duplicate-rpc", ids["fileBId"] != ids["fileAId"], ids["fileBId"])
        ok, info = wait_file_on_disk(vault_root, ids["fileBId"], ids["projectAId"], timeout)
        record("disk-after-duplicate", ok and ids["fileBId"] != ids["fileAId"], info)
        if not ok:
            raise RuntimeError(f"disk-after-duplicate failed: {info}")
        ids["fileBPathAfterDuplicate"] = info

        manage_post(base, "move", {"fileIds": [ids["fileBId"]], "projectId": ids["projectBId"]})
        ok, info = wait_move_on_disk(vault_root, ids["fileBId"], ids["projectBId"], ids["fileBPathAfterDuplicate"], timeout)
        record("disk-after-move", ok, info)
        if not ok:
            raise RuntimeError(f"disk-after-move failed: {info}")
        ids["fileBPathAfterMove"] = info

        delres = manage_post(base, "delete", {"fileId": ids["fileAId"]})
        ids["fileATrashedPath"] = delres["trashedPath"]
        ok, problems = assert_delete_on_disk(
            vault_root, ids["fileAId"], ids["fileAPathAfterRename"], ids["fileATrashedPath"]
        )
        record("disk-after-delete", ok, "; ".join(problems) if problems else ids["fileATrashedPath"])
        if not ok:
            raise RuntimeError(f"disk-after-delete failed: {problems}")

    except Exception as e:  # noqa: BLE001 — report partial progress, never crash silently
        record("EXCEPTION", False, str(e))

    overall_ok = all(s["ok"] for s in steps) and len(steps) > 0
    save_state(workdir, {"ids": ids, "steps": steps})
    out = {"ok": overall_ok, "steps": steps, "ids": ids}
    print(json.dumps(out, indent=2, sort_keys=True))
    sys.exit(0 if overall_ok else 1)


# --------------------------------------------------------------- wait-present


def cmd_wait_present(workdir, vault_root, timeout):
    """Post-restart: poll until the SURVIVING file (the moved duplicate) is
    back in the DB (its new project) and back on disk at its manifest path.
    This is the proof-of-looking gate for assert-disk below."""
    state = load_state(workdir)
    file_b_id = state["ids"]["fileBId"]
    project_b_id = state["ids"]["projectBId"]
    client = rt.Client()
    client.login()

    def check():
        try:
            ids = get_project_file_ids(client, project_b_id)
        except Exception as e:  # noqa: BLE001
            return False, f"rpc: {e}"
        if file_b_id not in ids:
            return False, f"surviving file not yet in project B's DB listing ({len(ids)} file(s) there)"
        e = manifest_entry(vault_root, file_b_id)
        if e is None:
            return False, "surviving file not yet in the manifest"
        if not dir_exists(vault_root, e["path"]):
            return False, f"surviving file's manifest path {e['path']!r} not yet a directory on disk"
        return True, e["path"]

    ok, info = poll(check, timeout)
    if not ok:
        die(f"wait-present timed out: {info}", 1)
    print("OK")


# --------------------------------------------------------------- assert-disk


def cmd_assert_disk_deleted_stays_deleted(workdir, vault_root, window_secs):
    """THE load-bearing check: sample, over <window_secs>, that the deleted
    original never comes back — not on disk, not in the manifest, not in the
    DB. Run `wait-present` first (proof the check mechanism itself works)."""
    state = load_state(workdir)
    file_a_id = state["ids"]["fileAId"]
    project_a_id = state["ids"]["projectAId"]
    old_path = state["ids"]["fileAPathAfterRename"]
    client = rt.Client()
    client.login()

    problems = []
    samples = 0
    deadline = time.time() + float(window_secs)
    while True:
        samples += 1
        if os.path.isdir(os.path.join(vault_root, old_path)):
            problems.append(f"RESURRECTED on disk at {old_path!r}")
            break
        if manifest_entry(vault_root, file_a_id) is not None:
            problems.append("RESURRECTED in the manifest")
            break
        try:
            ids = get_project_file_ids(client, project_a_id)
        except Exception as e:  # noqa: BLE001
            problems.append(f"RPC check failed (cannot rule out resurrection): {e}")
            break
        if file_a_id in ids:
            problems.append("RESURRECTED in the DB (get-project-files)")
            break
        if time.time() >= deadline:
            break
        time.sleep(POLL_INTERVAL)

    ok = len(problems) == 0
    out = {"ok": ok, "problems": problems, "samples": samples, "windowSecs": window_secs}
    print(json.dumps(out, indent=2, sort_keys=True))
    sys.exit(0 if ok else 1)


# --------------------------------------------------------- wait-board-indexed


def cmd_wait_board_indexed(workdir, timeout):
    state = load_state(workdir)
    file_b_id = state["ids"]["fileBId"]
    base = os.environ["PENPOT_BACKEND"]

    def check():
        try:
            data = http_get_json(f"{base}/__api/vault/boards")
        except Exception as e:  # noqa: BLE001
            return False, f"boards query failed: {e}"
        boards = data.get("boards", [])
        ids = {b.get("fileId") for b in boards}
        if file_b_id in ids:
            return True, f"{len(boards)} board(s) indexed, including the surviving file"
        return False, f"{len(boards)} board(s) indexed, surviving file not among them yet"

    ok, info = poll(check, timeout)
    if not ok:
        die(f"wait-board-indexed timed out: {info}", 1)
    print("OK")


# ----------------------------------------------------------------------- main


def main():
    if len(sys.argv) < 2:
        die("usage: d2_home_helper.py <cmd> ...", 64)
    cmd = sys.argv[1]
    if cmd == "lifecycle":
        cmd_lifecycle(sys.argv[2], sys.argv[3], sys.argv[4])
    elif cmd == "wait-present":
        cmd_wait_present(sys.argv[2], sys.argv[3], sys.argv[4])
    elif cmd == "assert-disk":
        state_name = sys.argv[5] if len(sys.argv) > 5 else ""
        if state_name != "deleted-stays-deleted":
            die(f"assert-disk: unknown state {state_name!r} (only 'deleted-stays-deleted' is implemented)", 64)
        cmd_assert_disk_deleted_stays_deleted(sys.argv[2], sys.argv[3], sys.argv[4])
    elif cmd == "wait-board-indexed":
        cmd_wait_board_indexed(sys.argv[2], sys.argv[3])
    else:
        die(f"unknown subcommand {cmd}", 64)


if __name__ == "__main__":
    main()
