#!/usr/bin/env python3
"""RPC + HTTP helper for scripts/n3-home.sh (the N3 lighttable gate).

Reuses the N1 torture-fixture seeder (scripts/n1_index_helper.py) and the
M0-verified RPC client (scripts/roundtrip.py). Talks to the stack THROUGH THE
PROXY (the /__api/vault/* endpoints only exist there).

Env (set by the shell script):
  PENPOT_BACKEND   proxy base url (http://localhost:<proxy-port>)
  PENPOT_TOKEN     access token from <data_dir>/credentials.json
  N1_DESIGNS_DIR   the sync root (PENPOT_LOCAL_DESIGNS_DIR)
  N1_FILES/N1_BOARDS  fixture size (defaults 100 / 10)

Subcommands (all take <workdir> where the N1 seeder wrote expect.json):
  assert_boards          /__api/vault/boards lists every fixture board with the
                         EXACT deep link (string-asserted from the payload) +
                         the right projects; prints grid-load timing JSON
  assert_sort            recency sort is newest-first; name sort is A→Z
  assert_filter PROJECT  ?project=PROJECT returns only that project's boards
  plant_thumb            write a fake .exports render for file0/board0 on disk
                         (state + png), print "<relPath> <boardId>"
  assert_thumb           the planted board now has a thumb URL AND the thumb
                         route serves the exact PNG bytes (degraded elsewhere)
  edit_board NEWNAME     rename file0/board0's board via update-file (mod-obj)
  wait_card NEWNAME T    poll /boards until a card shows NEWNAME; print elapsed
  strip_last_sync        print the strip's lastSyncAt (or "null")
  wait_strip_advance B T poll /strip until lastSyncAt > B (rfc3339); print elapsed
  wait_strip_conflict T  poll /strip until a conflict row appears; print its
                         conflictCopyPath
  reveal PATH            GET /__api/vault/reveal?path=PATH; print "<code> <ok>"
"""

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
import roundtrip as rt  # noqa: E402
import n1_index_helper as n1  # noqa: E402  (reuse the seeder + text helpers)

BASE = os.environ.get("PENPOT_BACKEND", "http://localhost:8940")
DESIGNS = os.environ.get("N1_DESIGNS_DIR", "")


def die(msg, code=2):
    print(f"HELPER-FAIL: {msg}", file=sys.stderr)
    sys.exit(code)


def load_expect(workdir):
    with open(os.path.join(workdir, "expect.json")) as fh:
        return json.load(fh)


def http_get(path):
    t0 = time.monotonic()
    with urllib.request.urlopen(BASE + path) as resp:
        body = json.loads(resp.read())
    return body, (time.monotonic() - t0) * 1000.0


def http_get_raw(path):
    with urllib.request.urlopen(BASE + path) as resp:
        return resp.getcode(), resp.headers.get("Content-Type", ""), resp.read()


def manifest_rel_for(file_id):
    """The .penpot dir path (vault-relative) for a file id, from the manifest."""
    with open(os.path.join(DESIGNS, ".penpot-sync.json")) as fh:
        m = json.load(fh)
    entry = m["files"].get(file_id)
    if not entry:
        die(f"file {file_id} not in the manifest")
    return entry["path"]


# --------------------------------------------------------------------- boards

def cmd_assert_boards(workdir):
    e = load_expect(workdir)
    body, ms = http_get("/__api/vault/boards")
    boards = body["boards"]
    expected_total = sum(len(f["boards"]) for f in e["files"])
    if body["count"] != expected_total or len(boards) != expected_total:
        die(f"boards count {body['count']}/{len(boards)} != expected {expected_total}")
    # Projects control lists all 4 fixture projects.
    want_projects = sorted({f"Torture-{i}" for i in range(n1.N_PROJECTS)})
    if sorted(body["projects"]) != want_projects:
        die(f"projects {body['projects']} != {want_projects}")
    # Index the expected frame ids -> (fileId, pageId) and deep links.
    by_board = {}
    for f in e["files"]:
        for b in f["boards"]:
            link = (f"/#/workspace?team-id={e['teamId']}"
                    f"&file-id={f['fileId']}&page-id={f['pageId']}")
            by_board[b["frameId"]] = (f["fileId"], f["pageId"], b["name"], link)
    missing = 0
    for card in boards:
        exp = by_board.get(card["boardId"])
        if not exp:
            die(f"unexpected board in listing: {card['boardId']}")
        fid, pid, name, link = exp
        if (card["fileId"] != fid or card["pageId"] != pid
                or card["name"] != name):
            die(f"board {card['boardId']} shaped wrong: {card}")
        # THE deep-link string, asserted verbatim from the payload.
        if card["deepLink"] != link:
            die(f"board {card['boardId']} deepLink {card['deepLink']} != {link}")
        # Exports are OFF in this leg -> every card is degraded (thumb null).
        if card["thumb"] is not None:
            die(f"board {card['boardId']} should be degraded, got thumb {card['thumb']}")
    # Every fixture board present exactly once.
    seen = {c["boardId"] for c in boards}
    if seen != set(by_board):
        die(f"listing missing {len(set(by_board) - seen)} boards")
    print(json.dumps({"count": body["count"], "gridLoadMs": round(ms, 1),
                      "projects": len(body["projects"])}))


def cmd_assert_sort(workdir):
    body, _ = http_get("/__api/vault/boards?sort=name")
    names = [c["name"].lower() for c in body["boards"]]
    if names != sorted(names):
        die("name sort not A→Z")
    body, _ = http_get("/__api/vault/boards?sort=recency")
    times = [c["lastSyncedAt"] for c in body["boards"]]
    if times != sorted(times, reverse=True):
        die("recency sort not newest-first")
    print("sort-ok")


def cmd_assert_filter(workdir, project):
    body, _ = http_get(f"/__api/vault/boards?project={urllib.parse.quote(project)}")
    if body["count"] == 0:
        die(f"filter {project} returned nothing")
    if any(c["project"] != project for c in body["boards"]):
        die(f"filter {project} leaked other projects")
    # The projects control still lists every project even when filtered.
    if len(body["projects"]) != n1.N_PROJECTS:
        die(f"filtered listing dropped projects: {body['projects']}")
    print(f"filter-ok {body['count']} boards in {project}")


# ------------------------------------------------------------------- thumbnails

# A minimal valid 1x1 opaque-red PNG, built at runtime (no binary literals).
def tiny_png():
    def chunk(tag, data):
        return (struct.pack(">I", len(data)) + tag + data
                + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF))
    sig = b"\x89PNG\r\n\x1a\n"
    ihdr = struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0)  # 1x1, 8-bit RGB
    raw = b"\x00\xff\x00\x00"  # filter byte + one red pixel
    idat = zlib.compress(raw)
    return sig + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b"")


def cmd_plant_thumb(workdir):
    e = load_expect(workdir)
    f0 = e["files"][0]
    b0 = f0["boards"][0]
    rel = manifest_rel_for(f0["fileId"])          # e.g. Torture-0/file-000.penpot
    exports_rel = rel[: -len(".penpot")] + ".exports"
    exports_dir = os.path.join(DESIGNS, exports_rel)
    os.makedirs(exports_dir, exist_ok=True)
    stem = b0["name"]  # a safe stem (board-000-00) -> unique_stems is identity
    png = tiny_png()
    with open(os.path.join(exports_dir, f"{stem}.png"), "wb") as fh:
        fh.write(png)
    state = {
        "schemaVersion": 1,
        "fileId": f0["fileId"],
        "renderedFromHash": "planted-by-n3-gate",
        "renderedAt": "2026-07-14T00:00:00Z",
        "boards": [{"objectId": b0["frameId"], "pageId": f0["pageId"],
                    "name": b0["name"], "fileStem": stem}],
    }
    with open(os.path.join(exports_dir, ".exports-state.json"), "w") as fh:
        json.dump(state, fh)
    with open(os.path.join(workdir, "planted.json"), "w") as fh:
        json.dump({"boardId": b0["frameId"], "relPath": rel,
                   "pngLen": len(png)}, fh)
    print(f"{rel} {b0['frameId']}")


def cmd_assert_thumb(workdir):
    with open(os.path.join(workdir, "planted.json")) as fh:
        planted = json.load(fh)
    body, _ = http_get("/__api/vault/boards")
    card = next((c for c in body["boards"] if c["boardId"] == planted["boardId"]), None)
    if not card:
        die("planted board vanished from listing")
    if not card["thumb"]:
        die("planted board still degraded (thumb is null)")
    # Fetch the thumbnail and check it is the exact PNG bytes we planted.
    code, ctype, data = http_get_raw(card["thumb"])
    if code != 200:
        die(f"thumb route returned {code}")
    if "image/png" not in ctype:
        die(f"thumb content-type {ctype!r} is not image/png")
    if len(data) != planted["pngLen"] or not data.startswith(b"\x89PNG"):
        die(f"thumb bytes wrong (len {len(data)} vs {planted['pngLen']})")
    # A board with no render still 404s (degraded path intact).
    other = next((c for c in body["boards"] if c["thumb"] is None), None)
    if other is None:
        die("expected some degraded boards alongside the planted one")
    print(f"thumb-ok {len(data)} bytes served, {card['thumb']}")


# ----------------------------------------------------------------- edit + strip

def cmd_edit_board(workdir, newname):
    e = load_expect(workdir)
    f0, b0 = e["files"][0], e["files"][0]["boards"][0]
    c = rt.Client()
    g = c.rpc("get-file", {"id": f0["fileId"]})
    c.rpc("update-file", {
        "id": f0["fileId"], "sessionId": str(uuid.uuid4()),
        "revn": g["revn"], "vern": g["vern"],
        "changes": [{"type": "mod-obj", "id": b0["frameId"],
                     "pageId": f0["pageId"],
                     "operations": [{"type": "set", "attr": "name",
                                     "val": newname}]}]})
    print(f"renamed {b0['frameId']} -> {newname}")


def cmd_wait_card(workdir, newname, timeout_s):
    start = time.monotonic()
    deadline = start + float(timeout_s)
    last = None
    while True:
        try:
            body, _ = http_get("/__api/vault/boards")
            if any(c["name"] == newname for c in body["boards"]):
                print(f"{time.monotonic() - start:.2f}")
                return
            last = f"{body['count']} boards, none named {newname!r}"
        except Exception as exc:
            last = str(exc)
        if time.monotonic() > deadline:
            die(f"card never showed {newname!r} within {timeout_s}s: {last}")
        time.sleep(0.5)


def cmd_strip_last_sync(workdir):
    body, _ = http_get("/__api/vault/strip")
    print(body.get("lastSyncAt") or "null")


def cmd_wait_strip_advance(workdir, before, timeout_s):
    start = time.monotonic()
    deadline = start + float(timeout_s)
    before = None if before == "null" else before
    last = None
    while True:
        try:
            body, _ = http_get("/__api/vault/strip")
            cur = body.get("lastSyncAt")
            if cur and (before is None or cur > before):
                print(f"{time.monotonic() - start:.2f}")
                return
            last = f"lastSyncAt={cur}"
        except Exception as exc:
            last = str(exc)
        if time.monotonic() > deadline:
            die(f"strip lastSyncAt never advanced past {before} in {timeout_s}s: {last}")
        time.sleep(0.5)


def cmd_wait_strip_conflict(workdir, timeout_s):
    start = time.monotonic()
    deadline = start + float(timeout_s)
    last = None
    while True:
        try:
            body, _ = http_get("/__api/vault/strip")
            for r in body.get("rows", []):
                if r.get("isConflict") and r.get("conflictCopyPath"):
                    print(r["conflictCopyPath"])
                    return
            last = f"{len(body.get('rows', []))} rows, no conflict; conflicts={body.get('conflicts')}"
        except Exception as exc:
            last = str(exc)
        if time.monotonic() > deadline:
            die(f"no conflict row in the strip within {timeout_s}s: {last}")
        time.sleep(0.5)


def cmd_reveal(workdir, path):
    url = BASE + "/__api/vault/reveal?path=" + urllib.parse.quote(path)
    try:
        with urllib.request.urlopen(url) as resp:
            body = json.loads(resp.read())
            print(f"{resp.getcode()} {body.get('ok')}")
    except urllib.error.HTTPError as ex:
        print(f"{ex.code} false")


def main():
    if len(sys.argv) < 3:
        die(f"usage: {sys.argv[0]} <subcommand> <workdir> [args...]", 64)
    cmd, workdir, args = sys.argv[1], sys.argv[2], sys.argv[3:]
    fn = {
        "assert_boards": cmd_assert_boards,
        "assert_sort": cmd_assert_sort,
        "assert_filter": cmd_assert_filter,
        "plant_thumb": cmd_plant_thumb,
        "assert_thumb": cmd_assert_thumb,
        "edit_board": cmd_edit_board,
        "wait_card": cmd_wait_card,
        "strip_last_sync": cmd_strip_last_sync,
        "wait_strip_advance": cmd_wait_strip_advance,
        "wait_strip_conflict": cmd_wait_strip_conflict,
        "reveal": cmd_reveal,
    }.get(cmd)
    if fn is None:
        die(f"unknown subcommand {cmd}", 64)
    fn(workdir, *args)


if __name__ == "__main__":
    main()
