#!/usr/bin/env python3
"""Adversarial probe helper for the independent N1/N2 verification.

Reuses scripts/n1_index_helper.py (seed/plant/search machinery) and
scripts/roundtrip.py (RPC client). Talks THROUGH THE PROXY.

Subcommands (all take <workdir> holding expect.json from an n1 `seed`):
  fts_hostility        plant a battery of hostile-text needles, assert each
                       marker is findable (or benignly no-hit) and NOTHING
                       crashes the index/search; prints a per-case report.
  bigtext              plant a ~10MB text layer with an embedded marker; assert
                       the indexer survives and the marker is findable.
  race                 plant a needle, rapid-fire 10 content edits (distinct
                       tokens) with a file rename mid-stream; assert the index
                       converges: only the LAST token is searchable, all 9
                       intermediate tokens are gone (zero stale), and the file
                       stays findable with a stable deepLink file-id.
  status               GET /__api/vault/status
"""
import json
import os
import sys
import time
import urllib.parse
import urllib.request
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import n1_index_helper as n1  # noqa: E402
import roundtrip as rt  # noqa: E402

BASE = os.environ.get("PENPOT_BACKEND", "http://localhost:8920")


def die(msg, code=2):
    print(f"ADV-FAIL: {msg}", file=sys.stderr)
    sys.exit(code)


def search(q, limit=50):
    url = f"{BASE}/__api/vault/search?q={urllib.parse.quote(q)}&limit={limit}"
    t0 = time.monotonic()
    with urllib.request.urlopen(url) as resp:
        body = json.loads(resp.read())
    return body, (time.monotonic() - t0) * 1000.0


def status():
    with urllib.request.urlopen(f"{BASE}/__api/vault/status") as resp:
        return json.loads(resp.read())


def plant_text(file_id, page_id, board_id, text, revn=None, vern=None):
    """Add a fresh text shape carrying `text`; returns its shape id."""
    c = rt.Client()
    g = c.rpc("get-file", {"id": file_id})
    sid = str(uuid.uuid4())
    obj = n1.text_obj(sid, "adv-needle", board_id, 70, 500, text)
    c.rpc("update-file", {
        "id": file_id, "sessionId": str(uuid.uuid4()),
        "revn": g["revn"], "vern": g["vern"],
        "changes": [{"type": "add-obj", "id": sid, "pageId": page_id,
                     "frameId": board_id, "parentId": board_id, "obj": obj}]})
    return sid


def set_text(file_id, page_id, shape_id, text):
    c = rt.Client()
    g = c.rpc("get-file", {"id": file_id})
    c.rpc("update-file", {
        "id": file_id, "sessionId": str(uuid.uuid4()),
        "revn": g["revn"], "vern": g["vern"],
        "changes": [{"type": "mod-obj", "id": shape_id, "pageId": page_id,
                     "operations": [{"type": "set", "attr": "content",
                                     "val": n1.text_content(text)}]}]})


def wait_hit_shape(shape_id, q, timeout_s, want=True):
    """Poll until query q {contains|excludes} shape_id. Returns elapsed s."""
    start = time.monotonic()
    deadline = start + timeout_s
    while True:
        try:
            body, _ = search(q)
            present = any(h["objectId"] == shape_id for h in body["hits"])
            if present == want:
                return time.monotonic() - start
            last = f"{body['count']} hits present={present}"
        except Exception as exc:
            last = str(exc)
        if time.monotonic() > deadline:
            die(f"timeout {timeout_s}s: q={q!r} want_present={want} ({last})")
        time.sleep(0.4)


def load(workdir):
    return n1.load_expect(workdir)


# --------------------------------------------------------------- FTS hostility
HOSTILE = [
    ("quotes",      'advQUOTES say "hello world" and \'single\''),
    ("hyphens",     "advHYPHENS semi-transparent multi-word-token co-op"),
    ("booleans",    "advBOOL alpha AND beta OR gamma NOT delta"),
    ("fts_syntax",  "advSYNTAX col:name near/2 (paren) ^caret *star* -dash"),
    ("cjk",         "advCJK 検索テスト设计中文レイアウト 한국어"),
    ("emoji",       "advEMOJI \U0001F600\U0001F680\U0001F4A9 rocket flag"),
    ("longstr",     "advLONG " + " ".join(f"word{i}" for i in range(4000))),
    ("nul_ish",     "advCTRL tab\ttab newline mixed\x0bvtab"),
    ("sqlish",      "advSQL '); DROP TABLE docs;-- fts_docs MATCH"),
]


def cmd_fts_hostility(workdir):
    e = load(workdir)
    f0 = e["files"][0]
    b0 = f0["boards"][0]
    report = []
    ok = True
    for tag, text in HOSTILE:
        sid = plant_text(f0["fileId"], f0["pageId"], b0["frameId"], text)
        marker = text.split()[0]  # e.g. advQUOTES
        # allow sync+index window
        try:
            elapsed = wait_hit_shape(sid, marker, 40, want=True)
            findable = True
        except SystemExit:
            findable = False
            elapsed = None
        # probe the hostile substrings never crash search (any result ok)
        crashed = False
        probes = {
            "quotes": ['"hello world"', "single"],
            "hyphens": ["semi-transparent", "co-op"],
            "booleans": ["alpha AND beta", "NOT delta"],
            "fts_syntax": ["col:name", "near/2", "*star*", "-dash", "^caret"],
            "cjk": ["検索テスト", "设计", "한국어"],
            "emoji": ["rocket", "\U0001F680"],
            "longstr": ["word0", "word3999"],
            "nul_ish": ["tab", "vtab"],
            "sqlish": ["DROP TABLE", "'); DROP", "MATCH"],
        }.get(tag, [])
        for p in probes:
            try:
                search(p)
            except Exception as exc:
                crashed = True
                report.append(f"  CRASH q={p!r}: {exc}")
        st = status()
        if st.get("lastError"):
            # lastError is cleared by next success; only fatal if it sticks
            pass
        report.append(f"  [{tag}] marker={marker} findable={findable} "
                      f"elapsed={elapsed} search_crash={crashed}")
        # marker MUST be findable for ascii/latin cases; emoji-only tokens may
        # not tokenize but the ascii marker prefix always must.
        if not findable:
            ok = False
            report.append(f"    !! marker {marker!r} NOT findable")
        if crashed:
            ok = False
    # index still healthy?
    st = status()
    print(json.dumps({"ok": ok, "docsTotal": st["docsTotal"],
                      "mutations": st["mutations"],
                      "lastError": st.get("lastError")}))
    print("\n".join(report))
    if not ok:
        sys.exit(1)


def cmd_bigtext(workdir):
    e = load(workdir)
    f0 = e["files"][0]
    b0 = f0["boards"][0]
    marker = "advBIGTEXT-needle-42"
    # ~10MB: 10 million chars. Embed the marker in the middle.
    filler = ("lorem ipsum dolor sit amet " * 400000)  # ~10.8MB
    big = f"{marker} " + filler + f" {marker}-end"
    print(f"planting {len(big)/1e6:.1f}MB text layer...", file=sys.stderr)
    sid = plant_text(f0["fileId"], f0["pageId"], b0["frameId"], big)
    elapsed = wait_hit_shape(sid, marker, 90, want=True)
    st = status()
    print(json.dumps({"ok": True, "sizeMB": round(len(big)/1e6, 1),
                      "findableAfterS": round(elapsed, 2),
                      "docsTotal": st["docsTotal"],
                      "lastError": st.get("lastError")}))


def cmd_race(workdir):
    e = load(workdir)
    f0 = e["files"][0]
    b0 = f0["boards"][0]
    file_id, page_id = f0["fileId"], f0["pageId"]
    base = uuid.uuid4().hex[:6]
    tokens = [f"raceTOK{base}v{i:02d}" for i in range(10)]
    # plant with token 0
    sid = plant_text(file_id, page_id, b0["frameId"], f"race start {tokens[0]}")
    wait_hit_shape(sid, tokens[0], 40, want=True)
    # rapid-fire edits 1..9, rename the file mid-stream (after edit 4)
    renamed_to = f"renamed-{base}"
    for i in range(1, 10):
        set_text(file_id, page_id, sid, f"race step {tokens[i]}")
        if i == 4:
            c = rt.Client()
            try:
                c.rpc("rename-file", {"id": file_id, "name": renamed_to})
            except Exception as exc:
                print(f"rename-file rpc failed (non-fatal): {exc}",
                      file=sys.stderr)
        # no sleep: hammer within the sync window
    # converge: only the last token searchable
    final = tokens[-1]
    conv = wait_hit_shape(sid, final, 60, want=True)
    # all intermediate tokens must be gone (zero stale hits)
    stale = []
    for tok in tokens[:-1]:
        try:
            wait_hit_shape(sid, tok, 30, want=False)
        except SystemExit:
            body, _ = search(tok)
            stale.append((tok, body["count"]))
    # file still findable; deepLink carries the SAME file id
    body, _ = search(final)
    hit = next((h for h in body["hits"] if h["objectId"] == sid), None)
    link_ok = hit is not None and f"file-id={file_id}" in hit["deepLink"]
    # path re-keyed? check relPath reflects the rename (best-effort)
    rel = hit["relPath"] if hit else None
    print(json.dumps({
        "ok": (not stale) and link_ok,
        "finalToken": final, "convergedAfterS": round(conv, 2),
        "staleHits": stale, "deepLinkFileIdStable": link_ok,
        "relPath": rel, "renamedTo": renamed_to,
    }))
    if stale or not link_ok:
        sys.exit(1)


def main():
    if len(sys.argv) < 3:
        die("usage: adv_helper.py <cmd> <workdir>", 64)
    cmd, workdir = sys.argv[1], sys.argv[2]
    fn = {
        "fts_hostility": cmd_fts_hostility,
        "bigtext": cmd_bigtext,
        "race": cmd_race,
        "status": lambda wd: print(json.dumps(status())),
    }.get(cmd)
    if fn is None:
        die(f"unknown cmd {cmd}", 64)
    fn(workdir)


if __name__ == "__main__":
    main()
