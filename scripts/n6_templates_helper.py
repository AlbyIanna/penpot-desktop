#!/usr/bin/env python3
"""N6 template-gallery gate helper (scripts/n6-templates.sh).

Subcommands, each printing a human line and exiting 0 (pass) / 1 (fail):

  assert_catalog  <base>
      GET <base>/__api/templates lists EXACTLY the shippable set (the 15
      builtin binfiles in scripts/n6-templates-reference.json), each with a
      display name, a known format, and a positive size.

  assert_gallery_offline <base>
      GET <base>/__templates returns 200 and is fully self-contained — no
      external (non-relative) URL anywhere in the HTML (the gallery is offline).

  assert_invalid  <base>
      POST <base>/__api/templates/new with a bogus templateId → a clean 4xx,
      and the stack still answers /__api/templates afterwards (no crash).

  new_and_verify  <base> <backend> <token> <vault_root> <template_id> <wait_s>
      The end-to-end check for one template:
        1. POST /__api/templates/new {templateId} → a new file + deep link.
        2. Wait for the sync daemon to materialize + index it; resolve its
           on-disk `.penpot` dir from /__api/vault/boards.
        3. Its page/board count + text content match the template (reference).
        4. The materialized tree ROUND-TRIPS A=B per roundtrip.py semantics
           (re-zip → import in-place → re-export → identical semantic hash).
      Prints a JSON report on the last line.

Stdlib only + scripts/roundtrip.py (imported for the normalize/hash/zip ops).
"""

import io
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
REFERENCE = os.path.join(HERE, "n6-templates-reference.json")
ROOT_FRAME = "00000000-0000-0000-0000-000000000000"


def load_reference():
    with open(REFERENCE) as fh:
        return json.load(fh)


# --------------------------------------------------------------- HTTP helpers

def http_get(url, timeout=30):
    req = urllib.request.Request(url, headers={"Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return resp.status, resp.read()


def http_post_json(url, payload, timeout=600):
    body = json.dumps(payload).encode()
    req = urllib.request.Request(
        url, data=body,
        headers={"Content-Type": "application/json", "Accept": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


# ------------------------------------------------------------- tree analysis

def analyze_tree(penpot_dir):
    """Count distinct pages, boards (every type=='frame' object — including the
    per-page root frame, matching the spike's phase-2 method that produced the
    reference), and gather all text into one blob — walking the on-disk unzipped
    binfile tree (`files/<fid>/pages/<pid>.json` + `.../<pid>/<obj>.json`)."""
    pages = set()
    boards = 0
    text_blob_parts = []
    page_re = re.compile(r"files/[0-9a-f-]+/pages/([0-9a-f-]+)\.json$")
    obj_re = re.compile(r"files/[0-9a-f-]+/pages/[0-9a-f-]+/[0-9a-f-]+\.json$")
    for dirpath, _dirs, files in os.walk(penpot_dir):
        for fn in files:
            p = os.path.join(dirpath, fn)
            rel = os.path.relpath(p, penpot_dir).replace(os.sep, "/")
            m = page_re.search(rel)
            if m:
                pages.add(m.group(1))
                continue
            if obj_re.search(rel):
                try:
                    with open(p, "rb") as fh:
                        obj = json.loads(fh.read())
                except Exception:
                    continue
                if obj.get("type") == "frame":
                    boards += 1
                if obj.get("type") == "text":
                    t = extract_text(obj.get("content"))
                    if t:
                        text_blob_parts.append(t)
    return len(pages), boards, " ␟ ".join(text_blob_parts)


def extract_text(content):
    out = []

    def walk(n):
        if isinstance(n, dict):
            if isinstance(n.get("text"), str):
                out.append(n["text"])
            for v in n.values():
                walk(v)
        elif isinstance(n, list):
            for x in n:
                walk(x)

    walk(content)
    return " ".join(out).strip()


# ------------------------------------------------------------------ commands

def assert_catalog(base):
    ref = load_reference()
    status, body = http_get(base + "/__api/templates")
    if status != 200:
        print(f"FAIL: /__api/templates HTTP {status}")
        return 1
    data = json.loads(body)
    got_ids = sorted(t["id"] for t in data.get("templates", []))
    want_ids = sorted(ref.keys())
    if data.get("count") != len(want_ids) or got_ids != want_ids:
        print(f"FAIL: catalog mismatch count={data.get('count')} "
              f"got={got_ids} want={want_ids}")
        return 1
    for t in data["templates"]:
        if not t.get("name"):
            print(f"FAIL: {t['id']} has no display name")
            return 1
        if t.get("format") not in ("v3-zip", "legacy-v1"):
            print(f"FAIL: {t['id']} unexpected format {t.get('format')!r}")
            return 1
        if not t.get("sizeBytes", 0) > 0:
            print(f"FAIL: {t['id']} non-positive size")
            return 1
    print(f"PASS: catalog lists exactly {len(want_ids)} shippable templates "
          f"(names + formats + sizes present)")
    return 0


# A "external URL" = an absolute http(s) URL or a protocol-relative `//host`
# reference. Relative links (`/__home`) are fine and expected.
EXTERNAL_URL_RE = re.compile(r"""(?:https?:)?//[A-Za-z0-9]""")


def assert_gallery_offline(base):
    status, body = http_get(base + "/__templates")
    if status != 200:
        print(f"FAIL: /__templates HTTP {status}")
        return 1
    html = body.decode("utf-8", "replace")
    hits = EXTERNAL_URL_RE.findall(html)
    if hits:
        print(f"FAIL: gallery references external URLs: {hits[:5]}")
        return 1
    if "New from template" not in html:
        print("FAIL: gallery HTML missing expected content")
        return 1
    print("PASS: /__templates serves offline (200, no external URL references)")
    return 0


def assert_invalid(base):
    status, body = http_post_json(
        base + "/__api/templates/new", {"templateId": "__does_not_exist__"})
    if not (400 <= status < 500):
        print(f"FAIL: invalid templateId returned HTTP {status} (want 4xx): "
              f"{body[:200]!r}")
        return 1
    # Stack still healthy afterwards.
    status2, _ = http_get(base + "/__api/templates")
    if status2 != 200:
        print(f"FAIL: catalog unavailable after invalid request (HTTP {status2})")
        return 1
    print(f"PASS: invalid templateId → HTTP {status} (clean 4xx), stack healthy")
    return 0


def _import_roundtrip(rt, client, penpot_dir, file_id, project_id):
    """A=B round-trip on the materialized tree: A = semantic hash of the disk
    tree; re-zip → import IN-PLACE (same file id) → re-export (embedAssets,
    matching the daemon) → normalize → B. Returns (a_hash, b_hash)."""
    files_a = rt.tree_files(penpot_dir)
    hash_a = rt.tree_hash(rt.semantic_files(files_a))

    zip_a = rt.zip_tree(penpot_dir)
    rt.import_binfile(client, project_id, zip_a, file_id=file_id, name="n6-settle")

    end = client.rpc_sse("export-binfile", {
        "fileId": file_id, "includeLibraries": False, "embedAssets": True})
    zip_b = client.download(rt.parse_transit_uri(end))
    work_b = penpot_dir + ".rtB"
    rt.unzip_to(zip_b, work_b)
    files_b = rt.tree_files(work_b)
    hash_b = rt.tree_hash(rt.semantic_files(files_b))
    return hash_a, hash_b


def new_and_verify(base, backend, token, vault_root, template_id, wait_s):
    ref = load_reference()
    if template_id not in ref:
        print(f"FAIL: no reference for {template_id}")
        return 1
    r = ref[template_id]
    t0 = time.time()

    # 1. Create the file from the template. The POST runs the whole
    #    settle-until-fixpoint synchronously, so its timeout must cover the
    #    heavy templates (penpot-design-system settles ~minutes); scale it with
    #    the caller's materialize budget.
    create_timeout = max(600.0, float(wait_s) * 4)
    status, body = http_post_json(
        base + "/__api/templates/new", {"templateId": template_id},
        timeout=create_timeout)
    if status != 200:
        print(f"FAIL: new-from-template {template_id} HTTP {status}: {body[:300]!r}")
        return 1
    resp = json.loads(body)
    file_id = resp.get("fileId")
    deep_link = resp.get("deepLink", "")
    if not file_id or f"file-id={file_id}" not in deep_link:
        print(f"FAIL: bad new-from-template response: {resp}")
        return 1
    if "page-id=" not in deep_link:
        print(f"FAIL: deep link missing page-id: {deep_link}")
        return 1
    create_ms = int((time.time() - t0) * 1000)

    # 2. Wait for the daemon to materialize + index it → resolve rel_path.
    rel_path = None
    deadline = time.time() + float(wait_s)
    while time.time() < deadline:
        try:
            st, bd = http_get(base + "/__api/vault/boards")
            if st == 200:
                for c in json.loads(bd).get("boards", []):
                    if c.get("fileId") == file_id:
                        rel_path = c.get("relPath")
                        break
        except Exception:
            pass
        if rel_path:
            break
        time.sleep(2)
    if not rel_path:
        print(f"FAIL: {template_id} never appeared on disk/index within {wait_s}s")
        return 1
    materialize_ms = int((time.time() - t0) * 1000)

    penpot_dir = os.path.join(vault_root, rel_path)
    if not os.path.isdir(penpot_dir):
        print(f"FAIL: materialized path is not a dir: {penpot_dir}")
        return 1

    # 3. Content matches the template (pages / boards / text).
    pages, boards, blob = analyze_tree(penpot_dir)
    if pages != r["pages"]:
        print(f"FAIL: {template_id} pages={pages} want={r['pages']}")
        return 1
    if boards != r["boards"]:
        print(f"FAIL: {template_id} boards={boards} want={r['boards']}")
        return 1
    missing = [t for t in r["texts"] if t and t not in blob]
    if missing:
        print(f"FAIL: {template_id} missing template text: {missing[:3]}")
        return 1

    # 4. Round-trip A=B on the materialized tree.
    sys.path.insert(0, HERE)
    os.environ["PENPOT_BACKEND"] = backend
    os.environ["PENPOT_FRONTEND"] = base
    os.environ["PENPOT_TOKEN"] = token
    import roundtrip as rt
    rt.BACKEND, rt.FRONTEND, rt.TOKEN = backend, base, token
    client = rt.Client()
    # The file's project (for the in-place import multipart field).
    finfo = client.rpc("get-file", {"id": file_id})
    project_id = finfo.get("projectId") if isinstance(finfo, dict) else None
    if not project_id:
        print(f"FAIL: could not resolve projectId for {file_id}")
        return 1
    hash_a, hash_b = _import_roundtrip(rt, client, penpot_dir, file_id, project_id)
    ab = hash_a == hash_b

    report = {
        "template": template_id,
        "format": resp.get("format"),
        "settled": resp.get("settled"),
        "settleCycles": resp.get("settleCycles"),
        "fileId": file_id,
        "relPath": rel_path,
        "pages": pages,
        "boards": boards,
        "roundTripAB": ab,
        "hashA": hash_a,
        "hashB": hash_b,
        "createMs": create_ms,
        "materializeMs": materialize_ms,
        "deepLink": deep_link,
    }
    if not ab:
        print(f"FAIL: {template_id} round-trip A!=B  A={hash_a[:12]} B={hash_b[:12]}")
        print(json.dumps(report))
        return 1
    print(f"PASS: {template_id} → {rel_path}  pages={pages} boards={boards} "
          f"text✓ round-trip A=B ✓ ({materialize_ms}ms)")
    print(json.dumps(report))
    return 0


def main():
    if len(sys.argv) < 2:
        print("usage: n6_templates_helper.py <subcommand> ...", file=sys.stderr)
        return 2
    cmd = sys.argv[1]
    a = sys.argv[2:]
    if cmd == "assert_catalog":
        return assert_catalog(a[0])
    if cmd == "assert_gallery_offline":
        return assert_gallery_offline(a[0])
    if cmd == "assert_invalid":
        return assert_invalid(a[0])
    if cmd == "new_and_verify":
        return new_and_verify(a[0], a[1], a[2], a[3], a[4], a[5])
    print(f"unknown subcommand {cmd!r}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
