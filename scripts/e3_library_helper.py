#!/usr/bin/env python3
"""E3 library-linking gate helper (scripts/e3-library.sh).

Reuses scripts/roundtrip.py as the Penpot RPC client (rt.Client) exactly as the
E3 de-risk probe (scripts/ecosystem-spike/e3_probe.py) did — the proven live
sequence this gate is seeded from. Every subcommand prints human PASS/FAIL lines
and/or a single trailing JSON line, and exits 0/1.

The three never-before-shipped library RPCs (`set-file-shared`,
`link-file-to-library`, `unlink-file-from-library`) plus `get-file-libraries`
are driven straight through rt.Client; the package verbs (publish/link) are the
apps/desktop `/__api/packages/*` routes the bash gate POSTs to directly.

Subcommands:

  add_library_content <base> <backend> <token> <lib_file_id>
      Author the library's contract INTO the installed library file (a first-
      class variant set + an exported color "Brand Teal" + a typography +
      a plain patch-target rect), via one update-file. Prints, on the last
      line, JSON {componentId, mainInstanceId, variantId, brandTealColorId,
      patchTargetId, pageId} — the ids the rest of the gate references.

  place_instance <base> <backend> <token> <cons_file_id> <lib_file_id>
                 <component_id> <main_instance_id>
      Place an INSTANCE of the library component into the consumer file (the
      proven wire: add-obj, one change, skipValidate, obj = normal shape +
      {componentId, componentFile:<libId>, componentRoot, shapeRef}). Asserts
      the instance's componentFile resolves to the library's vault-local id in
      the DB (get-file). Prints JSON {instanceId, componentFile}.

  wait_consumer_ondisk <base> <vault> <cons_file_id> <lib_file_id> <inst_id> <s>
      Poll until the sync daemon has exported the LINKED consumer to disk with
      the instance carrying componentFile=<libId>. This is the surgical-export
      barrier. Prints JSON {relPath, sig}.

  assert_consumer_singlefile <vault> <rel> <cons_file_id> <lib_file_id> <inst_id>
      Criterion 5: the linked consumer's on-disk `.penpot` tree is SINGLE-FILE
      (manifest.files == [consumer], relations == []), the instance still carries
      componentFile=<libId>, and the library is NOT inlined (no files/<libId>…).

  assert_reboot <base> <backend> <token> <cons_file_id> <lib_file_id> <inst_id> <s>
      Criteria 2 + 4: after a delete-DB + reboot, both files are live again under
      their ORIGINAL ids (get-file by id), the instance STILL resolves to <libId>,
      and file_library_rel is re-derived from the lockfile automatically by the
      boot re-link reconcile (get-file-libraries lists <libId> without any manual
      re-link). Polls up to <s> for the reconcile window.

  ondisk_sig <base> <vault> <file_id>
      Print JSON {relPath, sig} — sig = a raw content hash of the file's on-disk
      `.penpot` tree. The contract-diff anchor snapshotter.

  wait_ondisk_change <base> <vault> <file_id> <prev_sig> <s>
      Poll until the file's on-disk sig differs from <prev_sig>; print JSON
      {relPath, sig}. The edit->export barrier for the contract-diff channel.

  contract_edit <base> <backend> <token> <lib_file_id> <kind> [arg]
      Apply one edit to the library via update-file and print JSON
      {revnBefore, revnAfter}. kind in {patch, minor, major}:
        patch = mod-obj move the patch-target rect (impl only, arg=patchTargetId)
        minor = add-color "Extra Teal" (adds an exported asset)
        major = del-color <brandTealColorId> (removes an exported asset)

  unlink <base> <backend> <token> <cons_file_id> <lib_file_id>
      Criterion 6: unlink-file-from-library, then assert get-file-libraries is
      empty (relation cleared).

Stdlib only + scripts/roundtrip.py.
"""
import hashlib
import json
import os
import sys
import time
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)  # scripts/ for roundtrip.py

ROOT = "00000000-0000-0000-0000-000000000000"
IDENT = {"a": 1.0, "b": 0.0, "c": 0.0, "d": 1.0, "e": 0.0, "f": 0.0}


# ------------------------------------------------------------- rt client setup

def make_client(backend, base, token):
    import roundtrip as rt
    rt.BACKEND, rt.FRONTEND, rt.TOKEN = backend, base, token
    c = rt.Client()
    c.token = token
    return rt, c


# ------------------------------------------------------------------- shapes

def rect(sid, name, x, y, w=120, h=48, color="#4c6ef5", extra=None):
    s = {
        "id": sid, "type": "rect", "name": name,
        "x": x, "y": y, "width": w, "height": h, "rotation": 0,
        "selrect": {"x": x, "y": y, "width": w, "height": h,
                    "x1": x, "y1": y, "x2": x + w, "y2": y + h},
        "points": [{"x": x, "y": y}, {"x": x + w, "y": y},
                   {"x": x + w, "y": y + h}, {"x": x, "y": y + h}],
        "transform": IDENT, "transformInverse": IDENT,
        "parentId": ROOT, "frameId": ROOT,
        "fills": [{"fillColor": color, "fillOpacity": 1}], "strokes": [],
    }
    if extra:
        s.update(extra)
    return s


# ------------------------------------------------------------- HTTP for /boards

def http_get_json(url, timeout=60):
    import urllib.request
    req = urllib.request.Request(url, headers={"Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read())


def resolve_rel_path(base, file_id, wait_s=0):
    """Return the vault-relative .penpot path the daemon materialized file_id to,
    via /__api/vault/boards (fileId->relPath). Waits up to wait_s if needed."""
    deadline = time.time() + float(wait_s)
    while True:
        try:
            bd = http_get_json(base + "/__api/vault/boards")
            for c in bd.get("boards", []):
                if c.get("fileId") == file_id:
                    return c.get("relPath")
        except Exception:
            pass
        if time.time() >= deadline:
            return None
        time.sleep(2)


# --------------------------------------------------------- on-disk tree helpers

def tree_sig(root):
    """Raw content signature of an on-disk tree: sha256 over sorted
    (relpath, sha256(bytes))."""
    h = hashlib.sha256()
    entries = []
    for dp, _dirs, files in os.walk(root):
        for fn in files:
            p = os.path.join(dp, fn)
            rel = os.path.relpath(p, root).replace(os.sep, "/")
            with open(p, "rb") as fh:
                entries.append((rel, hashlib.sha256(fh.read()).hexdigest()))
    for rel, digest in sorted(entries):
        h.update(rel.encode())
        h.update(b"\0")
        h.update(digest.encode())
        h.update(b"\n")
    return h.hexdigest()


def read_manifest(tree_dir):
    with open(os.path.join(tree_dir, "manifest.json"), "rb") as fh:
        return json.loads(fh.read())


def find_shape_on_disk(tree_dir, shape_id):
    """Return the shape json dict whose id==shape_id, searching files/*/pages."""
    for dp, _dirs, files in os.walk(tree_dir):
        for fn in files:
            if not fn.endswith(".json"):
                continue
            p = os.path.join(dp, fn)
            try:
                with open(p, "rb") as fh:
                    obj = json.loads(fh.read())
            except Exception:
                continue
            if isinstance(obj, dict) and obj.get("id") == shape_id:
                return obj
    return None


# ------------------------------------------------------------------ commands

def add_library_content(base, backend, token, lib_file_id):
    rt, c = make_client(backend, base, token)
    meta = c.rpc("get-file", {"id": lib_file_id})
    page_id = next(iter(meta["data"]["pagesIndex"].keys()))

    comp_id = str(uuid.uuid4())
    variant_id = str(uuid.uuid4())
    main_inst = str(uuid.uuid4())
    brand_teal_id = str(uuid.uuid4())
    typo_id = str(uuid.uuid4())
    patch_target = str(uuid.uuid4())

    changes = [
        {"type": "add-obj", "id": main_inst, "pageId": page_id,
         "frameId": ROOT, "parentId": ROOT,
         "obj": rect(main_inst, "Button / Primary", 0, 0, extra={
             "componentId": comp_id, "componentFile": lib_file_id,
             "componentRoot": True, "mainInstance": True,
             "variantId": variant_id})},
        {"type": "add-component", "id": comp_id, "name": "Primary",
         "path": "Controls / Button",
         "mainInstanceId": main_inst, "mainInstancePage": page_id,
         "variantId": variant_id,
         "variantProperties": [{"name": "State", "value": "Default"}]},
        {"type": "add-color", "color": {
            "id": brand_teal_id, "name": "Brand Teal",
            "color": "#12b886", "opacity": 1}},
        {"type": "add-typography", "typography": {
            "id": typo_id, "name": "Heading XL",
            "fontId": "sourcesanspro", "fontFamily": "sourcesanspro",
            "fontVariantId": "bold", "fontSize": "36", "fontWeight": "700",
            "fontStyle": "normal", "lineHeight": "1.2",
            "letterSpacing": "0", "textTransform": "none"}},
        # a plain, contract-neutral rect used as the patch-edit target
        {"type": "add-obj", "id": patch_target, "pageId": page_id,
         "frameId": ROOT, "parentId": ROOT,
         "obj": rect(patch_target, "Patch Target", 300, 0, color="#adb5bd")},
    ]
    c.rpc("update-file", {
        "id": lib_file_id, "sessionId": str(uuid.uuid4()),
        "revn": meta["revn"], "vern": meta["vern"],
        "changes": changes, "skipValidate": True})

    print("PASS: authored library contract (variant set + color + typography) "
          f"into {lib_file_id}")
    print(json.dumps({
        "componentId": comp_id, "mainInstanceId": main_inst,
        "variantId": variant_id, "brandTealColorId": brand_teal_id,
        "patchTargetId": patch_target, "pageId": page_id}))
    return 0


def place_instance(base, backend, token, cons_id, lib_id, component_id, main_inst):
    rt, c = make_client(backend, base, token)
    cons = c.rpc("get-file", {"id": cons_id})
    page_id = next(iter(cons["data"]["pagesIndex"].keys()))
    inst_id = str(uuid.uuid4())
    changes = [
        {"type": "add-obj", "id": inst_id, "pageId": page_id,
         "frameId": ROOT, "parentId": ROOT,
         "obj": rect(inst_id, "Button / Primary (instance)", 40, 200, extra={
             "componentId": component_id,
             "componentFile": lib_id,          # the crux: library's file-id
             "componentRoot": True,
             "shapeRef": main_inst})},
    ]
    c.rpc("update-file", {
        "id": cons_id, "sessionId": str(uuid.uuid4()),
        "revn": cons["revn"], "vern": cons["vern"],
        "changes": changes, "skipValidate": True})

    back = c.rpc("get-file", {"id": cons_id})
    inst = None
    for page in back["data"]["pagesIndex"].values():
        if inst_id in page.get("objects", {}):
            inst = page["objects"][inst_id]
            break
    cf = inst.get("componentFile") if inst else None
    ok = cf == lib_id
    print(("PASS: " if ok else "FAIL: ")
          + f"instance componentFile resolves to the library vault-local id "
            f"(componentFile={cf}, libId={lib_id})")
    print(json.dumps({"instanceId": inst_id, "componentFile": cf}))
    return 0 if ok else 1


def wait_consumer_ondisk(base, vault, cons_id, lib_id, inst_id, wait_s):
    deadline = time.time() + float(wait_s)
    while True:
        rel = resolve_rel_path(base, cons_id, 0)
        if rel:
            tree = os.path.join(vault, rel)
            shape = find_shape_on_disk(tree, inst_id)
            if shape is not None and shape.get("componentFile") == lib_id:
                print("PASS: sync daemon exported the linked consumer to disk "
                      "with the instance (surgical export barrier)")
                print(json.dumps({"relPath": rel, "sig": tree_sig(tree)}))
                return 0
        if time.time() >= deadline:
            print(f"FAIL: consumer instance never appeared on disk within {wait_s}s "
                  f"(rel={rel})")
            return 1
        time.sleep(2)


def assert_consumer_singlefile(vault, rel, cons_id, lib_id, inst_id):
    ok = True

    def check(cond, msg):
        nonlocal ok
        print(("PASS: " if cond else "FAIL: ") + msg)
        ok = ok and cond

    tree = os.path.join(vault, rel)
    man = read_manifest(tree)
    file_ids = [f.get("id") for f in man.get("files", [])]
    check(file_ids == [cons_id],
          f"consumer tree is single-file on disk (manifest.files={file_ids})")
    rels = man.get("relations") or []
    check(rels == [],
          f"manifest.relations is empty — include-libraries NOT used as storage "
          f"(relations={rels})")

    shape = find_shape_on_disk(tree, inst_id)
    cf = shape.get("componentFile") if shape else None
    check(cf == lib_id,
          f"on-disk instance carries componentFile=<libId> as a bare id "
          f"(componentFile={cf})")

    # the library must NOT be inlined into the consumer tree
    inlined = os.path.exists(os.path.join(tree, "files", f"{lib_id}.json")) or \
        os.path.isdir(os.path.join(tree, "files", lib_id))
    check(not inlined,
          "library file is NOT inlined into the consumer tree "
          "(no files/<libId> subtree)")
    return 0 if ok else 1


def assert_reboot(base, backend, token, cons_id, lib_id, inst_id, wait_s):
    ok = True

    def check(cond, msg):
        nonlocal ok
        print(("PASS: " if cond else "FAIL: ") + msg)
        ok = ok and cond

    rt, c = make_client(backend, base, token)

    # both files resurrected under their ORIGINAL ids (get-file by id).
    deadline = time.time() + float(wait_s)
    lib_live = cons_live = False
    while time.time() < deadline:
        lib_live = _rpc_ok(c, "get-file", {"id": lib_id})
        cons_live = _rpc_ok(c, "get-file", {"id": cons_id})
        if lib_live and cons_live:
            break
        time.sleep(2)
    check(lib_live, f"library resurrected under its ORIGINAL id {lib_id} (M2)")
    check(cons_live, f"consumer resurrected under its ORIGINAL id {cons_id} (M2)")

    # instance STILL resolves to the library id after the wipe.
    cf = None
    if cons_live:
        back = c.rpc("get-file", {"id": cons_id})
        for page in back["data"]["pagesIndex"].values():
            if inst_id in page.get("objects", {}):
                cf = page["objects"][inst_id].get("componentFile")
                break
    check(cf == lib_id,
          f"instance STILL resolves to the library id after delete-DB+reboot "
          f"(componentFile={cf})")

    # file_library_rel re-derived from the lockfile by the boot reconcile,
    # WITHOUT any manual re-link (poll the reconcile window).
    rederived = False
    while time.time() < deadline:
        try:
            libs = c.rpc("get-file-libraries", {"fileId": cons_id})
            if any(l.get("id") == lib_id for l in (libs or [])):
                rederived = True
                break
        except Exception:
            pass
        time.sleep(2)
    check(rederived,
          "file_library_rel re-derived from the lockfile on rebuild "
          "(get-file-libraries lists the library, no manual re-link)")
    return 0 if ok else 1


def ondisk_sig(base, vault, file_id):
    rel = resolve_rel_path(base, file_id, 0)
    if not rel:
        print("FAIL: could not resolve on-disk path")
        return 1
    print(json.dumps({"relPath": rel, "sig": tree_sig(os.path.join(vault, rel))}))
    return 0


def wait_ondisk_change(base, vault, file_id, prev_sig, wait_s):
    deadline = time.time() + float(wait_s)
    while True:
        rel = resolve_rel_path(base, file_id, 0)
        if rel:
            sig = tree_sig(os.path.join(vault, rel))
            if sig != prev_sig:
                print(json.dumps({"relPath": rel, "sig": sig}))
                return 0
        if time.time() >= deadline:
            print(json.dumps({"relPath": rel, "sig": None, "error": "timeout"}))
            return 1
        time.sleep(2)


def contract_edit(base, backend, token, lib_id, kind, arg=None):
    rt, c = make_client(backend, base, token)
    meta = c.rpc("get-file", {"id": lib_id})
    page_id = next(iter(meta["data"]["pagesIndex"].keys()))
    revn_before = meta["revn"]

    if kind == "patch":
        changes = [{"type": "mod-obj", "id": arg, "pageId": page_id,
                    "operations": [{"type": "set", "attr": "x", "val": 321}]}]
    elif kind == "minor":
        changes = [{"type": "add-color", "color": {
            "id": str(uuid.uuid4()), "name": "Extra Teal",
            "color": "#0ca678", "opacity": 1}}]
    elif kind == "major":
        changes = [{"type": "del-color", "id": arg}]
    else:
        print(f"FAIL: unknown contract_edit kind {kind!r}")
        return 1

    c.rpc("update-file", {
        "id": lib_id, "sessionId": str(uuid.uuid4()),
        "revn": revn_before, "vern": meta["vern"],
        "changes": changes, "skipValidate": True})
    after = c.rpc("get-file", {"id": lib_id})
    print(json.dumps({"revnBefore": revn_before, "revnAfter": after["revn"]}))
    return 0


def unlink(base, backend, token, cons_id, lib_id):
    ok = True

    def check(cond, msg):
        nonlocal ok
        print(("PASS: " if cond else "FAIL: ") + msg)
        ok = ok and cond

    rt, c = make_client(backend, base, token)
    # pre-condition: the relation exists.
    before = c.rpc("get-file-libraries", {"fileId": cons_id}) or []
    check(any(l.get("id") == lib_id for l in before),
          "relation present before unlink (get-file-libraries lists the library)")
    c.rpc("unlink-file-from-library", {"fileId": cons_id, "libraryId": lib_id})
    after = c.rpc("get-file-libraries", {"fileId": cons_id}) or []
    check(not any(l.get("id") == lib_id for l in after),
          f"unlink cleared the relation (get-file-libraries now {after})")
    return 0 if ok else 1


def _rpc_ok(client, cmd, payload):
    try:
        client.rpc(cmd, payload)
        return True
    except Exception:
        return False


def main():
    if len(sys.argv) < 2:
        print("usage: e3_library_helper.py <subcommand> ...", file=sys.stderr)
        return 2
    cmd, a = sys.argv[1], sys.argv[2:]
    if cmd == "add_library_content":
        return add_library_content(a[0], a[1], a[2], a[3])
    if cmd == "place_instance":
        return place_instance(a[0], a[1], a[2], a[3], a[4], a[5], a[6])
    if cmd == "wait_consumer_ondisk":
        return wait_consumer_ondisk(a[0], a[1], a[2], a[3], a[4], a[5])
    if cmd == "assert_consumer_singlefile":
        return assert_consumer_singlefile(a[0], a[1], a[2], a[3], a[4])
    if cmd == "assert_reboot":
        return assert_reboot(a[0], a[1], a[2], a[3], a[4], a[5], a[6])
    if cmd == "ondisk_sig":
        return ondisk_sig(a[0], a[1], a[2])
    if cmd == "wait_ondisk_change":
        return wait_ondisk_change(a[0], a[1], a[2], a[3], a[4])
    if cmd == "contract_edit":
        return contract_edit(a[0], a[1], a[2], a[3], a[4],
                             a[5] if len(a) > 5 else None)
    if cmd == "unlink":
        return unlink(a[0], a[1], a[2], a[3], a[4])
    print(f"unknown subcommand {cmd!r}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
