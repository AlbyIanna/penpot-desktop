#!/usr/bin/env python3
"""RPC + HTTP helper for scripts/n1-index.sh (the N1 vault-index gate) and
THE shared torture-fixture seeder the milestone ladder reuses (N3/N5).

Reuses scripts/roundtrip.py (M0-verified RPC client). Talks to the stack
THROUGH THE PROXY (search endpoints only exist there).

Env (set by the shell script):
  PENPOT_BACKEND   proxy base url (http://localhost:<proxy-port>)
  PENPOT_TOKEN     access token from <data_dir>/credentials.json
  N1_DESIGNS_DIR   the sync root (PENPOT_LOCAL_DESIGNS_DIR)
  N1_FILES         torture fixture size (default 100 files)
  N1_BOARDS        boards per file (default 10)

Subcommands (all take <workdir> for state files):
  seed              create the torture fixture: N1_FILES files across 4
                    projects, N1_BOARDS boards each, one text layer per board
                    with a deterministic unique token, plus a library color +
                    typography per file -> writes expect.json
  fixture_stats     print "<files> <boards>" from expect.json
  queries           correctness + latency battery over deterministic tokens:
                    every token's top hit must carry the expected
                    file/page/board ids; prints JSON {ok,n,p50Ms,maxMs,...}
  snapshot F        canonical search results (fixed query list, tookMs
                    stripped) written to file F — rebuild-identity comparison
  plant_needle SLOT TEXT   add a text shape with TEXT to file 0 / board 0
                           (state saved in <workdir>/needle-SLOT.json)
  rename_needle SLOT TEXT  mod-obj: rewrite that needle's content to TEXT
  wait_hit SLOT Q TIMEOUT  poll search until needle SLOT is hit for query Q;
                           prints "<elapsed-s> <query-latency-ms>"
  wait_no_hit Q TIMEOUT poll until query Q returns ZERO hits; prints elapsed
  search_ids Q          top-hit ids for Q as JSON (debugging)
  status                GET /__api/vault/status (raw JSON)
"""

import json
import os
import random
import statistics
import sys
import time
import urllib.parse
import urllib.request
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import roundtrip as rt  # noqa: E402  (reads PENPOT_BACKEND/PENPOT_TOKEN env)

BASE = os.environ.get("PENPOT_BACKEND", "http://localhost:8916")
N_FILES = int(os.environ.get("N1_FILES", "100"))
N_BOARDS = int(os.environ.get("N1_BOARDS", "10"))
N_PROJECTS = 4
IDENT = {"a": 1.0, "b": 0.0, "c": 0.0, "d": 1.0, "e": 0.0, "f": 0.0}

WORDS = (
    "atlas breeze cobalt drift ember flux garnet harbor indigo juniper "
    "kelvin lumen meadow nectar onyx prism quartz raven summit thistle "
    "umber vertex willow xenon yonder zephyr"
).split()


def die(msg, code=2):
    print(f"HELPER-FAIL: {msg}", file=sys.stderr)
    sys.exit(code)


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
           "strokes": [],
           "fills": []}
    obj.update(geometry(x, y, 300, 40))
    return obj


def board_token(fi, bi):
    return f"tok{fi:03d}x{bi:02d}"


def board_text(rng, fi, bi):
    words = " ".join(rng.choice(WORDS) for _ in range(8))
    return f"{words} {board_token(fi, bi)} {rng.choice(WORDS)}"


def cmd_seed(workdir):
    rng = random.Random(1337)  # deterministic content
    c = rt.Client()
    team = c.rpc("get-profile", {})["defaultTeamId"]
    projects = []
    for p in range(N_PROJECTS):
        proj = c.rpc("create-project", {"teamId": team, "name": f"Torture-{p}"})
        projects.append(proj["id"])
    out = {"teamId": team, "projects": projects, "files": []}
    t0 = time.monotonic()
    for fi in range(N_FILES):
        project_id = projects[fi % N_PROJECTS]
        created = c.rpc("create-file",
                        {"name": f"file-{fi:03d}", "projectId": project_id})
        file_id, page_id = created["id"], created["data"]["pages"][0]
        changes, boards = [], []
        for bi in range(N_BOARDS):
            frid, tid = str(uuid.uuid4()), str(uuid.uuid4())
            bname = f"board-{fi:03d}-{bi:02d}"
            changes.append({"type": "add-obj", "id": frid, "pageId": page_id,
                            "frameId": rt.ROOT_FRAME, "parentId": rt.ROOT_FRAME,
                            "obj": frame_obj(frid, bname,
                                             (bi % 5) * 900, (bi // 5) * 700,
                                             800, 600)})
            text = board_text(rng, fi, bi)
            tobj = text_obj(tid, f"text-{fi:03d}-{bi:02d}", frid,
                            (bi % 5) * 900 + 50, (bi // 5) * 700 + 50, text)
            changes.append({"type": "add-obj", "id": tid, "pageId": page_id,
                            "frameId": frid, "parentId": frid, "obj": tobj})
            boards.append({"frameId": frid, "textId": tid, "name": bname,
                           "token": board_token(fi, bi), "text": text})
        color_id, typo_id = str(uuid.uuid4()), str(uuid.uuid4())
        changes.append({"type": "add-color", "color": {
            "id": color_id, "name": f"palette-{fi:03d}",
            "color": f"#{(0x100000 + fi * 3271) & 0xFFFFFF:06x}", "opacity": 1}})
        changes.append({"type": "add-typography", "typography": {
            "id": typo_id, "name": f"typo-{fi:03d}",
            "fontId": "sourcesanspro", "fontFamily": "sourcesanspro",
            "fontSize": "16", "fontStyle": "normal", "fontWeight": "400",
            "fontVariantId": "regular", "lineHeight": "1.2",
            "letterSpacing": "0", "textTransform": "none"}})
        c.rpc("update-file", {"id": file_id, "sessionId": str(uuid.uuid4()),
                              "revn": created["revn"], "vern": created["vern"],
                              "changes": changes})
        out["files"].append({"fileId": file_id, "pageId": page_id,
                             "projectId": project_id, "boards": boards,
                             "colorId": color_id, "typoId": typo_id})
    elapsed = time.monotonic() - t0
    with open(os.path.join(workdir, "expect.json"), "w") as fh:
        json.dump(out, fh, indent=2, sort_keys=True)
    print(json.dumps({"files": N_FILES, "boards": N_FILES * N_BOARDS,
                      "seedSeconds": round(elapsed, 1)}))


def load_expect(workdir):
    with open(os.path.join(workdir, "expect.json")) as fh:
        return json.load(fh)


def cmd_fixture_stats(workdir):
    e = load_expect(workdir)
    print(f"{len(e['files'])} {sum(len(f['boards']) for f in e['files'])}")


# ------------------------------------------------------------------- search

def http_search(q, limit=50):
    url = f"{BASE}/__api/vault/search?q={urllib.parse.quote(q)}&limit={limit}"
    t0 = time.monotonic()
    with urllib.request.urlopen(url) as resp:
        body = json.loads(resp.read())
    latency_ms = (time.monotonic() - t0) * 1000.0
    return body, latency_ms


def cmd_status(workdir):
    with urllib.request.urlopen(f"{BASE}/__api/vault/status") as resp:
        print(resp.read().decode())


def cmd_search_ids(workdir, q):
    body, ms = http_search(q)
    top = body["hits"][0] if body["hits"] else None
    print(json.dumps({"count": body["count"], "tookMs": body["tookMs"],
                      "latencyMs": ms, "top": top}, sort_keys=True))


def cmd_queries(workdir):
    """20 deterministic per-board tokens + one color + one typography name:
    correctness is HARD (exact file/page/board ids), latency is reported."""
    e = load_expect(workdir)
    rng = random.Random(4242)
    picks = []
    for _ in range(20):
        f = rng.choice(e["files"])
        b = rng.choice(f["boards"])
        picks.append((b["token"], f, b))
    latencies, failures = [], []
    for token, f, b in picks:
        body, ms = http_search(token)
        latencies.append(ms)
        hits = body["hits"]
        if not hits:
            failures.append(f"{token}: no hits")
            continue
        top = hits[0]
        expected_link = (f"/#/workspace?team-id={e['teamId']}"
                         f"&file-id={f['fileId']}&page-id={f['pageId']}")
        if (top["fileId"] != f["fileId"] or top["pageId"] != f["pageId"]
                or top["boardId"] != b["frameId"]
                or top["objectId"] != b["textId"]
                or top["kind"] != "text"
                or top["deepLink"] != expected_link):
            failures.append(f"{token}: wrong hit {top}")
    # library assets
    f0 = e["files"][0]
    body, ms = http_search("palette-000")
    latencies.append(ms)
    if not (body["hits"] and body["hits"][0]["kind"] == "color"
            and body["hits"][0]["fileId"] == f0["fileId"]
            and body["hits"][0]["objectId"] == f0["colorId"]):
        failures.append(f"palette-000: wrong hit {body['hits'][:1]}")
    body, ms = http_search("typo-000")
    latencies.append(ms)
    if not (body["hits"] and body["hits"][0]["kind"] == "typography"
            and body["hits"][0]["objectId"] == f0["typoId"]):
        failures.append(f"typo-000: wrong hit {body['hits'][:1]}")
    # a board-name query
    b0 = f0["boards"][0]
    body, ms = http_search(b0["name"])
    latencies.append(ms)
    if not (body["hits"] and any(
            h["kind"] == "board" and h["objectId"] == b0["frameId"]
            for h in body["hits"][:3])):
        failures.append(f"{b0['name']}: board not in top hits")
    print(json.dumps({
        "ok": not failures, "failures": failures, "n": len(latencies),
        "p50Ms": round(statistics.median(latencies), 2),
        "maxMs": round(max(latencies), 2),
        "meanMs": round(statistics.fmean(latencies), 2),
    }, sort_keys=True))
    if failures:
        sys.exit(1)


SNAPSHOT_QUERIES = ["tok000x00", "tok042x05", "tok099x09", "atlas", "zephyr",
                    "palette-013", "typo-077", "board-050-05", "quartz summit"]


def cmd_snapshot(workdir, outfile):
    out = {}
    for q in SNAPSHOT_QUERIES:
        body, _ms = http_search(q, limit=100)
        out[q] = {"count": body["count"], "hits": body["hits"]}
    with open(outfile, "w") as fh:
        json.dump(out, fh, indent=2, sort_keys=True)
    print(f"snapshot: {sum(v['count'] for v in out.values())} hits "
          f"over {len(SNAPSHOT_QUERIES)} queries -> {outfile}")


# ------------------------------------------------------------------- needle

def needle_path(workdir, slot):
    return os.path.join(workdir, f"needle-{slot}.json")


def cmd_plant_needle(workdir, slot, text):
    e = load_expect(workdir)
    f0, b0 = e["files"][0], e["files"][0]["boards"][0]
    c = rt.Client()
    g = c.rpc("get-file", {"id": f0["fileId"]})
    sid = str(uuid.uuid4())
    obj = text_obj(sid, f"needle-{slot}", b0["frameId"], 60, 400, text)
    c.rpc("update-file", {"id": f0["fileId"], "sessionId": str(uuid.uuid4()),
                          "revn": g["revn"], "vern": g["vern"],
                          "changes": [{"type": "add-obj", "id": sid,
                                       "pageId": f0["pageId"],
                                       "frameId": b0["frameId"],
                                       "parentId": b0["frameId"], "obj": obj}]})
    with open(needle_path(workdir, slot), "w") as fh:
        json.dump({"shapeId": sid, "fileId": f0["fileId"],
                   "pageId": f0["pageId"], "boardId": b0["frameId"],
                   "text": text, "plantedAt": time.time()}, fh)
    print(sid)


def cmd_rename_needle(workdir, slot, text):
    with open(needle_path(workdir, slot)) as fh:
        needle = json.load(fh)
    c = rt.Client()
    g = c.rpc("get-file", {"id": needle["fileId"]})
    c.rpc("update-file", {
        "id": needle["fileId"], "sessionId": str(uuid.uuid4()),
        "revn": g["revn"], "vern": g["vern"],
        "changes": [{"type": "mod-obj", "id": needle["shapeId"],
                     "pageId": needle["pageId"],
                     "operations": [{"type": "set", "attr": "content",
                                     "val": text_content(text)}]}]})
    print("renamed")


def cmd_wait_hit(workdir, slot, q, timeout_s):
    with open(needle_path(workdir, slot)) as fh:
        needle = json.load(fh)
    start = time.monotonic()
    deadline = start + float(timeout_s)
    last = None
    while True:
        try:
            body, ms = http_search(q)
            for h in body["hits"]:
                if h["objectId"] == needle["shapeId"]:
                    if (h["fileId"] != needle["fileId"]
                            or h["pageId"] != needle["pageId"]
                            or h["boardId"] != needle["boardId"]):
                        die(f"needle hit has wrong ids: {h} vs {needle}")
                    print(f"{time.monotonic() - start:.2f} {ms:.2f}")
                    return
            last = f"{body['count']} hits, none is the needle"
        except Exception as exc:  # index may 503 while booting
            last = str(exc)
        if time.monotonic() > deadline:
            die(f"timed out after {timeout_s}s waiting for {q!r}: {last}")
        time.sleep(0.5)


def cmd_wait_no_hit(workdir, q, timeout_s):
    start = time.monotonic()
    deadline = start + float(timeout_s)
    last = None
    while True:
        try:
            body, _ms = http_search(q)
            if body["count"] == 0:
                print(f"{time.monotonic() - start:.2f}")
                return
            last = f"{body['count']} hits still present"
        except Exception as exc:
            last = str(exc)
        if time.monotonic() > deadline:
            die(f"timed out after {timeout_s}s waiting for {q!r} to vanish: {last}")
        time.sleep(0.5)


def main():
    if len(sys.argv) < 3:
        die(f"usage: {sys.argv[0]} <subcommand> <workdir> [args...]", 64)
    cmd, workdir, args = sys.argv[1], sys.argv[2], sys.argv[3:]
    fn = {
        "seed": cmd_seed,
        "fixture_stats": cmd_fixture_stats,
        "queries": cmd_queries,
        "snapshot": cmd_snapshot,
        "plant_needle": cmd_plant_needle,
        "rename_needle": cmd_rename_needle,
        "wait_hit": cmd_wait_hit,
        "wait_no_hit": cmd_wait_no_hit,
        "search_ids": cmd_search_ids,
        "status": cmd_status,
    }.get(cmd)
    if fn is None:
        die(f"unknown subcommand {cmd}", 64)
    fn(workdir, *args)


if __name__ == "__main__":
    main()
