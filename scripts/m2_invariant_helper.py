#!/usr/bin/env python3
"""RPC + hashing helper for scripts/m2-invariant.sh (the M2 core-invariant test).

Reuses scripts/roundtrip.py (M0-verified normalization/hash functions and the
RPC client). Talks to the stack THROUGH THE PROXY (asset downloads require it).

Env (set by the shell script):
  PENPOT_BACKEND   proxy base url (http://localhost:<proxy-port>)
  PENPOT_TOKEN     access token from <data_dir>/credentials.json
  M2_DESIGNS_DIR   the sync root (PENPOT_LOCAL_DESIGNS_DIR)

Subcommands (all take <workdir> for state files):
  setup    create 2 projects / 3 files with shapes (+1 uploaded PNG referenced
           by an image-filled rect) -> writes expect.json
  check    one poll step of "is the daemon fully caught up on disk?"
           prints OK (and writes hashes.json) or WAIT <reason>
  hashes   recompute per-file semantic disk hashes -> stdout JSON
  verify   post-DB-wipe verification via RPC (projects, file ids, shapes,
           media blob) -> stdout JSON {"ok":bool,"sameIds":bool,...}
  dbstate  DB fingerprint for the no-op restart check -> stdout JSON
"""

import io
import json
import os
import struct
import sys
import uuid
import urllib.request
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import roundtrip as rt  # noqa: E402  (reads PENPOT_BACKEND/PENPOT_TOKEN env)

DESIGNS = os.environ["M2_DESIGNS_DIR"]
BASE = os.environ["PENPOT_BACKEND"]
MANIFEST = os.path.join(DESIGNS, ".penpot-sync.json")

RECTS = {  # file name -> [(shape name, x, color)]
    "alpha": [("M2 Rect Alpha", 100, "#B1B2B5")],
    "beta": [("M2 Rect Beta", 100, "#7048E8")],
    "gamma": [("M2 Rect Gamma", 100, "#12B886")],
}


def die(msg, code=2):
    print(f"HELPER-FAIL: {msg}", file=sys.stderr)
    sys.exit(code)


def tiny_png():
    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d))

    ihdr = chunk(b"IHDR", struct.pack(">IIBBBBB", 8, 8, 8, 0, 0, 0, 0))
    raw = b"".join(b"\x00" + bytes((x * 30) % 256 for x in range(8)) for _ in range(8))
    return b"\x89PNG\r\n\x1a\n" + ihdr + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")


def upload_media(client, file_id, name, png_bytes):
    """upload-file-media-object (multipart, kebab-case fields, image/png)."""
    boundary = "m2inv" + uuid.uuid4().hex
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
    buf.write(png_bytes + b"\r\n")
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


def add_rect(client, fid, page, name, x, color, fill_image=None):
    sid = str(uuid.uuid4())
    obj = rt.rect_shape(sid, name, x, 120, 200, 150, color)
    if fill_image is not None:
        obj["fills"] = [{"fillImage": fill_image}]
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
                    "obj": obj,
                }
            ],
        },
    )


def cmd_setup(workdir):
    c = rt.Client()
    profile = c.rpc("get-profile", {})
    team = profile["defaultTeamId"]
    pa = c.rpc("create-project", {"teamId": team, "name": "Client A"})
    pb = c.rpc("create-project", {"teamId": team, "name": "Client B"})

    expect = {"projects": {"Client A": pa["id"], "Client B": pb["id"]}, "files": {}}
    pages = {}
    for proj, fname in [(pa, "alpha"), (pa, "beta"), (pb, "gamma")]:
        created = c.rpc("create-file", {"name": fname, "projectId": proj["id"]})
        fid = created["id"]
        pages[fname] = created["data"]["pages"][0]
        for nm, x, col in RECTS[fname]:
            add_rect(c, fid, pages[fname], nm, x, col)
        expect["files"][fid] = {
            "name": fname,
            "projectName": proj["name"],
            "shapes": [nm for nm, _, _ in RECTS[fname]],
        }

    # Media coverage: upload a PNG into alpha and reference it from a shape
    # fill (unreferenced media may be pruned by export -> would make the media
    # check vacuous).
    alpha = next(fid for fid, i in expect["files"].items() if i["name"] == "alpha")
    png = tiny_png()
    with open(os.path.join(workdir, "tiny.png"), "wb") as fh:
        fh.write(png)
    media = upload_media(c, alpha, "tiny.png", png)
    fill_image = {
        "id": media["id"],
        "name": media["name"],
        "width": media["width"],
        "height": media["height"],
        "mtype": media["mtype"],
        "keepAspectRatio": False,
    }
    add_rect(c, alpha, pages["alpha"], "M2 Image Rect", 400, "#000000", fill_image)
    expect["files"][alpha]["shapes"].append("M2 Image Rect")
    expect["files"][alpha]["hasMedia"] = True

    for fid in expect["files"]:
        g = c.rpc("get-file", {"id": fid})
        expect["files"][fid]["revn"] = g["revn"]

    with open(os.path.join(workdir, "expect.json"), "w") as fh:
        json.dump(expect, fh, indent=2, sort_keys=True)
    print(json.dumps({"fileIds": sorted(expect["files"])}))


def cmd_check(workdir):
    expect = load_expect(workdir)
    manifest = load_manifest()
    if manifest is None:
        print("WAIT no manifest yet")
        return
    hashes = {}
    for fid, info in expect["files"].items():
        entry = manifest.get("files", {}).get(fid)
        if entry is None:
            print(f"WAIT {info['name']} not in manifest")
            return
        if entry["revn"] != info["revn"]:
            print(f"WAIT {info['name']} manifest revn {entry['revn']} != DB revn {info['revn']}")
            return
        abs_dir = os.path.join(DESIGNS, entry["path"])
        if not os.path.isdir(abs_dir):
            print(f"WAIT {entry['path']} not on disk yet")
            return
        h = semantic_dir_hash(abs_dir)
        if h != entry["lastSyncedHash"]:
            print(f"WAIT {info['name']} disk hash != lastSyncedHash (mid-swap?)")
            return
        hashes[fid] = {"hash": h, "path": entry["path"]}
        if info.get("hasMedia"):
            blobs = [r for r in rt.tree_files(abs_dir) if not r.endswith(".json")]
            if not blobs:
                die(f"{entry['path']} contains no non-JSON media blob — media not embedded in export")
    with open(os.path.join(workdir, "hashes.json"), "w") as fh:
        # print()-style trailing newline so `cmp` against cmd_hashes output works
        fh.write(json.dumps(hashes, indent=2, sort_keys=True) + "\n")
    print("OK")


def cmd_hashes(workdir):
    expect = load_expect(workdir)
    manifest = load_manifest()
    if manifest is None:
        die("no manifest on disk")
    out = {}
    for fid in expect["files"]:
        entry = manifest.get("files", {}).get(fid)
        if entry is None:
            die(f"file {fid} missing from manifest")
        out[fid] = {"hash": semantic_dir_hash(os.path.join(DESIGNS, entry["path"])), "path": entry["path"]}
    print(json.dumps(out, indent=2, sort_keys=True))


def iter_dicts(obj):
    if isinstance(obj, dict):
        yield obj
        for v in obj.values():
            yield from iter_dicts(v)
    elif isinstance(obj, list):
        for v in obj:
            yield from iter_dicts(v)


def cmd_verify(workdir):
    expect = load_expect(workdir)
    c = rt.Client()
    problems = []

    team = c.rpc("get-profile", {})["defaultTeamId"]
    projects = {p["name"]: p["id"] for p in c.rpc("get-projects", {"teamId": team})}
    for pname in expect["projects"]:
        if pname not in projects:
            problems.append(f"project '{pname}' missing after wipe")

    found = {}
    for pname in expect["projects"]:
        if pname in projects:
            for f in c.rpc("get-project-files", {"projectId": projects[pname]}):
                found[f["id"]] = {"name": f["name"], "project": pname}
    same_ids = set(found) == set(expect["files"])
    if not same_ids:
        problems.append(
            f"file-id mismatch: expected {sorted(expect['files'])}, found {sorted(found)}"
        )

    png_path = os.path.join(workdir, "tiny.png")
    original_png = open(png_path, "rb").read() if os.path.exists(png_path) else None
    for fid, info in expect["files"].items():
        if fid not in found:
            continue
        if found[fid]["project"] != info["projectName"]:
            problems.append(
                f"{info['name']} is in project '{found[fid]['project']}', expected '{info['projectName']}'"
            )
        g = c.rpc("get-file", {"id": fid})
        blob = json.dumps(g)
        for shape in info["shapes"]:
            if shape not in blob:
                problems.append(f"{info['name']}: shape '{shape}' missing from get-file")
        if info.get("hasMedia"):
            fill_ids = [
                d["fillImage"]["id"]
                for d in iter_dicts(g)
                if isinstance(d.get("fillImage"), dict) and "id" in d["fillImage"]
            ]
            if not fill_ids:
                problems.append(f"{info['name']}: no fillImage found after wipe")
            else:
                try:
                    fetched = c.download(f"{BASE}/assets/by-file-media-id/{fill_ids[0]}")
                    if not fetched.startswith(b"\x89PNG"):
                        problems.append(f"{info['name']}: media asset is not a PNG")
                    elif original_png is not None and fetched != original_png:
                        problems.append(f"{info['name']}: media bytes differ from the uploaded PNG")
                except Exception as e:  # noqa: BLE001
                    problems.append(f"{info['name']}: media asset fetch failed: {e}")

    print(json.dumps({"ok": not problems, "sameIds": same_ids, "problems": problems}, indent=2))
    sys.exit(0 if not problems else 1)


def cmd_dbstate(workdir):
    expect = load_expect(workdir)
    c = rt.Client()
    team = c.rpc("get-profile", {})["defaultTeamId"]
    projects = sorted(p["name"] for p in c.rpc("get-projects", {"teamId": team}))
    files = {}
    for fid in expect["files"]:
        g = c.rpc("get-file", {"id": fid})
        files[fid] = {"revn": g["revn"], "modifiedAt": g["modifiedAt"], "name": g["name"]}
    print(json.dumps({"projects": projects, "files": files}, indent=2, sort_keys=True))


def main():
    if len(sys.argv) != 3:
        die(f"usage: {sys.argv[0]} <setup|check|hashes|verify|dbstate> <workdir>", 64)
    cmd, workdir = sys.argv[1], sys.argv[2]
    fn = {
        "setup": cmd_setup,
        "check": cmd_check,
        "hashes": cmd_hashes,
        "verify": cmd_verify,
        "dbstate": cmd_dbstate,
    }.get(cmd)
    if fn is None:
        die(f"unknown subcommand {cmd}", 64)
    fn(workdir)


if __name__ == "__main__":
    main()
