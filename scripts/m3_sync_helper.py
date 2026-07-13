#!/usr/bin/env python3
"""RPC + hashing helper for scripts/m3-sync.sh (the M3 two-way-sync test).

Reuses scripts/roundtrip.py (M0-verified normalization/hash functions and the
RPC client). Talks to the stack THROUGH THE PROXY.

Env (set by the shell script):
  PENPOT_BACKEND   proxy base url (http://localhost:<proxy-port>)
  PENPOT_TOKEN     access token from <data_dir>/credentials.json
  M3_DESIGNS_DIR   the sync root (PENPOT_LOCAL_DESIGNS_DIR)

Subcommands (all take <workdir> for state files):
  setup         create 1 project ("M3 Sync") / 1 file ("homepage") with one
                rect -> writes expect.json
  check         one poll step of "is the daemon fully caught up on disk?"
                against LIVE DB facts (revn == DB revn, lastSyncedHash ==
                recomputed disk semantic hash). Prints "OK <hash> <relpath>"
                or "WAIT <reason>". This doubles as the manifest-consistency
                assertion.
  edit2         RPC edit: add a second rect ("M3 Rect Two") via update-file
  edit_db       RPC edit: add a third rect ("M3 Rect DB") via update-file
                (the DB side of the simultaneous-edit conflict test)
  disk_edit     ON-DISK edit: rename shape "M3 Rect One" ->
                "M3 Rect One (disk)" inside the live .penpot dir, re-dumping
                each touched .json with the exact normalization spec (sorted
                keys, 2-space indent, ensure_ascii=False, LF, trailing
                newline). Prints the new semantic disk hash.
  export_hash   fresh export-binfile (embed-assets like the daemon) ->
                normalize -> semantic tree hash -> stdout. Independent probe
                of what the DB currently holds.
  dir_hash P    semantic tree hash of directory P (abs, or rel to designs)
  wait_revert   poll get-file until "M3 Rect Two" is ABSENT and
                "M3 Rect One" PRESENT (exit criterion 1 convergence);
                prints elapsed seconds. Args: <timeout-s>
  shapes        prints JSON {"<shape name>": true/false} for the shape names
                given as extra args (presence in the get-file blob)
  dbstate       {"revn": .., "modifiedAt": ..} of the file -> stdout
"""

import json
import os
import sys
import time
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import roundtrip as rt  # noqa: E402  (reads PENPOT_BACKEND/PENPOT_TOKEN env)

DESIGNS = os.environ["M3_DESIGNS_DIR"]
MANIFEST = os.path.join(DESIGNS, ".penpot-sync.json")

PROJECT_NAME = "M3 Sync"
FILE_NAME = "homepage"
SHAPE_ONE = "M3 Rect One"
SHAPE_TWO = "M3 Rect Two"
SHAPE_DB = "M3 Rect DB"
SHAPE_ONE_DISK = "M3 Rect One (disk)"


def die(msg, code=2):
    print(f"HELPER-FAIL: {msg}", file=sys.stderr)
    sys.exit(code)


def semantic_dir_hash(abs_dir):
    return rt.tree_hash(rt.semantic_files(rt.tree_files(abs_dir)))


def load_expect(workdir):
    with open(os.path.join(workdir, "expect.json")) as fh:
        return json.load(fh)


def load_manifest():
    if not os.path.exists(MANIFEST):
        return None
    with open(MANIFEST) as fh:
        return json.load(fh)


def add_rect(client, fid, page, name, x, color):
    sid = str(uuid.uuid4())
    g = client.rpc("get-file", {"id": fid})
    client.rpc(
        "update-file",
        {
            "id": fid,
            "sessionId": str(uuid.uuid4()),
            "revn": g["revn"],
            "vern": g["vern"],
            "changes": [
                {
                    "type": "add-obj",
                    "id": sid,
                    "pageId": page,
                    "frameId": rt.ROOT_FRAME,
                    "parentId": rt.ROOT_FRAME,
                    "obj": rt.rect_shape(sid, name, x, 120, 200, 150, "#B1B2B5" if not color else color),
                }
            ],
        },
    )


def cmd_setup(workdir):
    c = rt.Client()
    team = c.rpc("get-profile", {})["defaultTeamId"]
    proj = c.rpc("create-project", {"teamId": team, "name": PROJECT_NAME})
    created = c.rpc("create-file", {"name": FILE_NAME, "projectId": proj["id"]})
    fid = created["id"]
    page = created["data"]["pages"][0]
    add_rect(c, fid, page, SHAPE_ONE, 100, "#B1B2B5")
    expect = {"projectId": proj["id"], "fileId": fid, "pageId": page}
    with open(os.path.join(workdir, "expect.json"), "w") as fh:
        json.dump(expect, fh, indent=2, sort_keys=True)
    print(json.dumps(expect))


def cmd_check(workdir):
    """Daemon fully caught up? Compares the manifest against LIVE DB facts
    and the recomputed disk hash — this IS the manifest-consistency check."""
    expect = load_expect(workdir)
    fid = expect["fileId"]
    manifest = load_manifest()
    if manifest is None:
        print("WAIT no manifest yet")
        return
    entry = manifest.get("files", {}).get(fid)
    if entry is None:
        print("WAIT file not in manifest")
        return
    c = rt.Client()
    g = c.rpc("get-file", {"id": fid})
    if entry["revn"] != g["revn"]:
        print(f"WAIT manifest revn {entry['revn']} != DB revn {g['revn']}")
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


def cmd_edit2(workdir):
    expect = load_expect(workdir)
    add_rect(rt.Client(), expect["fileId"], expect["pageId"], SHAPE_TWO, 400, "#7048E8")
    print("edited: added " + SHAPE_TWO)


def cmd_edit_db(workdir):
    expect = load_expect(workdir)
    add_rect(rt.Client(), expect["fileId"], expect["pageId"], SHAPE_DB, 700, "#12B886")
    print("edited: added " + SHAPE_DB)


def rename_in_obj(obj, old, new):
    """Recursively rename shape name values `old` -> `new`. Returns hit count."""
    hits = 0
    if isinstance(obj, dict):
        for k, v in obj.items():
            if k == "name" and v == old:
                obj[k] = new
                hits += 1
            else:
                hits += rename_in_obj(v, old, new)
    elif isinstance(obj, list):
        for v in obj:
            hits += rename_in_obj(v, old, new)
    return hits


def cmd_disk_edit(workdir):
    """The FS side of the simultaneous-edit test: rename SHAPE_ONE on disk,
    re-serializing with the exact normalization spec (so the change is a
    legitimate normalized tree, not a formatting diff)."""
    expect = load_expect(workdir)
    manifest = load_manifest()
    entry = (manifest or {}).get("files", {}).get(expect["fileId"])
    if entry is None:
        die("file not in manifest; cannot disk-edit")
    abs_dir = os.path.join(DESIGNS, entry["path"])
    total = 0
    for dirpath, _dirs, files in os.walk(abs_dir):
        for fn in files:
            if not fn.endswith(".json"):
                continue
            p = os.path.join(dirpath, fn)
            with open(p, "rb") as fh:
                data = json.loads(fh.read().decode("utf-8"))
            hits = rename_in_obj(data, SHAPE_ONE, SHAPE_ONE_DISK)
            if hits:
                out = json.dumps(data, sort_keys=True, indent=2, ensure_ascii=False) + "\n"
                with open(p, "wb") as fh:
                    fh.write(out.encode("utf-8"))
                total += hits
    if total == 0:
        die(f"shape {SHAPE_ONE!r} not found in any .json under {abs_dir}")
    print(semantic_dir_hash(abs_dir))


def fresh_export_semantic_hash(fid):
    """export-binfile with the daemon's parameters (embed-assets=true,
    include-libraries=false) -> unzip -> normalize -> semantic hash."""
    c = rt.Client()
    end = c.rpc_sse(
        "export-binfile",
        {"fileId": fid, "includeLibraries": False, "embedAssets": True},
    )
    zip_bytes = c.download(rt.parse_transit_uri(end))
    import tempfile, shutil  # noqa: E401

    tmp = tempfile.mkdtemp(prefix="m3-export-")
    try:
        rt.unzip_to(zip_bytes, os.path.join(tmp, "tree"))
        rt.normalize_tree(os.path.join(tmp, "tree"))
        return semantic_dir_hash(os.path.join(tmp, "tree"))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def cmd_export_hash(workdir):
    print(fresh_export_semantic_hash(load_expect(workdir)["fileId"]))


def cmd_dir_hash(workdir, path):
    abs_dir = path if os.path.isabs(path) else os.path.join(DESIGNS, path)
    if not os.path.isdir(abs_dir):
        die(f"not a directory: {abs_dir}")
    print(semantic_dir_hash(abs_dir))


def shape_present(blob, name):
    """Exact shape-name presence in a get-file JSON blob. Matches the fully
    quoted JSON string so 'M3 Rect One' does NOT match 'M3 Rect One (disk)'."""
    return json.dumps(name) in blob


def cmd_wait_revert(workdir, timeout_s):
    """Exit criterion 1 convergence probe: poll get-file until the v2-only
    shape is gone (and the v1 shape is back). Prints elapsed seconds."""
    expect = load_expect(workdir)
    fid = expect["fileId"]
    c = rt.Client()
    start = time.monotonic()
    deadline = start + float(timeout_s)
    while True:
        blob = json.dumps(c.rpc("get-file", {"id": fid}))
        if not shape_present(blob, SHAPE_TWO) and shape_present(blob, SHAPE_ONE):
            print(f"{time.monotonic() - start:.2f}")
            return
        if time.monotonic() > deadline:
            die(
                f"timed out after {timeout_s}s: "
                f"two_present={shape_present(blob, SHAPE_TWO)} "
                f"one_present={shape_present(blob, SHAPE_ONE)}"
            )
        time.sleep(0.2)


def cmd_shapes(workdir, *names):
    expect = load_expect(workdir)
    blob = json.dumps(rt.Client().rpc("get-file", {"id": expect["fileId"]}))
    print(json.dumps({n: shape_present(blob, n) for n in names}, sort_keys=True))


def cmd_dbstate(workdir):
    expect = load_expect(workdir)
    g = rt.Client().rpc("get-file", {"id": expect["fileId"]})
    print(json.dumps({"revn": g["revn"], "modifiedAt": g["modifiedAt"]}, sort_keys=True))


def main():
    if len(sys.argv) < 3:
        die(f"usage: {sys.argv[0]} <subcommand> <workdir> [args...]", 64)
    cmd, workdir, args = sys.argv[1], sys.argv[2], sys.argv[3:]
    fn = {
        "setup": cmd_setup,
        "check": cmd_check,
        "edit2": cmd_edit2,
        "edit_db": cmd_edit_db,
        "disk_edit": cmd_disk_edit,
        "export_hash": cmd_export_hash,
        "dir_hash": cmd_dir_hash,
        "wait_revert": cmd_wait_revert,
        "shapes": cmd_shapes,
        "dbstate": cmd_dbstate,
    }.get(cmd)
    if fn is None:
        die(f"unknown subcommand {cmd}", 64)
    fn(workdir, *args)


if __name__ == "__main__":
    main()
