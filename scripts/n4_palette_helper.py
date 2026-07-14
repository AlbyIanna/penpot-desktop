#!/usr/bin/env python3
"""N4 palette/peek/checkpoint HTTP assertions (companion to n4-palette.sh).

Talks to the SAME running stack the gate booted, over the proxy (the
/__api/vault/* endpoints only exist there). Reuses the N1/N3 seed
expectation (expect.json) and the N3 planted render (planted.json).

Subcommands (all take <workdir> after the seeded stack is up):
  assert_palette WORKDIR NEEDLE      query /__api/vault/palette?q=NEEDLE and
                                     assert the renamed target board ranks
                                     FIRST, with kind=board and the EXACT
                                     /#/workspace deep link (Enter payload);
                                     prints "<tookMs> <deepLink>"
  assert_peek WORKDIR                the planted board's Peek preview: fetch
                                     its .exports render via the thumb route,
                                     assert HTTP 200 + the served bytes' sha256
                                     equals the on-disk render's sha256;
                                     prints "<code> <sha256> <bytes>"
"""
import hashlib
import json
import os
import sys
import time
import urllib.parse
import urllib.request

BASE = os.environ["PENPOT_BACKEND"].rstrip("/")
DESIGNS = os.environ["N1_DESIGNS_DIR"]


def die(msg, code=2):
    print(f"HELPER-FAIL: {msg}", file=sys.stderr)
    sys.exit(code)


def load_json(workdir, name):
    with open(os.path.join(workdir, name)) as fh:
        return json.load(fh)


def http_get(path):
    t0 = time.monotonic()
    with urllib.request.urlopen(BASE + path) as resp:
        body = json.loads(resp.read())
    return body, (time.monotonic() - t0) * 1000.0


def http_get_raw(path):
    with urllib.request.urlopen(BASE + path) as resp:
        return resp.getcode(), resp.headers.get("Content-Type", ""), resp.read()


def cmd_assert_palette(workdir, needle):
    e = load_json(workdir, "expect.json")
    f0 = e["files"][0]
    b0 = f0["boards"][0]
    want_board = b0["frameId"]
    want_link = (f"/#/workspace?team-id={e['teamId']}"
                 f"&file-id={f0['fileId']}&page-id={f0['pageId']}")
    body, ms = http_get("/__api/vault/palette?q=" + urllib.parse.quote(needle))
    hits = body.get("hits", [])
    if not hits:
        die(f"palette returned no hits for {needle!r}")
    top = hits[0]
    # THE gateable value: the intended board ranks first.
    if top.get("kind") != "board":
        die(f"top hit kind {top.get('kind')} != board: {top}")
    if top.get("boardId") != want_board:
        die(f"top hit board {top.get('boardId')} != target {want_board}")
    # Enter payload = the EXACT deep link.
    if top.get("deepLink") != want_link:
        die(f"top hit deepLink {top.get('deepLink')} != {want_link}")
    print(f"{round(body.get('tookMs', ms), 2)} {top['deepLink']}")


def cmd_assert_peek(workdir):
    planted = load_json(workdir, "planted.json")
    e = load_json(workdir, "expect.json")
    f0 = e["files"][0]
    b0 = f0["boards"][0]
    board = planted["boardId"]
    rel = planted["relPath"]
    stem = b0["name"]
    on_disk = os.path.join(DESIGNS, rel[: -len(".penpot")] + ".exports", f"{stem}.png")
    with open(on_disk, "rb") as fh:
        disk_bytes = fh.read()
    disk_sha = hashlib.sha256(disk_bytes).hexdigest()

    # Locate the served thumb URL from the boards listing (same URL Peek uses).
    body, _ = http_get("/__api/vault/boards")
    card = next((c for c in body["boards"] if c["boardId"] == board), None)
    if not card or not card.get("thumb"):
        die("planted board has no thumb URL (Peek would be degraded)")
    code, ctype, served = http_get_raw(card["thumb"])
    if code != 200:
        die(f"peek preview returned {code}")
    if "image/png" not in ctype:
        die(f"peek content-type {ctype!r} not image/png")
    served_sha = hashlib.sha256(served).hexdigest()
    if served_sha != disk_sha:
        die(f"peek bytes sha {served_sha} != on-disk {disk_sha}")
    print(f"{code} {served_sha} {len(served)}")


def main():
    if len(sys.argv) < 3:
        die(f"usage: {sys.argv[0]} <subcommand> <workdir> [args...]", 64)
    cmd, workdir, args = sys.argv[1], sys.argv[2], sys.argv[3:]
    fn = {
        "assert_palette": cmd_assert_palette,
        "assert_peek": cmd_assert_peek,
    }.get(cmd)
    if fn is None:
        die(f"unknown subcommand {cmd}", 64)
    fn(workdir, *args)


if __name__ == "__main__":
    main()
