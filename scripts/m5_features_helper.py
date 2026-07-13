#!/usr/bin/env python3
"""RPC + manifest helper for scripts/m5-features.sh (the M5 feature test).

Reuses scripts/roundtrip.py (M0-verified RPC client + normalization/hash).
Talks to the stack THROUGH THE PROXY.

Env (set by the shell script):
  PENPOT_BACKEND   proxy base url (http://localhost:<proxy-port>)
  PENPOT_TOKEN     access token from <data_dir>/credentials.json
  M5_DESIGNS_DIR   the sync root (PENPOT_LOCAL_DESIGNS_DIR)

Subcommands (all take <workdir> for state files; file keys are the logical
names "alpha"/"beta" mapped to ids via expect.json):
  seed             create project "ProjectA" with file "alpha" (boards
                   "Cover"+"Detail") and file "beta" (board "Solo")
                   -> writes expect.json
  check K          m3-style settle probe for file K: manifest revn == DB revn
                   and disk hash == lastSyncedHash -> "OK <hash> <relpath>"
                   or "WAIT <reason>"
  exports_check K  "OK <exports-relpath>" when <name>.exports/ exists, its
                   .exports-state.json renderedFromHash == the manifest's
                   lastSyncedHash, and every seeded board has .svg + .png
  edit_alpha       RPC edit: add a rect inside alpha's "Cover" board
  info K           {"id","name","revn","modifiedAt"} of file K (get-file)
  manifest_entry K {"path","projectId","projectName"} of file K's manifest row
  project_files P  JSON list of {"id","name"} in project P (get-project-files)
  wait_name K NAME TIMEOUT   poll get-file until file K's name == NAME;
                   prints elapsed seconds
"""

import json
import os
import sys
import time
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import roundtrip as rt  # noqa: E402  (reads PENPOT_BACKEND/PENPOT_TOKEN env)

DESIGNS = os.environ["M5_DESIGNS_DIR"]
MANIFEST = os.path.join(DESIGNS, ".penpot-sync.json")

PROJECT_NAME = "ProjectA"
IDENT = {"a": 1.0, "b": 0.0, "c": 0.0, "d": 1.0, "e": 0.0, "f": 0.0}
BOARDS = {"alpha": ["Cover", "Detail"], "beta": ["Solo"]}


def die(msg, code=2):
    print(f"HELPER-FAIL: {msg}", file=sys.stderr)
    sys.exit(code)


def load_expect(workdir):
    with open(os.path.join(workdir, "expect.json")) as fh:
        return json.load(fh)


def load_manifest():
    if not os.path.exists(MANIFEST):
        return None
    with open(MANIFEST) as fh:
        return json.load(fh)


def manifest_entry(expect, key):
    manifest = load_manifest()
    if manifest is None:
        return None
    return manifest.get("files", {}).get(expect["files"][key]["fileId"])


def frame_obj(fid, name, x, y, w, h):
    return {
        "id": fid, "type": "frame", "name": name,
        "x": x, "y": y, "width": w, "height": h, "rotation": 0,
        "selrect": {"x": x, "y": y, "width": w, "height": h,
                    "x1": x, "y1": y, "x2": x + w, "y2": y + h},
        "points": [{"x": x, "y": y}, {"x": x + w, "y": y},
                   {"x": x + w, "y": y + h}, {"x": x, "y": y + h}],
        "transform": IDENT, "transformInverse": IDENT,
        "parentId": rt.ROOT_FRAME, "frameId": rt.ROOT_FRAME,
        "fills": [{"fillColor": "#FFFFFF", "fillOpacity": 1}],
        "strokes": [], "shapes": [],
    }


def cmd_seed(workdir):
    c = rt.Client()
    team = c.rpc("get-profile", {})["defaultTeamId"]
    proj = c.rpc("create-project", {"teamId": team, "name": PROJECT_NAME})
    out = {"projectId": proj["id"], "files": {}}
    for key, boards in BOARDS.items():
        created = c.rpc("create-file", {"name": key, "projectId": proj["id"]})
        fid, page = created["id"], created["data"]["pages"][0]
        changes, binfo = [], []
        for i, bname in enumerate(boards):
            frid, rid = str(uuid.uuid4()), str(uuid.uuid4())
            changes.append({
                "type": "add-obj", "id": frid, "pageId": page,
                "frameId": rt.ROOT_FRAME, "parentId": rt.ROOT_FRAME,
                "obj": frame_obj(frid, bname, i * 900, 0, 800, 600),
            })
            rect = rt.rect_shape(rid, f"{bname} rect", i * 900 + 100, 100,
                                 250, 180, "#7048E8")
            rect.update({"parentId": frid, "frameId": frid})
            changes.append({
                "type": "add-obj", "id": rid, "pageId": page,
                "frameId": frid, "parentId": frid, "obj": rect,
            })
            binfo.append({"frameId": frid, "name": bname})
        c.rpc("update-file", {
            "id": fid, "sessionId": str(uuid.uuid4()),
            "revn": created["revn"], "vern": created["vern"],
            "changes": changes,
        })
        out["files"][key] = {"fileId": fid, "pageId": page, "boards": binfo}
    with open(os.path.join(workdir, "expect.json"), "w") as fh:
        json.dump(out, fh, indent=2, sort_keys=True)
    print(json.dumps(out))


def semantic_dir_hash(abs_dir):
    return rt.tree_hash(rt.semantic_files(rt.tree_files(abs_dir)))


def cmd_check(workdir, key):
    expect = load_expect(workdir)
    entry = manifest_entry(expect, key)
    if entry is None:
        print("WAIT file not in manifest")
        return
    c = rt.Client()
    g = c.rpc("get-file", {"id": expect["files"][key]["fileId"]})
    if entry["revn"] != g["revn"]:
        print(f"WAIT manifest revn {entry['revn']} != DB revn {g['revn']}")
        return
    # The advisory pair must have caught up too: a stale dbModifiedAt means
    # a DB->FS export (e.g. the post-rename name refresh) is still pending.
    if entry.get("dbModifiedAt") and entry["dbModifiedAt"] != g["modifiedAt"]:
        print("WAIT manifest dbModifiedAt behind the DB (export pending)")
        return
    abs_dir = os.path.join(DESIGNS, entry["path"])
    if not os.path.isdir(abs_dir):
        print(f"WAIT {entry['path']} not on disk yet")
        return
    h = semantic_dir_hash(abs_dir)
    if h != entry["lastSyncedHash"]:
        print("WAIT disk hash != lastSyncedHash (mid-swap?)")
        return
    print(f"OK {h} {entry['path']}")


def cmd_exports_check(workdir, key):
    expect = load_expect(workdir)
    entry = manifest_entry(expect, key)
    if entry is None:
        print("WAIT file not in manifest")
        return
    rel = entry["path"]
    rel_exports = rel[: -len(".penpot")] + ".exports" if rel.endswith(".penpot") else rel + ".exports"
    exports_dir = os.path.join(DESIGNS, rel_exports)
    state_path = os.path.join(exports_dir, ".exports-state.json")
    if not os.path.isfile(state_path):
        print(f"WAIT no exports state at {rel_exports}")
        return
    with open(state_path) as fh:
        state = json.load(fh)
    if state.get("renderedFromHash") != entry["lastSyncedHash"]:
        print("WAIT exports state hash behind the manifest")
        return
    for b in expect["files"][key]["boards"]:
        for ext in ("svg", "png"):
            p = os.path.join(exports_dir, f"{b['name']}.{ext}")
            if not os.path.isfile(p) or os.path.getsize(p) == 0:
                print(f"WAIT missing/empty {b['name']}.{ext}")
                return
    print(f"OK {rel_exports}")


def cmd_edit_alpha(workdir):
    expect = load_expect(workdir)
    alpha = expect["files"]["alpha"]
    cover = next(b for b in alpha["boards"] if b["name"] == "Cover")
    c = rt.Client()
    g = c.rpc("get-file", {"id": alpha["fileId"]})
    rid = str(uuid.uuid4())
    rect = rt.rect_shape(rid, "M5 Edit Rect", 400, 300, 120, 90, "#12B886")
    rect.update({"parentId": cover["frameId"], "frameId": cover["frameId"]})
    c.rpc("update-file", {
        "id": alpha["fileId"], "sessionId": str(uuid.uuid4()),
        "revn": g["revn"], "vern": g["vern"],
        "changes": [{
            "type": "add-obj", "id": rid, "pageId": alpha["pageId"],
            "frameId": cover["frameId"], "parentId": cover["frameId"],
            "obj": rect,
        }],
    })
    print("edited: added M5 Edit Rect to alpha/Cover")


def cmd_info(workdir, key):
    expect = load_expect(workdir)
    g = rt.Client().rpc("get-file", {"id": expect["files"][key]["fileId"]})
    print(json.dumps({
        "id": g["id"], "name": g["name"], "revn": g["revn"],
        "modifiedAt": g["modifiedAt"],
    }, sort_keys=True))


def cmd_manifest_entry(workdir, key):
    expect = load_expect(workdir)
    entry = manifest_entry(expect, key)
    if entry is None:
        die("file not in manifest")
    print(json.dumps({
        "path": entry["path"], "projectId": entry["projectId"],
        "projectName": entry["projectName"],
    }, sort_keys=True))


def cmd_project_files(workdir, project_id):
    files = rt.Client().rpc("get-project-files", {"projectId": project_id})
    print(json.dumps([{"id": f["id"], "name": f["name"]} for f in files],
                     sort_keys=True))


def cmd_wait_name(workdir, key, name, timeout_s):
    expect = load_expect(workdir)
    fid = expect["files"][key]["fileId"]
    c = rt.Client()
    start = time.monotonic()
    deadline = start + float(timeout_s)
    last = None
    while True:
        last = c.rpc("get-file", {"id": fid})["name"]
        if last == name:
            print(f"{time.monotonic() - start:.2f}")
            return
        if time.monotonic() > deadline:
            die(f"timed out after {timeout_s}s: DB name is {last!r}, want {name!r}")
        time.sleep(0.2)


def main():
    if len(sys.argv) < 3:
        die(f"usage: {sys.argv[0]} <subcommand> <workdir> [args...]", 64)
    cmd, workdir, args = sys.argv[1], sys.argv[2], sys.argv[3:]
    fn = {
        "seed": cmd_seed,
        "check": cmd_check,
        "exports_check": cmd_exports_check,
        "edit_alpha": cmd_edit_alpha,
        "info": cmd_info,
        "manifest_entry": cmd_manifest_entry,
        "project_files": cmd_project_files,
        "wait_name": cmd_wait_name,
    }.get(cmd)
    if fn is None:
        die(f"unknown subcommand {cmd}", 64)
    fn(workdir, *args)


if __name__ == "__main__":
    main()
