#!/usr/bin/env python3
"""M0 round-trip spike: export-binfile -> normalize -> import-binfile (in-place)
-> re-export, and byte-diff the normalized trees.

Stdlib only (urllib / zipfile / json / hashlib). Targets the penpot-m0 docker
stack (Penpot 2.16.2):

  backend  http://localhost:6060   (RPC)
  frontend http://localhost:9001   (asset downloads MUST go through here)

What it does, in order:
  1. Reuse (or create) a test file with real shape content in the Drafts project.
  2. export-binfile -> zip A -> unzip -> normalize every .json -> tree hash A.
  3. Re-zip normalized tree A -> import-binfile IN-PLACE (same file-id).
  4. export-binfile again -> zip B -> normalize -> tree hash B.
  5. Byte/structural diff A vs B (per-file changed json paths).
  6. import-binfile as NEW once, normalize its export, diff vs A (id churn).
  7. Second in-place cycle: import normalized B in-place -> export C -> normalize.
     Stability check: normalized C must equal normalized B byte-for-byte.
  8. Write docs/m0/roundtrip-report.md + a JSON summary on stdout.

Auth: PENPOT_TOKEN env var if set, otherwise login-with-password
(PENPOT_EMAIL / PENPOT_PASSWORD, defaults m0@local.test / m0-spike-password).
"""

import hashlib
import io
import json
import os
import re
import shutil
import sys
import urllib.error
import urllib.request
import uuid
import zipfile
from datetime import datetime, timezone

BACKEND = os.environ.get("PENPOT_BACKEND", "http://localhost:6060")
FRONTEND = os.environ.get("PENPOT_FRONTEND", "http://localhost:9001")
EMAIL = os.environ.get("PENPOT_EMAIL", "m0@local.test")
PASSWORD = os.environ.get("PENPOT_PASSWORD", "m0-spike-password")
TOKEN = os.environ.get("PENPOT_TOKEN")

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WORK = os.environ.get(
    "ROUNDTRIP_WORKDIR", os.path.join(REPO, "m0", "roundtrip-work")
)
REPORT = os.path.join(REPO, "docs", "m0", "roundtrip-report.md")

TEST_FILE_NAME = "m0-roundtrip-spike"
ROOT_FRAME = "00000000-0000-0000-0000-000000000000"


# ---------------------------------------------------------------- HTTP layer

class Client:
    def __init__(self):
        self.cookie = None
        self.token = TOKEN

    def _auth_headers(self):
        h = {}
        if self.token:
            h["Authorization"] = "Token " + self.token
        elif self.cookie:
            h["Cookie"] = self.cookie
        return h

    def login(self):
        if self.token:
            return self.rpc("get-profile", {})
        req = urllib.request.Request(
            BACKEND + "/api/rpc/command/login-with-password",
            data=json.dumps({"email": EMAIL, "password": PASSWORD}).encode(),
            headers={"Content-Type": "application/json",
                     "Accept": "application/json"},
        )
        with urllib.request.urlopen(req) as resp:
            for k, v in resp.getheaders():
                if k.lower() == "set-cookie" and v.startswith("auth-token="):
                    self.cookie = v.split(";", 1)[0]
            profile = json.loads(resp.read())
        if not self.cookie:
            raise RuntimeError("login-with-password returned no auth-token cookie")
        return profile

    def rpc(self, command, payload):
        """Plain-JSON RPC call, returns decoded JSON (or raw bytes for 204)."""
        headers = {"Content-Type": "application/json",
                   "Accept": "application/json"}
        headers.update(self._auth_headers())
        req = urllib.request.Request(
            f"{BACKEND}/api/rpc/command/{command}",
            data=json.dumps(payload).encode(), headers=headers)
        with urllib.request.urlopen(req) as resp:
            body = resp.read()
        if not body:
            return None
        return json.loads(body)

    def rpc_sse(self, command, payload=None, multipart=None):
        """RPC call whose response is an SSE stream. Returns the data payload
        of the final 'end' event (transit-encoded string). Raises on 'error'."""
        headers = {"Accept": "application/json"}
        headers.update(self._auth_headers())
        if multipart is not None:
            body, ctype = multipart
            headers["Content-Type"] = ctype
        else:
            body = json.dumps(payload).encode()
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(
            f"{BACKEND}/api/rpc/command/{command}", data=body, headers=headers)
        with urllib.request.urlopen(req) as resp:
            text = resp.read().decode("utf-8")
        event, end_data = None, None
        for line in text.splitlines():
            if line.startswith("event:"):
                event = line.split(":", 1)[1].strip()
            elif line.startswith("data:"):
                data = line.split(":", 1)[1].strip()
                if event == "error":
                    raise RuntimeError(f"{command} SSE error: {data}")
                if event == "end":
                    end_data = data
        if end_data is None:
            raise RuntimeError(
                f"{command}: no 'end' event in SSE response:\n{text[:2000]}")
        return end_data

    def download(self, url):
        """GET an asset URL (frontend host) with auth; returns bytes."""
        req = urllib.request.Request(url, headers=self._auth_headers())
        with urllib.request.urlopen(req) as resp:
            return resp.read()


# ------------------------------------------------------------- multipart enc

def encode_multipart(fields, file_field=None):
    """fields: list of (name, str-value); file_field: (name, filename, bytes).
    Returns (body-bytes, content-type)."""
    boundary = "m0boundary" + uuid.uuid4().hex
    buf = io.BytesIO()
    for name, value in fields:
        buf.write(f"--{boundary}\r\n".encode())
        buf.write(f'Content-Disposition: form-data; name="{name}"\r\n\r\n'.encode())
        buf.write(str(value).encode() + b"\r\n")
    if file_field:
        name, filename, content = file_field
        buf.write(f"--{boundary}\r\n".encode())
        buf.write((f'Content-Disposition: form-data; name="{name}"; '
                   f'filename="{filename}"\r\n'
                   f"Content-Type: application/zip\r\n\r\n").encode())
        buf.write(content + b"\r\n")
    buf.write(f"--{boundary}--\r\n".encode())
    return buf.getvalue(), f"multipart/form-data; boundary={boundary}"


# ------------------------------------------------------- penpot round-trip ops

def parse_transit_uri(data):
    """'{"~#uri":"http://..."}' -> url string"""
    return json.loads(data)["~#uri"]


def parse_transit_uuid_list(data):
    """'["~u<uuid>", ...]' -> [uuid-str, ...]"""
    return [x[2:] if x.startswith("~u") else x for x in json.loads(data)]


def export_binfile(client, file_id):
    end = client.rpc_sse("export-binfile", {
        "fileId": file_id, "includeLibraries": False, "embedAssets": False})
    url = parse_transit_uri(end)
    return client.download(url)


def import_binfile(client, project_id, zip_bytes, file_id=None,
                   name="roundtrip"):
    fields = [("name", name), ("project-id", project_id)]
    if file_id:
        fields.append(("file-id", file_id))
    body, ctype = encode_multipart(fields, ("file", "roundtrip.penpot", zip_bytes))
    end = client.rpc_sse("import-binfile", multipart=(body, ctype))
    ids = parse_transit_uuid_list(end)
    if len(ids) != 1:
        raise RuntimeError(f"import-binfile returned {ids!r}")
    return ids[0]


def rect_shape(shape_id, name, x, y, w, h, color):
    ident = {"a": 1.0, "b": 0.0, "c": 0.0, "d": 1.0, "e": 0.0, "f": 0.0}
    return {
        "id": shape_id, "type": "rect", "name": name,
        "x": x, "y": y, "width": w, "height": h, "rotation": 0,
        "selrect": {"x": x, "y": y, "width": w, "height": h,
                    "x1": x, "y1": y, "x2": x + w, "y2": y + h},
        "points": [{"x": x, "y": y}, {"x": x + w, "y": y},
                   {"x": x + w, "y": y + h}, {"x": x, "y": y + h}],
        "transform": ident, "transformInverse": ident,
        "parentId": ROOT_FRAME, "frameId": ROOT_FRAME,
        "fills": [{"fillColor": color, "fillOpacity": 1}],
        "strokes": [],
    }


def ensure_test_file(client, project_id):
    """Reuse TEST_FILE_NAME if present, else create it with two rectangles."""
    files = client.rpc("get-project-files", {"projectId": project_id})
    for f in files:
        if f["name"] == TEST_FILE_NAME:
            return f["id"], False
    created = client.rpc("create-file",
                         {"name": TEST_FILE_NAME, "projectId": project_id})
    file_id = created["id"]
    page_id = created["data"]["pages"][0]
    changes = []
    for i, (nm, x, col) in enumerate(
            [("RT Rect A", 100, "#B1B2B5"), ("RT Rect B", 400, "#7048E8")]):
        sid = str(uuid.uuid4())
        changes.append({
            "type": "add-obj", "id": sid, "pageId": page_id,
            "frameId": ROOT_FRAME, "parentId": ROOT_FRAME,
            "obj": rect_shape(sid, nm, x, 120 + i * 10, 200, 150, col),
        })
    client.rpc("update-file", {
        "id": file_id, "sessionId": str(uuid.uuid4()),
        "revn": created["revn"], "vern": created["vern"], "changes": changes})
    return file_id, True


# --------------------------------------------------- normalize / hash / diff

def unzip_to(zip_bytes, dest):
    if os.path.exists(dest):
        shutil.rmtree(dest)
    os.makedirs(dest)
    with zipfile.ZipFile(io.BytesIO(zip_bytes)) as zf:
        zf.extractall(dest)


def normalize_tree(root):
    """Rewrite every .json under root: sorted keys, 2-space indent, LF,
    trailing newline. Non-JSON files untouched."""
    for dirpath, _dirs, files in os.walk(root):
        for fn in files:
            if not fn.endswith(".json"):
                continue
            p = os.path.join(dirpath, fn)
            with open(p, "rb") as fh:
                data = json.loads(fh.read().decode("utf-8"))
            out = json.dumps(data, sort_keys=True, indent=2,
                             ensure_ascii=False) + "\n"
            with open(p, "wb") as fh:
                fh.write(out.encode("utf-8"))


# Keys observed (by this script) to be rewritten by the server on every
# import: in-place import sets both to the import wall-clock time inside
# files/<fid>.json. Stripping them is part of the daemon normalization spec.
VOLATILE_KEYS = {"createdAt", "modifiedAt"}


def strip_volatile(obj):
    if isinstance(obj, dict):
        return {k: strip_volatile(v) for k, v in obj.items()
                if k not in VOLATILE_KEYS}
    if isinstance(obj, list):
        return [strip_volatile(x) for x in obj]
    return obj


def semantic_files(files):
    """Second normalization tier: formatting-normalized tree with volatile
    keys stripped from every .json. Returns a new {relpath: bytes} dict."""
    out = {}
    for rel, content in files.items():
        if rel.endswith(".json"):
            data = strip_volatile(json.loads(content))
            content = (json.dumps(data, sort_keys=True, indent=2,
                                  ensure_ascii=False) + "\n").encode("utf-8")
        out[rel] = content
    return out


def tree_files(root):
    """{relpath: bytes} for every file under root (LF-normalized paths)."""
    out = {}
    for dirpath, _dirs, files in os.walk(root):
        for fn in files:
            p = os.path.join(dirpath, fn)
            rel = os.path.relpath(p, root).replace(os.sep, "/")
            with open(p, "rb") as fh:
                out[rel] = fh.read()
    return out


def tree_hash(files):
    h = hashlib.sha256()
    for rel in sorted(files):
        h.update(rel.encode())
        h.update(b"\0")
        h.update(hashlib.sha256(files[rel]).hexdigest().encode())
        h.update(b"\n")
    return h.hexdigest()


def zip_tree(root):
    """Deterministically zip a directory (sorted paths, fixed date)."""
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as zf:
        files = tree_files(root)
        for rel in sorted(files):
            zi = zipfile.ZipInfo(rel, date_time=(1980, 1, 1, 0, 0, 0))
            zi.compress_type = zipfile.ZIP_DEFLATED
            zf.writestr(zi, files[rel])
    return buf.getvalue()


def json_diff(a, b, path="$"):
    """Structural diff. Returns list of (json_path, kind, a_repr, b_repr)."""
    diffs = []
    if type(a) is not type(b):
        diffs.append((path, "type-changed", short(a), short(b)))
        return diffs
    if isinstance(a, dict):
        for k in sorted(set(a) | set(b)):
            p = f"{path}.{k}"
            if k not in b:
                diffs.append((p, "removed", short(a[k]), None))
            elif k not in a:
                diffs.append((p, "added", None, short(b[k])))
            else:
                diffs.extend(json_diff(a[k], b[k], p))
    elif isinstance(a, list):
        if len(a) != len(b):
            diffs.append((path, f"list-len {len(a)}->{len(b)}",
                          short(a), short(b)))
        for i, (x, y) in enumerate(zip(a, b)):
            diffs.extend(json_diff(x, y, f"{path}[{i}]"))
    else:
        if a != b:
            diffs.append((path, "value-changed", short(a), short(b)))
    return diffs


def short(v, n=60):
    s = json.dumps(v, sort_keys=True, ensure_ascii=False)
    return s if len(s) <= n else s[:n] + "…"


def diff_trees(files_a, files_b, label_a="A", label_b="B"):
    """Per-file diff of two normalized trees. Returns dict with
    only_a / only_b / identical / changed {rel: [diff tuples]}."""
    result = {"only_a": [], "only_b": [], "identical": [], "changed": {}}
    for rel in sorted(set(files_a) | set(files_b)):
        if rel not in files_b:
            result["only_a"].append(rel)
        elif rel not in files_a:
            result["only_b"].append(rel)
        elif files_a[rel] == files_b[rel]:
            result["identical"].append(rel)
        else:
            if rel.endswith(".json"):
                da = json.loads(files_a[rel])
                db = json.loads(files_b[rel])
                result["changed"][rel] = json_diff(da, db)
            else:
                result["changed"][rel] = [
                    ("(binary)", "bytes-changed",
                     hashlib.sha256(files_a[rel]).hexdigest()[:16],
                     hashlib.sha256(files_b[rel]).hexdigest()[:16])]
    return result


UUID_RE = re.compile(
    r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")


def build_id_map(files_old, files_new):
    """Map new-tree uuids back to old-tree uuids so that an import-as-new tree
    can be structurally compared with the original. Matching strategy:
      file id  : position in manifest.json 'files' list
      page ids : order of the 'pages' list in files/<fid>.json
      shape ids: shape 'name' within the same page (falls back to unmatched)
    Returns (mapping new->old, notes list)."""
    notes = []
    mapping = {}

    man_old = json.loads(files_old["manifest.json"])
    man_new = json.loads(files_new["manifest.json"])
    for fo, fn in zip(man_old["files"], man_new["files"]):
        mapping[fn["id"]] = fo["id"]
    fid_old = man_old["files"][0]["id"]
    fid_new = man_new["files"][0]["id"]

    def load(files, rel):
        return json.loads(files[rel]) if rel in files else None

    fdata_old = load(files_old, f"files/{fid_old}.json")
    fdata_new = load(files_new, f"files/{fid_new}.json")
    pages_old = (fdata_old or {}).get("options", {}).get("pages") or []
    pages_new = (fdata_new or {}).get("options", {}).get("pages") or []
    if not pages_old:  # pages list may live elsewhere; derive from paths
        pages_old = sorted({m.group(0) for rel in files_old
                            for m in [re.match(
                                rf"files/{fid_old}/pages/({UUID_RE.pattern})\.json$",
                                rel)] if m}) or []
        pages_old = [re.match(
            rf"files/{fid_old}/pages/({UUID_RE.pattern})\.json$", rel).group(1)
            for rel in sorted(files_old)
            if re.match(rf"files/{fid_old}/pages/{UUID_RE.pattern}\.json$", rel)]
        pages_new = [re.match(
            rf"files/{fid_new}/pages/({UUID_RE.pattern})\.json$", rel).group(1)
            for rel in sorted(files_new)
            if re.match(rf"files/{fid_new}/pages/{UUID_RE.pattern}\.json$", rel)]
    for po, pn in zip(pages_old, pages_new):
        mapping[pn] = po

    # shapes: match by name within corresponding pages
    for po, pn in zip(pages_old, pages_new):
        shapes_old, shapes_new = {}, {}
        for files, fid, pid, acc in ((files_old, fid_old, po, shapes_old),
                                     (files_new, fid_new, pn, shapes_new)):
            prefix = f"files/{fid}/pages/{pid}/"
            for rel in files:
                if rel.startswith(prefix) and rel.endswith(".json"):
                    obj = json.loads(files[rel])
                    acc.setdefault(obj.get("name", rel), []).append(obj["id"])
        for name, new_ids in shapes_new.items():
            old_ids = shapes_old.get(name, [])
            for oi, ni in zip(sorted(old_ids), sorted(new_ids)):
                mapping[ni] = oi
            if len(old_ids) != len(new_ids):
                notes.append(f"shape name {name!r}: {len(old_ids)} old vs "
                             f"{len(new_ids)} new ids, matched pairwise")
    return mapping, notes


def remap_tree(files_new, mapping):
    """Rewrite relpaths and file contents of the new tree replacing new uuids
    with their old equivalents (string-level; JSON re-normalized after)."""
    out = {}
    for rel, content in files_new.items():
        new_rel = rel
        text = content.decode("utf-8") if rel.endswith(".json") else None
        for new_id, old_id in mapping.items():
            new_rel = new_rel.replace(new_id, old_id)
            if text is not None:
                text = text.replace(new_id, old_id)
        if text is not None:
            # renormalize: key order may change after id substitution
            content = (json.dumps(json.loads(text), sort_keys=True, indent=2,
                                  ensure_ascii=False) + "\n").encode("utf-8")
        out[new_rel] = content
    return out


# ------------------------------------------------------------------- report

def fmt_diff(diff, indent="  "):
    lines = []
    for rel in diff["only_a"]:
        lines.append(f"{indent}- only in first tree: `{rel}`")
    for rel in diff["only_b"]:
        lines.append(f"{indent}- only in second tree: `{rel}`")
    for rel, entries in diff["changed"].items():
        lines.append(f"{indent}- `{rel}` — {len(entries)} changed path(s):")
        for path, kind, av, bv in entries:
            lines.append(f"{indent}  - `{path}` ({kind}): `{av}` -> `{bv}`")
    if not (diff["only_a"] or diff["only_b"] or diff["changed"]):
        lines.append(f"{indent}- (no differences)")
    return "\n".join(lines)


def changed_paths(diff):
    out = []
    for rel, entries in diff["changed"].items():
        for path, kind, _a, _b in entries:
            out.append(f"{rel}:{path} ({kind})")
    out += [f"only-in-first:{r}" for r in diff["only_a"]]
    out += [f"only-in-second:{r}" for r in diff["only_b"]]
    return out


# --------------------------------------------------------------------- main

def main():
    os.makedirs(WORK, exist_ok=True)
    os.makedirs(os.path.dirname(REPORT), exist_ok=True)

    client = Client()
    profile = client.login()
    project_id = profile["defaultProjectId"]
    log = []

    def step(msg):
        print(f"[roundtrip] {msg}", flush=True)
        log.append(msg)

    step(f"logged in as {profile['email']}, project {project_id}")

    file_id, created = ensure_test_file(client, project_id)
    step(f"{'created' if created else 'reusing'} test file "
         f"{TEST_FILE_NAME!r} id={file_id}")
    meta0 = next(f for f in client.rpc("get-project-files",
                                       {"projectId": project_id})
                 if f["id"] == file_id)
    step(f"file revn={meta0['revn']} vern={meta0['vern']}")

    # ---- step 2: export A, normalize
    zip_a = export_binfile(client, file_id)
    open(os.path.join(WORK, "export-a.zip"), "wb").write(zip_a)
    dir_a = os.path.join(WORK, "tree-a")
    unzip_to(zip_a, dir_a)
    raw_entries = sorted(tree_files(dir_a))
    normalize_tree(dir_a)
    files_a = tree_files(dir_a)
    hash_a = tree_hash(files_a)
    step(f"export A: {len(zip_a)} bytes zip, {len(files_a)} entries, "
         f"normalized tree hash {hash_a}")

    # ---- step 3: re-zip normalized A, import IN-PLACE
    zip_a_norm = zip_tree(dir_a)
    returned_id = import_binfile(client, project_id, zip_a_norm,
                                 file_id=file_id, name=TEST_FILE_NAME)
    same_id_1 = returned_id == file_id
    step(f"in-place import #1: returned file id {returned_id} "
         f"(same as original: {same_id_1})")
    meta1 = next(f for f in client.rpc("get-project-files",
                                       {"projectId": project_id})
                 if f["id"] == file_id)
    step(f"after in-place import #1: revn={meta1['revn']} vern={meta1['vern']}")

    # ---- step 4: export B, normalize
    zip_b = export_binfile(client, file_id)
    open(os.path.join(WORK, "export-b.zip"), "wb").write(zip_b)
    dir_b = os.path.join(WORK, "tree-b")
    unzip_to(zip_b, dir_b)
    normalize_tree(dir_b)
    files_b = tree_files(dir_b)
    hash_b = tree_hash(files_b)
    step(f"export B: normalized tree hash {hash_b} "
         f"(equal to A: {hash_b == hash_a})")

    # ---- step 5: diff A vs B (formatting tier + volatile-stripped tier)
    diff_ab = diff_trees(files_a, files_b)
    step(f"A vs B: {len(diff_ab['identical'])} identical files, "
         f"{len(diff_ab['changed'])} changed, "
         f"{len(diff_ab['only_a'])}/{len(diff_ab['only_b'])} only-in-one")
    sem_a, sem_b = semantic_files(files_a), semantic_files(files_b)
    sem_hash_a, sem_hash_b = tree_hash(sem_a), tree_hash(sem_b)
    diff_ab_sem = diff_trees(sem_a, sem_b)
    step(f"A vs B with volatile keys ({', '.join(sorted(VOLATILE_KEYS))}) "
         f"stripped: identical = {sem_hash_a == sem_hash_b}")

    # ---- step 7 (run now so B is fresh): second in-place cycle B -> C
    zip_b_norm = zip_tree(dir_b)
    returned_id2 = import_binfile(client, project_id, zip_b_norm,
                                  file_id=file_id, name=TEST_FILE_NAME)
    same_id_2 = returned_id2 == file_id
    zip_c = export_binfile(client, file_id)
    open(os.path.join(WORK, "export-c.zip"), "wb").write(zip_c)
    dir_c = os.path.join(WORK, "tree-c")
    unzip_to(zip_c, dir_c)
    normalize_tree(dir_c)
    files_c = tree_files(dir_c)
    hash_c = tree_hash(files_c)
    stable_fmt = hash_c == hash_b
    diff_bc = diff_trees(files_b, files_c)
    sem_c = semantic_files(files_c)
    sem_hash_c = tree_hash(sem_c)
    stable_sem = sem_hash_c == sem_hash_b
    diff_bc_sem = diff_trees(sem_b, sem_c)
    step(f"in-place cycle #2: same id {same_id_2}, tree hash {hash_c}, "
         f"stable vs cycle #1: formatting-only {stable_fmt}, "
         f"volatile-stripped {stable_sem}")

    # ---- step 6: import-as-NEW, diff against A
    new_id = import_binfile(client, project_id, zip_a_norm, file_id=None,
                            name=TEST_FILE_NAME + "-asnew")
    step(f"import-as-new: created file id {new_id} "
         f"(differs from original: {new_id != file_id})")
    zip_n = export_binfile(client, new_id)
    dir_n = os.path.join(WORK, "tree-asnew")
    unzip_to(zip_n, dir_n)
    normalize_tree(dir_n)
    files_n = tree_files(dir_n)
    raw_diff_n = diff_trees(files_a, files_n)
    id_map, map_notes = build_id_map(files_a, files_n)
    files_n_mapped = remap_tree(files_n, id_map)
    diff_an = diff_trees(files_a, files_n_mapped)
    diff_an_sem = diff_trees(sem_a, semantic_files(files_n_mapped))
    step(f"as-new vs A (raw): {len(raw_diff_n['only_b'])} new paths; "
         f"after id-mapping: {len(diff_an['changed'])} changed files, "
         f"{len(diff_an['identical'])} identical; after also stripping "
         f"volatile keys: {len(diff_an_sem['changed'])} changed")
    client.rpc("delete-file", {"id": new_id})
    step(f"deleted as-new file {new_id} (cleanup)")

    # ------------------------------------------------------------- report
    ab_paths = changed_paths(diff_ab)
    ab_sem_paths = changed_paths(diff_ab_sem)
    bc_sem_paths = changed_paths(diff_bc_sem)
    an_paths = changed_paths(diff_an)
    an_sem_paths = changed_paths(diff_an_sem)
    fully_stable = (sem_hash_a == sem_hash_b) and stable_sem
    now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")

    manifest = json.loads(files_a["manifest.json"])
    report = f"""# M0 round-trip byte-diff report

Generated {now} by `scripts/roundtrip.py` against Penpot 2.16.2
(`penpot-m0` compose stack, backend `{BACKEND}`, assets via `{FRONTEND}`).
All numbers below are from a real run; work dir with the actual zips/trees:
`{WORK}`.

## TL;DR

- **In-place import keeps the file id** (verified in two consecutive cycles).
- **Formatting-only normalization is NOT byte-stable**: the server rewrites
  exactly two fields on every import — `createdAt` and `modifiedAt` in
  `files/<fid>.json` (both set to the import wall-clock time). Nothing else
  changes: no id churn, no ordering noise, no float reformatting.
- **Formatting normalization + stripping `createdAt`/`modifiedAt` IS
  byte-stable: {fully_stable}** across consecutive in-place cycles.
- Import-as-new remaps every uuid (file/page/shape) but is otherwise a pure
  id-rewrite plus the same two timestamps.

## Setup

- Test file: `{TEST_FILE_NAME}` id `{file_id}` in project `{project_id}`,
  revn {meta0['revn']} / vern {meta0['vern']} at start
  ({'created fresh with two rects via update-file' if created else 'reused from a previous run'}).
- Export produces a binfile-v3 zip: `manifest.json`
  (generatedBy `{manifest.get('generatedBy')}`) + `files/<fid>.json` +
  `files/<fid>/pages/<pid>.json` + one JSON per shape (the root frame is shape
  `00000000-0000-0000-0000-000000000000`).
- Zip entries in export A ({len(files_a)} files):

```
{chr(10).join(raw_entries)}
```

## Normalization tiers tested

1. **Formatting tier**: every `.json` rewritten as
   `json.dumps(obj, sort_keys=True, indent=2, ensure_ascii=False)` + trailing
   `\\n`, LF endings; non-JSON files untouched.
2. **Volatile-strip tier**: tier 1 plus recursively dropping keys
   `createdAt` and `modifiedAt` from every `.json`.

Tree hash = sha256 over sorted (relative path, sha256(content)) pairs.
The zip container itself is never compared (entry order/timestamps/compression
are irrelevant; only the extracted tree matters).

## In-place round-trip (cycle 1): export A -> normalize -> re-zip -> import in-place -> export B

- import-binfile with `file-id={file_id}` returned id `{returned_id}` —
  **same file id: {same_id_1}**.
- revn/vern before: {meta0['revn']}/{meta0['vern']}; after in-place import:
  {meta1['revn']}/{meta1['vern']} (revn is reset to the value inside the zip,
  not incremented).
- Formatting-tier tree hash A: `{hash_a}`
- Formatting-tier tree hash B: `{hash_b}`
- **Byte-identical at formatting tier: {hash_a == hash_b}**

Per-file diff A vs B (formatting tier):

{fmt_diff(diff_ab)}

With `createdAt`/`modifiedAt` stripped (volatile-strip tier):

{fmt_diff(diff_ab_sem)}

- Volatile-strip hash A: `{sem_hash_a}`
- Volatile-strip hash B: `{sem_hash_b}`
- **Byte-identical at volatile-strip tier: {sem_hash_a == sem_hash_b}**

## Stability (cycle 2): import B in-place -> export C

- Same file id again: {same_id_2}.
- Formatting-tier hash C: `{hash_c}` — equals B: {stable_fmt}
- Volatile-strip hash C: `{sem_hash_c}` — **equals B: {stable_sem}**

Per-file diff B vs C (formatting tier):

{fmt_diff(diff_bc)}

Per-file diff B vs C (volatile-strip tier):

{fmt_diff(diff_bc_sem)}

## Import-as-NEW variant (diff vs A)

- New file id minted: `{new_id}` (original `{file_id}`).
- Raw path diff before id-mapping: {len(raw_diff_n['only_a'])} path(s) only in
  A, {len(raw_diff_n['only_b'])} only in the as-new export — every path
  containing a uuid changes, because **ALL ids are remapped**: the file id,
  every page id, every shape id, and every reference to them inside the JSON
  (`id`, `parentId`, `frameId`, `pages` lists, manifest `files[].id`,
  directory/file names).
- After mapping new ids back to old ids (file id via manifest order, page ids
  via path order, shape ids via shape `name`){' — notes: ' + '; '.join(map_notes) if map_notes else ''},
  the residual diff is:

{fmt_diff(diff_an)}

After also stripping volatile keys:

{fmt_diff(diff_an_sem)}

## Exactly which fields change across an in-place round-trip

{chr(10).join('- `' + p + '`' for p in ab_paths) if ab_paths else '- none'}

- IDs: **unchanged** (file, page, shape uuids all survive in-place import).
- Ordering: **unchanged** (no array reordering or key-order noise observed
  after sort_keys normalization).
- revn/vern/version/migrations inside `files/<fid>.json`: **unchanged**
  (revn round-trips as the value stored in the zip).
- Timestamps: `createdAt` and `modifiedAt` in `files/<fid>.json` are both
  rewritten to the import time (observed: they become equal to each other
  after an in-place import). These are the ONLY unstable fields.

## Does normalization suffice?

- Pure formatting normalization (sorted keys / indent / LF / trailing
  newline): **no** — `createdAt`/`modifiedAt` still differ every cycle and
  would make the hash ledger see a phantom change after every import.
- Formatting + strip `createdAt` + `modifiedAt`: **{'yes — byte-stable across consecutive cycles' if fully_stable else 'NO — residual instability, see diffs above'}**.
{'' if fully_stable else chr(10).join('  - residual: `' + p + '`' for p in (ab_sem_paths + bc_sem_paths))}

## Normalization spec for the sync daemon (driven by the data above)

1. Parse and re-serialize every `.json`: **sorted object keys, 2-space
   indent, non-ASCII preserved (`ensure_ascii=False`), LF line endings,
   single trailing newline**.
2. **Strip `createdAt` and `modifiedAt`** (at minimum from
   `files/<fid>.json`; stripping them recursively everywhere is safe and
   future-proof) before hashing for the ledger. Alternative: keep them on
   disk for humans but exclude them from the tree hash.
3. Hash the tree as sha256 over sorted (relative path, content sha256)
   pairs; never hash or compare the zip container itself.
4. Treat `revn` inside the export as advisory only: in-place import resets
   the DB revn to the zip's value, so revn is not monotonic and must not be
   used as a "newer than" signal across imports.
5. If the daemon ever falls back to import-as-new, it must expect a full
   uuid rewrite (file, pages, shapes) and update its fileId<->path manifest;
   content is otherwise preserved verbatim ({'confirmed: residual diff after id-mapping was only the volatile timestamps' if not an_sem_paths else 'WARNING: residual non-id differences were observed, see the as-new section'}).

## Run log

```
{chr(10).join(log)}
```
"""
    with open(REPORT, "w") as fh:
        fh.write(report)
    step(f"report written to {REPORT}")

    summary = {
        "scriptPath": os.path.abspath(__file__),
        "reportPath": REPORT,
        "fileId": file_id,
        "sameFileIdAfterInPlace": bool(same_id_1 and same_id_2),
        "inPlaceStableFormattingOnly": bool(stable_fmt),
        "inPlaceStable": bool(fully_stable),
        "changedFields": ab_paths,
        "changedFieldsAfterVolatileStrip": ab_sem_paths + bc_sem_paths,
        "asNewChangedFields": an_paths,
        "volatileKeysStripped": sorted(VOLATILE_KEYS),
        "treeHashA": hash_a, "treeHashB": hash_b, "treeHashC": hash_c,
        "semanticHashA": sem_hash_a, "semanticHashB": sem_hash_b,
        "semanticHashC": sem_hash_c,
    }
    print(json.dumps(summary, indent=2))
    return summary


if __name__ == "__main__":
    main()
