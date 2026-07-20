#!/usr/bin/env python3
"""RPC + control-server + manifest helper for scripts/d5-documents.sh (the D5
document-based-app gate, PLAN4 milestone D5, `just d5`).

Reuses scripts/roundtrip.py (M0-verified RPC client) exactly like every other
gate helper. For the zero-cross-vault-spill leg (assertion d) this ALSO
imports scripts/n5_vaults_helper.py directly and calls its `db_file_ids` /
`boards_file_ids` / `search_count` functions rather than re-deriving the
spill check a second time — the brief is explicit that this leg must "reuse
the N5 spill assertions", and those three functions are the actual
assertions (not the label/expect.json bookkeeping the rest of that helper
needs, which this gate has no use for). Every other subcommand here is a
small, gate-local duplicate of the manage_post/poll boilerplate, same
convention scripts/d3_menus_helper.py's own doc comment describes.

Talks to the stack THROUGH THE PROXY (RPC + /__api/vault/manage/rename) and
to the GUI binary's localhost control server (GET /health, GET /windows) —
the ONLY way this gate can observe that a document actually opened inside
the real Tauri window registry (see apps/desktop/src/control.rs's module
doc: "a shell cannot reach into the Tauri process's WindowRegistry any other
way").

Env (set by the shell script):
  PENPOT_BACKEND   proxy base url (http://localhost:<proxy-port>) — required
                   even for subcommands that don't call it directly, because
                   importing n5_vaults_helper reads it at MODULE load time.
  PENPOT_TOKEN     access token from <data_dir>/credentials.json (re-read
                   after every vault switch — provisioning mints a fresh one)

Subcommands:
  seed_docs <workdir> <vault_root> <timeout>
      RPC-create project "D5DocsGate" with two bare files "DocA"/"DocB" (no
      board content needed — this gate only cares about file id/title/open,
      not shape data), poll each onto disk as a real `.penpot` dir. Writes
      <workdir>/docs.json {"projectId", "docs": {"DocA": {...}, "DocB": {...}}}
      where each entry is {"fileId","relPath","absPath","title"}.

  seed_needle_file <workdir> <vault_root> <name> <needle> <timeout>
      RPC-create ONE file <name> (its own fresh project) with a board + text
      layer carrying <needle> — same shape n5_vaults_helper's own seed uses
      for one file, reusing its frame_obj/text_obj builders directly. Polls
      it onto disk. Writes <workdir>/needle-<name>.json
      {"fileId","relPath","absPath","needle"} — this is the file assertion
      (d) copies OUT of the vault to stand in for "a `.penpot` a user has
      lying around outside their vault".

  wait_reachable <control_base> <timeout>
      Poll GET <control_base>/health then GET /windows until BOTH succeed
      AND the /windows body is a non-empty array containing the home window
      (label "main"). "Proof of looking" per the brief: an unreachable or
      empty control server must FAIL here, never silently read as "no
      document is open".

  wait_file_window <control_base> <file_id> <want_title> <timeout>
      Poll GET /windows until an entry has fileId == <file_id> AND
      title == <want_title>. Prints {"ok","label","title"} and exits 0, or
      {"ok":false,"reason"} and exits 1 on timeout.

  wait_window_title <control_base> <label> <want_title> <timeout>
      Poll GET /windows until the entry with this label has
      title == <want_title> (the rename-tracking leg of assertion b).
      Prints OK or dies with the last-seen title.

  window_count <control_base>
      Print the number of entries currently in GET /windows (a before/after
      baseline for assertion c's "did a NEW window appear" check).

  rename_file <file_id> <new_name>
      POST /__api/vault/manage/rename {"kind":"file",...} — the SAME route
      the (unavailable-here) Rename menu action would call. Prints the raw
      JSON response.

  wait_manifest_id <vault_root> <rel_path> <timeout>
      Poll <vault_root>/.penpot-sync.json until an entry exists AT
      <rel_path> (the sync daemon's Direction B noticing a new `.penpot` on
      disk and assigning it a file id) and print that id. Used after this
      script copies a `.penpot` into the vault's Imported/ folder itself
      (the brief's explicit fallback for the native-confirm-dialog leg that
      cannot be driven headlessly).

  assert_present <file_id> <needle>
      Query the CURRENTLY ACTIVE vault's DB (get-projects/get-project-files),
      /__api/vault/boards and /__api/vault/search — via n5_vaults_helper's
      OWN db_file_ids/boards_file_ids/search_count — and exit 0 only if
      <file_id> is present in ALL THREE and <needle> has >=1 search hit.
      Prints a JSON detail either way.

  assert_absent <file_id> <needle>
      The negative twin (the actual zero-cross-vault-spill check): exit 0
      only if <file_id> is in NONE of DB/boards and <needle> has 0 search
      hits.
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
import n5_vaults_helper as n5  # noqa: E402  (reused zero-spill primitives + shape builders)

MANIFEST_NAME = ".penpot-sync.json"
POLL_INTERVAL = 2.0  # matches the sync daemon's own poll cadence


def die(msg, code=2):
    print(f"HELPER-FAIL: {msg}", file=sys.stderr)
    sys.exit(code)


# --------------------------------------------------------------------- poll


def poll(check, timeout, interval=POLL_INTERVAL):
    """Call check() -> (ok, info) until True or the timeout elapses. Never
    trusts the first read. Returns the LAST (ok, info) observed."""
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


# ------------------------------------------------------------------ manifest


def load_manifest(vault_root):
    path = os.path.join(vault_root, MANIFEST_NAME)
    if not os.path.exists(path):
        return None
    with open(path) as fh:
        return json.load(fh)


def wait_file_on_disk(vault_root, file_id, expect_project_id, timeout):
    def check():
        m = load_manifest(vault_root)
        if m is None:
            return False, "no manifest yet"
        e = m.get("files", {}).get(file_id)
        if e is None:
            return False, "not yet in the manifest"
        if expect_project_id and e.get("projectId") != expect_project_id:
            return False, f"manifest projectId={e.get('projectId')!r} != expected {expect_project_id!r}"
        path = e.get("path", "")
        if not path or not os.path.isdir(os.path.join(vault_root, path)):
            return False, f"{path!r} is not yet a directory on disk"
        return True, path

    return poll(check, timeout)


# `docopen::display_title`'s rule, mirrored by hand (it's a two-line pure
# function on the Rust side too — see that function's own doc comment on why
# a second copy here is fine rather than a shared implementation).
def display_title(rel_path):
    base = rel_path.rsplit("/", 1)[-1]
    return base[: -len(".penpot")] if base.endswith(".penpot") else base


# ----------------------------------------------------------------- seed_docs


def cmd_seed_docs(workdir, vault_root, timeout):
    c = rt.Client()
    team = c.rpc("get-profile", {})["defaultTeamId"]
    proj = c.rpc("create-project", {"teamId": team, "name": "D5DocsGate"})
    docs = {}
    for name in ("DocA", "DocB"):
        f = c.rpc("create-file", {"name": name, "projectId": proj["id"]})
        fid = f["id"]
        ok, info = wait_file_on_disk(vault_root, fid, proj["id"], timeout)
        if not ok:
            die(f"{name} never landed on disk: {info}")
        docs[name] = {
            "fileId": fid,
            "relPath": info,
            "absPath": os.path.join(vault_root, info),
            "title": display_title(info),
        }
    out = {"projectId": proj["id"], "docs": docs}
    with open(os.path.join(workdir, "docs.json"), "w") as fh:
        json.dump(out, fh, indent=2, sort_keys=True)
    print(json.dumps(out, indent=2, sort_keys=True))


# ------------------------------------------------------------ seed_needle_file


def cmd_seed_needle_file(workdir, vault_root, name, needle, timeout):
    c = rt.Client()
    team = c.rpc("get-profile", {})["defaultTeamId"]
    proj = c.rpc("create-project", {"teamId": team, "name": "D5ImportSource"})
    created = c.rpc("create-file", {"name": name, "projectId": proj["id"]})
    fid, page = created["id"], created["data"]["pages"][0]
    board_id, text_id = str(uuid.uuid4()), str(uuid.uuid4())
    changes = [
        {
            "type": "add-obj", "id": board_id, "pageId": page,
            "frameId": rt.ROOT_FRAME, "parentId": rt.ROOT_FRAME,
            "obj": n5.frame_obj(board_id, "Board", 0, 0, 800, 600),
        },
        {
            "type": "add-obj", "id": text_id, "pageId": page,
            "frameId": board_id, "parentId": board_id,
            "obj": n5.text_obj(text_id, "needle", board_id, 40, 40, needle),
        },
    ]
    c.rpc("update-file", {
        "id": fid, "sessionId": str(uuid.uuid4()),
        "revn": created["revn"], "vern": created["vern"],
        "changes": changes,
    })
    ok, info = wait_file_on_disk(vault_root, fid, proj["id"], timeout)
    if not ok:
        die(f"needle file never landed on disk: {info}")
    out = {"fileId": fid, "relPath": info, "absPath": os.path.join(vault_root, info), "needle": needle}
    with open(os.path.join(workdir, f"needle-{name}.json"), "w") as fh:
        json.dump(out, fh, indent=2, sort_keys=True)
    print(json.dumps(out, indent=2, sort_keys=True))


# ------------------------------------------------------------- wait_manifest_id


def cmd_wait_manifest_id(vault_root, rel_path, timeout):
    def check():
        m = load_manifest(vault_root)
        if m is None:
            return False, "no manifest yet"
        for fid, e in m.get("files", {}).items():
            if e.get("path") == rel_path:
                return True, fid
        return False, f"{rel_path!r} not yet in the manifest (daemon hasn't imported it)"

    ok, info = poll(check, timeout)
    if not ok:
        die(f"wait_manifest_id({rel_path!r}) timed out: {info}")
    print(info)


# --------------------------------------------------------------- control server


def http_get_json(url, timeout=10):
    with urllib.request.urlopen(url, timeout=timeout) as resp:
        return json.loads(resp.read())


def cmd_wait_reachable(control_base, timeout):
    deadline = time.time() + float(timeout)
    last = "?"
    while time.time() < deadline:
        try:
            health = urllib.request.urlopen(f"{control_base}/health", timeout=5).read().decode()
            if health.strip() != "ok":
                last = f"/health returned {health!r}"
            else:
                body = http_get_json(f"{control_base}/windows")
                windows = body.get("windows")
                if not isinstance(windows, list) or not windows:
                    last = f"/windows empty or malformed: {body}"
                elif not any(w.get("label") == "main" for w in windows):
                    last = f"/windows has no home window: {body}"
                else:
                    print("OK")
                    return
        except Exception as e:  # noqa: BLE001 — still booting; keep polling
            last = str(e)
        time.sleep(1)
    die(f"control server never became reachable with a real, non-empty /windows body: {last}", 1)


def cmd_wait_file_window(control_base, file_id, want_title, timeout):
    deadline = time.time() + float(timeout)
    last = "?"
    while time.time() < deadline:
        try:
            body = http_get_json(f"{control_base}/windows")
            windows = body.get("windows", [])
            match = next((w for w in windows if w.get("fileId") == file_id), None)
            if match is None:
                last = f"no window with fileId={file_id!r} yet ({len(windows)} window(s) open)"
            elif match.get("title") != want_title:
                last = f"found the window but title={match.get('title')!r} != {want_title!r}"
            else:
                print(json.dumps({"ok": True, "label": match["label"], "title": match["title"]}))
                return
        except Exception as e:  # noqa: BLE001
            last = str(e)
        time.sleep(1)
    print(json.dumps({"ok": False, "reason": last}))
    sys.exit(1)


def cmd_wait_window_title(control_base, label, want_title, timeout):
    deadline = time.time() + float(timeout)
    last = "?"
    while time.time() < deadline:
        try:
            body = http_get_json(f"{control_base}/windows")
            windows = body.get("windows", [])
            match = next((w for w in windows if w.get("label") == label), None)
            if match is None:
                last = f"label {label!r} not found ({len(windows)} window(s) open)"
            elif match.get("title") != want_title:
                last = f"title is {match.get('title')!r}"
            else:
                print("OK")
                return
        except Exception as e:  # noqa: BLE001
            last = str(e)
        time.sleep(1)
    die(f"wait_window_title({label!r}, {want_title!r}) timed out: {last}", 1)


def cmd_window_count(control_base):
    body = http_get_json(f"{control_base}/windows")
    print(len(body.get("windows", [])))


# ------------------------------------------------------------------- rename


def cmd_rename_file(file_id, new_name):
    base = os.environ["PENPOT_BACKEND"]
    data = json.dumps({"kind": "file", "id": file_id, "name": new_name}).encode()
    req = urllib.request.Request(
        f"{base}/__api/vault/manage/rename", data=data,
        headers={"Content-Type": "application/json", "Accept": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req) as resp:
            print(resp.read().decode())
    except urllib.error.HTTPError as e:
        print(json.dumps({"ok": False, "error": e.read().decode("utf-8", errors="replace")}))
        sys.exit(1)


# ---------------------------------------------------------- present / absent


def cmd_assert_present(file_id, needle):
    c = rt.Client()
    db_ids, _ = n5.db_file_ids(c)
    b_ids = n5.boards_file_ids()
    cnt, _ = n5.search_count(needle)
    problems = []
    if file_id not in db_ids:
        problems.append("file id not in DB")
    if file_id not in b_ids:
        problems.append("file id not in /__api/vault/boards")
    if cnt < 1:
        problems.append(f"needle not searchable ({cnt} hits)")
    out = {
        "ok": not problems, "fileId": file_id, "needle": needle,
        "inDb": file_id in db_ids, "inBoards": file_id in b_ids,
        "searchHits": cnt, "problems": problems,
    }
    print(json.dumps(out, indent=2, sort_keys=True))
    sys.exit(0 if not problems else 1)


def cmd_assert_absent(file_id, needle):
    c = rt.Client()
    db_ids, _ = n5.db_file_ids(c)
    b_ids = n5.boards_file_ids()
    cnt, _ = n5.search_count(needle)
    problems = []
    if file_id in db_ids:
        problems.append("SPILL: file id found in DB")
    if file_id in b_ids:
        problems.append("SPILL: file id found in /__api/vault/boards")
    if cnt != 0:
        problems.append(f"SPILL: needle searchable here ({cnt} hits)")
    out = {
        "ok": not problems, "fileId": file_id, "needle": needle,
        "inDb": file_id in db_ids, "inBoards": file_id in b_ids,
        "searchHits": cnt, "problems": problems,
    }
    print(json.dumps(out, indent=2, sort_keys=True))
    sys.exit(0 if not problems else 1)


# ----------------------------------------------------------------------- main


def main():
    if len(sys.argv) < 2:
        die("usage: d5_docs_helper.py <cmd> ...", 64)
    cmd, args = sys.argv[1], sys.argv[2:]
    try:
        if cmd == "seed_docs":
            cmd_seed_docs(*args)
        elif cmd == "seed_needle_file":
            cmd_seed_needle_file(*args)
        elif cmd == "wait_manifest_id":
            cmd_wait_manifest_id(*args)
        elif cmd == "wait_reachable":
            cmd_wait_reachable(*args)
        elif cmd == "wait_file_window":
            cmd_wait_file_window(*args)
        elif cmd == "wait_window_title":
            cmd_wait_window_title(*args)
        elif cmd == "window_count":
            cmd_window_count(*args)
        elif cmd == "rename_file":
            cmd_rename_file(*args)
        elif cmd == "assert_present":
            cmd_assert_present(*args)
        elif cmd == "assert_absent":
            cmd_assert_absent(*args)
        else:
            die(f"unknown subcommand {cmd}", 64)
    except TypeError as e:
        die(f"bad arguments for {cmd}: {e}", 64)


if __name__ == "__main__":
    main()
