#!/usr/bin/env python3
"""RPC + HTTP helper for scripts/n5-vaults.sh (the N5 vaults / zero-spill gate).

Reuses scripts/roundtrip.py (M0-verified RPC client). Talks to the stack
THROUGH THE PROXY. Each vault is a small tree: two projects with names that are
IDENTICAL across vaults (overlapping project names) and one file each carrying a
distinct, searchable needle token — so a spill (a file of the other vault
surfacing in this one) is detectable by file id in the DB, in /__api/vault/boards
and in /__api/vault/search. One file per vault ALSO embeds a distinct uploaded
raster (a PNG referenced by an image fill) so media hygiene is provable: after a
switch the other vault's blob must NOT be served (its DB rows are wiped) and its
bytes must NOT linger under <data>/assets (that dir is wiped on switch and
re-materialized from the .penpot on reconcile — see docs/milestones/n5.md).

Env (set by the shell script):
  PENPOT_BACKEND   proxy base url (http://localhost:<proxy-port>)
  PENPOT_TOKEN     access token from <data_dir>/credentials.json (re-read after
                   every switch — provisioning mints a fresh token)

Subcommands:
  seed <workdir> <label> <needle>
      Create 2 overlapping-named projects ("Shared", "Studio") each with one
      file carrying <needle> in a board's text layer. Writes
      <workdir>/expect-<label>.json and prints the file ids.
  wait_synced <workdir> <label> <vault_root> <timeout>
      Poll until every file of <label> is on disk under <vault_root> with the
      manifest caught up (revn + lastSyncedHash). Prints OK or a WAIT reason.
  assert_state <workdir> <this_label> <other_label>
      THE zero-spill assertion, run after a switch: the current DB / boards /
      search contain exactly <this_label>'s files, keep their ORIGINAL ids, and
      contain NONE of <other_label>'s files or needle. Prints JSON, exits
      non-zero on any spill / id mismatch.
  media_assert <workdir> <this_label> <other_label>
      THE media zero-spill + survival assertion, run after a switch: this
      vault's embedded image is served with its EXACT uploaded bytes
      (survival / re-materialized from the .penpot), and the other vault's
      media id is NOT served (its DB rows are gone). Prints JSON, exits
      non-zero on any media spill or a lost blob.
  png_path <workdir> <label>
      Print the absolute path of the PNG uploaded into <label>'s vault (so the
      shell can byte-scan <data>/assets for it).
  tree_hash <vault_root>
      Aggregate sha256 over the RAW bytes of every file inside a `*.penpot`
      dir (the user's design bytes; excludes the manifest, .exports, the
      .penpot-vault marker and conflict copies). Prints the hex digest — the
      shell compares it before/after to prove the user disk is byte-untouched.
"""

import hashlib
import io
import json
import os
import struct
import sys
import time
import urllib.parse
import urllib.request
import uuid
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import roundtrip as rt  # noqa: E402  (reads PENPOT_BACKEND/PENPOT_TOKEN env)

BASE = os.environ["PENPOT_BACKEND"]
MANIFEST_NAME = ".penpot-sync.json"

# Overlapping project names — IDENTICAL across vault A and vault B by design.
PROJECT_NAMES = ["Shared", "Studio"]
IDENT = {"a": 1.0, "b": 0.0, "c": 0.0, "d": 1.0, "e": 0.0, "f": 0.0}


def die(msg, code=2):
    print(f"HELPER-FAIL: {msg}", file=sys.stderr)
    sys.exit(code)


# --------------------------------------------------------------------- shapes


def geometry(x, y, w, h):
    return {
        "x": x, "y": y, "width": w, "height": h, "rotation": 0,
        "selrect": {"x": x, "y": y, "width": w, "height": h,
                    "x1": x, "y1": y, "x2": x + w, "y2": y + h},
        "points": [{"x": x, "y": y}, {"x": x + w, "y": y},
                   {"x": x + w, "y": y + h}, {"x": x, "y": y + h}],
        "transform": IDENT, "transformInverse": IDENT,
    }


def frame_obj(fid, name, x, y, w, h):
    obj = {"id": fid, "type": "frame", "name": name,
           "parentId": rt.ROOT_FRAME, "frameId": rt.ROOT_FRAME,
           "fills": [{"fillColor": "#FFFFFF", "fillOpacity": 1}],
           "strokes": [], "shapes": []}
    obj.update(geometry(x, y, w, h))
    return obj


def text_content(text):
    node = {
        "text": text,
        "fontId": "sourcesanspro", "fontFamily": "sourcesanspro",
        "fontSize": "14", "fontStyle": "normal", "fontWeight": "400",
        "fontVariantId": "regular", "textDecoration": "none",
        "fills": [{"fillColor": "#000000", "fillOpacity": 1}],
    }
    para = {
        "type": "paragraph", "children": [node],
        "fontId": "sourcesanspro", "fontFamily": "sourcesanspro",
        "fontSize": "14", "fontStyle": "normal", "fontWeight": "400",
        "fontVariantId": "regular", "textDecoration": "none",
        "textAlign": "left",
        "fills": [{"fillColor": "#000000", "fillOpacity": 1}],
    }
    return {"type": "root", "children": [
        {"type": "paragraph-set", "children": [para]}]}


def text_obj(sid, name, parent, x, y, text):
    obj = {"id": sid, "type": "text", "name": name,
           "parentId": parent, "frameId": parent,
           "content": text_content(text), "growType": "auto-width",
           "strokes": [], "fills": []}
    obj.update(geometry(x, y, 300, 40))
    return obj


# ------------------------------------------------------------------ raster media


def tiny_png(seed):
    """A tiny 8x8 PNG whose pixel content varies with <seed>, so vault A and
    vault B upload DISTINCT blobs (different bytes -> different storage object,
    no content dedup between them)."""
    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d))

    ihdr = chunk(b"IHDR", struct.pack(">IIBBBBB", 8, 8, 8, 0, 0, 0, 0))
    raw = b"".join(
        b"\x00" + bytes(((x * 30) + seed * 7) % 256 for x in range(8)) for _ in range(8)
    )
    return b"\x89PNG\r\n\x1a\n" + ihdr + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")


def upload_media(file_id, name, png):
    """upload-file-media-object (multipart, kebab-case fields, image/png)."""
    boundary = "n5media" + uuid.uuid4().hex
    buf = io.BytesIO()
    for n, v in [("file-id", file_id), ("is-local", "true"), ("name", name)]:
        buf.write(f"--{boundary}\r\n".encode())
        buf.write(f'Content-Disposition: form-data; name="{n}"\r\n\r\n{v}\r\n'.encode())
    buf.write(f"--{boundary}\r\n".encode())
    buf.write(
        (
            f'Content-Disposition: form-data; name="content"; filename="{name}"\r\n'
            "Content-Type: image/png\r\n\r\n"
        ).encode()
    )
    buf.write(png + b"\r\n")
    buf.write(f"--{boundary}--\r\n".encode())
    headers = {
        "Content-Type": f"multipart/form-data; boundary={boundary}",
        "Accept": "application/json",
        "Authorization": "Token " + os.environ["PENPOT_TOKEN"],
    }
    req = urllib.request.Request(
        f"{BASE}/api/rpc/command/upload-file-media-object", data=buf.getvalue(), headers=headers
    )
    with urllib.request.urlopen(req) as resp:
        return json.loads(resp.read())


def png_path(workdir, label):
    return os.path.join(workdir, f"img-{label}.png")


def iter_dicts(obj):
    if isinstance(obj, dict):
        yield obj
        for v in obj.values():
            yield from iter_dicts(v)
    elif isinstance(obj, list):
        for v in obj:
            yield from iter_dicts(v)


def live_fill_id(client, file_id):
    """The image-fill media id currently in the DB for <file_id> (resolved from
    get-file so it reflects the re-imported file, not the seed-time value)."""
    g = client.rpc("get-file", {"id": file_id})
    ids = [
        d["fillImage"]["id"]
        for d in iter_dicts(g)
        if isinstance(d.get("fillImage"), dict) and "id" in d["fillImage"]
    ]
    return ids[0] if ids else None


def asset_bytes(fill_media_id):
    """GET /assets/by-file-media-id/<id> through the proxy. Returns (ok, body):
    ok=True + bytes when served, ok=False + b"" on any non-200 / error."""
    url = f"{BASE}/assets/by-file-media-id/{fill_media_id}"
    try:
        with urllib.request.urlopen(url) as resp:
            if resp.status != 200:
                return False, b""
            return True, resp.read()
    except Exception:  # noqa: BLE001  (404 for a wiped object is expected)
        return False, b""


# --------------------------------------------------------------------- state


def expect_path(workdir, label):
    return os.path.join(workdir, f"expect-{label}.json")


def load_expect(workdir, label):
    with open(expect_path(workdir, label)) as fh:
        return json.load(fh)


def load_manifest(vault_root):
    path = os.path.join(vault_root, MANIFEST_NAME)
    if not os.path.exists(path):
        return None
    with open(path) as fh:
        return json.load(fh)


def semantic_dir_hash(abs_dir):
    return rt.tree_hash(rt.semantic_files(rt.tree_files(abs_dir)))


# ----------------------------------------------------------------------- seed


def cmd_seed(workdir, label, needle):
    c = rt.Client()
    team = c.rpc("get-profile", {})["defaultTeamId"]
    files = {}
    media = None
    # Distinct blob per vault so the two vaults never share a storage object.
    png = tiny_png(1 if label == "A" else 2)
    with open(png_path(workdir, label), "wb") as fh:
        fh.write(png)
    for i, pname in enumerate(PROJECT_NAMES):
        proj = c.rpc("create-project", {"teamId": team, "name": pname})
        fname = f"design-{i}"
        created = c.rpc("create-file", {"name": fname, "projectId": proj["id"]})
        fid, page = created["id"], created["data"]["pages"][0]
        board_id, text_id = str(uuid.uuid4()), str(uuid.uuid4())
        board_name = f"Board {label}-{i}"
        # A per-file needle token so the two files of a vault are distinct too.
        file_needle = f"{needle}{i}"
        changes = [
            {"type": "add-obj", "id": board_id, "pageId": page,
             "frameId": rt.ROOT_FRAME, "parentId": rt.ROOT_FRAME,
             "obj": frame_obj(board_id, board_name, i * 900, 0, 800, 600)},
            {"type": "add-obj", "id": text_id, "pageId": page,
             "frameId": board_id, "parentId": board_id,
             "obj": text_obj(text_id, f"needle-{label}-{i}", board_id,
                             i * 900 + 40, 40, file_needle)},
        ]
        # Embed the raster in the first file: upload the PNG, reference it from
        # an image-filled rect so export-binfile carries the blob (unreferenced
        # media may be pruned, which would make the media check vacuous).
        if i == 0:
            up = upload_media(fid, f"{needle}.png", png)
            img_id = str(uuid.uuid4())
            img = {"id": img_id, "type": "rect", "name": f"Image {label}",
                   "parentId": rt.ROOT_FRAME, "frameId": rt.ROOT_FRAME,
                   "strokes": [],
                   "fills": [{"fillOpacity": 1, "fillImage": {
                       "id": up["id"], "name": up["name"], "width": up["width"],
                       "height": up["height"], "mtype": up["mtype"],
                       "keepAspectRatio": False}}]}
            img.update(geometry(40, 320, 300, 200))
            changes.append(
                {"type": "add-obj", "id": img_id, "pageId": page,
                 "frameId": rt.ROOT_FRAME, "parentId": rt.ROOT_FRAME, "obj": img})
            media = {"fileId": fid, "fillMediaId": up["id"],
                     "pngPath": png_path(workdir, label)}
        c.rpc("update-file", {"id": fid, "sessionId": str(uuid.uuid4()),
                              "revn": created["revn"], "vern": created["vern"],
                              "changes": changes})
        g = c.rpc("get-file", {"id": fid})
        files[fid] = {
            "name": fname, "projectName": pname, "projectId": proj["id"],
            "boardId": board_id, "pageId": page, "needle": file_needle,
            "revn": g["revn"],
        }
    expect = {
        "label": label, "teamId": team, "needle": needle,
        "projectNames": PROJECT_NAMES, "files": files, "media": media,
    }
    with open(expect_path(workdir, label), "w") as fh:
        json.dump(expect, fh, indent=2, sort_keys=True)
    print(json.dumps({"label": label, "fileIds": sorted(files),
                      "mediaFillId": media["fillMediaId"]}))


# --------------------------------------------------------------- wait_synced


def cmd_wait_synced(workdir, label, vault_root, timeout):
    expect = load_expect(workdir, label)
    deadline = time.time() + float(timeout)
    last = "?"
    while time.time() < deadline:
        last = _synced_once(expect, vault_root)
        if last == "OK":
            print("OK")
            return
        time.sleep(2)
    die(f"wait_synced({label}) timed out: {last}", 1)


def _synced_once(expect, vault_root):
    manifest = load_manifest(vault_root)
    if manifest is None:
        return "no manifest yet"
    mfiles = manifest.get("files", {})
    for fid, info in expect["files"].items():
        entry = mfiles.get(fid)
        if entry is None:
            return f"{info['name']} not in manifest"
        if entry["revn"] != info["revn"]:
            return f"{info['name']} manifest revn {entry['revn']} != DB revn {info['revn']}"
        abs_dir = os.path.join(vault_root, entry["path"])
        if not os.path.isdir(abs_dir):
            return f"{entry['path']} not on disk"
        if semantic_dir_hash(abs_dir) != entry["lastSyncedHash"]:
            return f"{info['name']} disk hash != lastSyncedHash (mid-swap?)"
    return "OK"


# --------------------------------------------------------------- assert_state


def http_json(path):
    url = f"{BASE}{path}"
    with urllib.request.urlopen(url) as resp:
        return json.loads(resp.read())


def db_file_ids(client):
    """Every file id currently in the DB, across every project of the team."""
    team = client.rpc("get-profile", {})["defaultTeamId"]
    ids = set()
    projects = {}
    for p in client.rpc("get-projects", {"teamId": team}):
        projects[p["id"]] = p["name"]
        for f in client.rpc("get-project-files", {"projectId": p["id"]}):
            ids.add(f["id"])
    return ids, projects


def search_count(needle):
    body = http_json(f"/__api/vault/search?q={urllib.parse.quote(needle)}&limit=50")
    return body.get("count", 0), body.get("hits", [])


def boards_file_ids():
    # /__api/vault/boards → {count, projects, boards:[{fileId, ...}]}
    body = http_json("/__api/vault/boards")
    ids = set()
    for card in body.get("boards", []):
        fid = card.get("fileId")
        if fid:
            ids.add(fid)
    return ids


def cmd_wait_present(workdir, label, timeout):
    """After a switch to a vault with EXISTING files, startup reconciliation
    (re-import from disk) and the index rebuild run ASYNC after boot() returns.
    Poll until this vault's files are back in the DB (original ids) AND its
    needles are searchable, so the strict assert_state runs on a settled stack."""
    expect = load_expect(workdir, label)
    ids = set(expect["files"])
    needles = [info["needle"] for info in expect["files"].values()]
    deadline = time.time() + float(timeout)
    last = "?"
    while time.time() < deadline:
        try:
            db_ids, _ = db_file_ids(rt.Client())
        except Exception as e:  # noqa: BLE001  (backend may still be settling)
            last = f"rpc: {e}"
            time.sleep(2)
            continue
        if ids <= db_ids:
            if all(search_count(n)[0] >= 1 for n in needles):
                print("OK")
                return
            last = "index not caught up"
        else:
            last = f"db missing {sorted(ids - db_ids)}"
        time.sleep(2)
    die(f"wait_present({label}) timed out: {last}", 1)


def cmd_assert_state(workdir, this_label, other_label):
    this = load_expect(workdir, this_label)
    other = load_expect(workdir, other_label)
    this_ids = set(this["files"])
    other_ids = set(other["files"])
    problems = []

    c = rt.Client()

    # 1. DB: exactly this vault's files, keeping their ORIGINAL ids; no spill.
    db_ids, _projects = db_file_ids(c)
    same_ids = db_ids == this_ids
    if not same_ids:
        missing = sorted(this_ids - db_ids)
        extra = sorted(db_ids - this_ids)
        problems.append(f"DB file-id mismatch: missing={missing} extra={extra}")
    db_spill = sorted(db_ids & other_ids)
    if db_spill:
        problems.append(f"SPILL: other vault's files in DB: {db_spill}")

    # 2. Lighttable (/__api/vault/boards): no other-vault file id; this present.
    try:
        b_ids = boards_file_ids()
        board_spill = sorted(b_ids & other_ids)
        if board_spill:
            problems.append(f"SPILL: other vault's files in /boards: {board_spill}")
        this_in_boards = this_ids & b_ids
        if not this_in_boards:
            problems.append("this vault's files absent from /boards")
    except Exception as e:  # noqa: BLE001
        problems.append(f"/boards query failed: {e}")

    # 3. Index (/__api/vault/search): other vault's needles gone; this present.
    for fid, info in other["files"].items():
        cnt, _ = search_count(info["needle"])
        if cnt != 0:
            problems.append(f"SPILL: other vault needle '{info['needle']}' has {cnt} search hit(s)")
    for fid, info in this["files"].items():
        cnt, _ = search_count(info["needle"])
        if cnt < 1:
            problems.append(f"this vault needle '{info['needle']}' not searchable ({cnt} hits)")

    out = {
        "ok": not problems,
        "thisLabel": this_label,
        "otherLabel": other_label,
        "sameIds": same_ids,
        "dbFileCount": len(db_ids),
        "problems": problems,
    }
    print(json.dumps(out, indent=2, sort_keys=True))
    sys.exit(0 if not problems else 1)


# --------------------------------------------------------------- media_assert


def cmd_media_assert(workdir, this_label, other_label):
    """After a switch: this vault's embedded image is served with its EXACT
    uploaded bytes (survival — re-materialized from the .penpot), and the other
    vault's media id is NOT served (zero media spill — its DB rows are wiped)."""
    this = load_expect(workdir, this_label)
    other = load_expect(workdir, other_label)
    problems = []
    c = rt.Client()

    this_media = this.get("media")
    other_media = other.get("media")
    if this_media is None or other_media is None:
        die("expect file missing media block (re-seed needed)")

    # 1. SURVIVAL: this vault's image is served with the exact uploaded bytes.
    fill_id = live_fill_id(c, this_media["fileId"])
    served_this = False
    bytes_match = False
    if fill_id is None:
        problems.append(f"{this_label}: no fillImage in DB after switch")
    else:
        ok, body = asset_bytes(fill_id)
        served_this = ok
        original = open(this_media["pngPath"], "rb").read()
        bytes_match = ok and body == original
        if not ok:
            problems.append(f"{this_label}: own image not served (media lost on reconcile)")
        elif not bytes_match:
            problems.append(f"{this_label}: served image bytes differ from the uploaded PNG")

    # 2. MEDIA ZERO-SPILL: the other vault's media id is not served here.
    other_served, _ = asset_bytes(other_media["fillMediaId"])
    if other_served:
        problems.append(
            f"MEDIA SPILL: other vault's media {other_media['fillMediaId']} is served under {this_label}")

    out = {
        "ok": not problems,
        "thisLabel": this_label,
        "otherLabel": other_label,
        "thisImageServed": served_this,
        "thisBytesMatch": bytes_match,
        "otherImageServed": other_served,
        "problems": problems,
    }
    print(json.dumps(out, indent=2, sort_keys=True))
    sys.exit(0 if not problems else 1)


def cmd_png_path(workdir, label):
    print(load_expect(workdir, label)["media"]["pngPath"])


# ------------------------------------------------------------------ tree_hash


def cmd_tree_hash(vault_root):
    """Aggregate sha256 over the RAW bytes of every file inside a real
    (non-conflict) `*.penpot` directory — the user's design bytes."""
    records = []
    for dirpath, dirnames, filenames in os.walk(vault_root):
        # Skip dot-directories entirely (.exports, .penpot-vault, .git, …).
        dirnames[:] = [d for d in dirnames if not d.startswith(".")]
        # Only descend / hash inside real .penpot dirs (not conflict copies).
        rel_top = os.path.relpath(dirpath, vault_root)
        parts = [] if rel_top == "." else rel_top.split(os.sep)
        penpot_seg = next((p for p in parts if p.endswith(".penpot")), None)
        if penpot_seg is None:
            continue
        if ".conflict-" in penpot_seg:
            continue
        for name in filenames:
            abs_path = os.path.join(dirpath, name)
            rel = os.path.relpath(abs_path, vault_root)
            with open(abs_path, "rb") as fh:
                digest = hashlib.sha256(fh.read()).hexdigest()
            records.append(f"{rel}\0{digest}")
    agg = hashlib.sha256("\n".join(sorted(records)).encode()).hexdigest()
    print(agg)


# ----------------------------------------------------------------------- main


def main():
    if len(sys.argv) < 2:
        die("usage: n5_vaults_helper.py <cmd> ...", 64)
    cmd = sys.argv[1]
    if cmd == "seed":
        cmd_seed(sys.argv[2], sys.argv[3], sys.argv[4])
    elif cmd == "wait_synced":
        cmd_wait_synced(sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5])
    elif cmd == "wait_present":
        cmd_wait_present(sys.argv[2], sys.argv[3], sys.argv[4])
    elif cmd == "assert_state":
        cmd_assert_state(sys.argv[2], sys.argv[3], sys.argv[4])
    elif cmd == "media_assert":
        cmd_media_assert(sys.argv[2], sys.argv[3], sys.argv[4])
    elif cmd == "png_path":
        cmd_png_path(sys.argv[2], sys.argv[3])
    elif cmd == "tree_hash":
        cmd_tree_hash(sys.argv[2])
    else:
        die(f"unknown subcommand {cmd}", 64)


if __name__ == "__main__":
    main()
