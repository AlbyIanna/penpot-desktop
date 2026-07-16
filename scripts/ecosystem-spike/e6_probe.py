#!/usr/bin/env python3
"""E6 live-probe helper (driven by scripts/e6-library-portability-spike.sh).

Reuses scripts/roundtrip.py as the RPC client (the M0-proven wire) and
scripts/ecosystem-spike/e6_rewrite.py for the static extraction/verify halves,
so the live captures and the offline rewrite tool share one definition of
"component identity" and "zero dangling".

Subcommands (each prints PASS/FAIL lines and/or one trailing JSON line):

  author_sources <base> <backend> <token> <out_dir>
      Author the package SOURCE trees in the current (scratch) vault: a
      component-library file (variant-set component "Controls / Button" whose
      main instance is a FRAME with two children — the nested-shapeRef
      fixture — plus a plain component "Badge", an exported color and a
      typography) and an empty consumer file. Exports both to
      <out_dir>/e6-button-kit.penpot and <out_dir>/e6-app.penpot and writes
      <out_dir>/source-ids.json with the source-side ids per E1 key.

  capture_lib <base> <backend> <token> <vault> <file_id> <copy_dest> <wait_s>
      Wait for the installed library file to land on disk, copy its tree to
      <copy_dest>, and print the identity capture {fileId, relPath,
      components: [{key, componentId, mainInstanceId, subtreeShapeIds}],
      colors: [{key, id}], typographies: [{key, id}]} extracted STATICALLY
      from the copied tree (e6_rewrite.extract_library).

  place_instances <base> <backend> <token> <cons_file_id> <libcap_json>
      Place TWO instances plus TWO library-STYLED shapes into the consumer:
      (1) an E3-style ROOT-ONLY instance of "Badge" (rect copy: componentId+
      componentFile+componentRoot+shapeRef); (2) a NESTED SUBTREE instance of
      "Controls / Button" (a copied frame whose root carries the component
      trio and whose children each carry a bare shapeRef to the corresponding
      library child shape); (3) a rect whose fill AND stroke carry the
      library color (fillColorRefId/fillColorRefFile + strokeColorRefId/
      strokeColorRefFile); (4) a text shape whose content nodes (paragraph +
      span) carry the library typography (typographyRefId/typographyRefFile)
      and whose span fill carries the library color. (3)+(4) exercise the
      ASSET-ref rewrite path live end-to-end. Prints {rootInstanceId,
      nestedInstanceId, nestedChildIds, styledRectId, styledTextId}.

  wait_ondisk <base> <vault> <file_id> <lib_file_id> <inst_id> <wait_s>
      Poll until the sync daemon exported <file_id> to disk with shape
      <inst_id> carrying componentFile=<lib_file_id> (the surgical-export
      barrier). Prints {relPath}.

  find_file_by_rel <vault> <rel_path> <wait_s>
      Poll the vault's sync manifest (.penpot-sync.json) until <rel_path>
      appears; print {fileId} — the DB-authoritative id the daemon minted
      (the boards index is disk-derived and reports a hand-dropped tree's
      embedded foreign id until the daemon re-exports).

  add_lock_entry <vault> <entry_id> <file_id> <name> <lib_pkg_id>
                 <lib_file_id> <lib_version> <lib_contract_hash>
      Add a lock.json entry for a CARRIED consumer file: pins its fileId and
      its library link so (a) the sync daemon uses the E3 surgical export and
      (b) the boot re-link reconcile re-derives file_library_rel after a wipe.

  link <base> <backend> <token> <cons_file_id> <lib_file_id>
      Raw idempotent link-file-to-library + get-file-libraries witness.

  nudge <base> <backend> <token> <file_id> <shape_id> <x>
      One mod-obj (set x) — forces the daemon to re-export the file.

  verify_live <base> <backend> <token> <cons_file_id> <lib_file_id>
              [<wait_s>] [--expect-relink]
      The ZERO-DANGLING check over RPC: get-file(consumer) → every shape
      carrying componentFile must equal <lib_file_id>; every componentId must
      be a live component in get-file(library).data.components; every shapeRef
      (root or nested) must be a real object in the library's pages; every
      ASSET ref (fillColorRefFile/Id, strokeColorRefFile/Id,
      typographyRefFile/Id — enumerated recursively through fills, strokes
      and text content nodes) must point at <lib_file_id> and a live color/
      typography in the library's data.colors / data.typographies. With
      --expect-relink also polls get-file-libraries until the relation lists
      the library (the lockfile boot re-link witness). Retries until <wait_s>.

Env: none required (args carry base/backend/token). Stdlib only.
"""
import json
import os
import sys
import time
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(HERE, ".."))  # scripts/ for roundtrip.py

import e6_rewrite as e6  # noqa: E402
import roundtrip as rt  # noqa: E402

ROOT = "00000000-0000-0000-0000-000000000000"
IDENT = {"a": 1.0, "b": 0.0, "c": 0.0, "d": 1.0, "e": 0.0, "f": 0.0}


def make_client(base, backend, token):
    rt.BACKEND, rt.FRONTEND, rt.TOKEN = backend, base, token
    c = rt.Client()
    c.token = token
    return c


def geometry(x, y, w, h):
    return {
        "x": x, "y": y, "width": w, "height": h, "rotation": 0,
        "selrect": {"x": x, "y": y, "width": w, "height": h,
                    "x1": x, "y1": y, "x2": x + w, "y2": y + h},
        "points": [{"x": x, "y": y}, {"x": x + w, "y": y},
                   {"x": x + w, "y": y + h}, {"x": x, "y": y + h}],
        "transform": IDENT, "transformInverse": IDENT,
    }


def rect(sid, name, x, y, w=120, h=48, color="#4c6ef5", parent=ROOT,
         frame=ROOT, extra=None):
    s = {"id": sid, "type": "rect", "name": name,
         "parentId": parent, "frameId": frame,
         "fills": [{"fillColor": color, "fillOpacity": 1}], "strokes": []}
    s.update(geometry(x, y, w, h))
    if extra:
        s.update(extra)
    return s


def frame(sid, name, x, y, w=160, h=64, parent=ROOT, frm=ROOT, extra=None):
    s = {"id": sid, "type": "frame", "name": name,
         "parentId": parent, "frameId": frm,
         "fills": [{"fillColor": "#ffffff", "fillOpacity": 1}],
         "strokes": [], "shapes": []}
    s.update(geometry(x, y, w, h))
    if extra:
        s.update(extra)
    return s


def update_file(client, file_id, changes):
    meta = client.rpc("get-file", {"id": file_id})
    client.rpc("update-file", {
        "id": file_id, "sessionId": str(uuid.uuid4()),
        "revn": meta["revn"], "vern": meta["vern"],
        "changes": changes, "skipValidate": True})
    return meta


def export_tree(client, file_id, dest, *, embed=True, include_libs=False):
    end = client.rpc_sse("export-binfile", {
        "fileId": file_id, "includeLibraries": include_libs,
        "embedAssets": embed})
    zip_bytes = client.download(rt.parse_transit_uri(end))
    rt.unzip_to(zip_bytes, dest)
    rt.normalize_tree(dest)


def http_get_json(url, timeout=60):
    import urllib.request
    req = urllib.request.Request(url, headers={"Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read())


def resolve_rel_path(base, file_id):
    try:
        bd = http_get_json(base + "/__api/vault/boards")
        for c in bd.get("boards", []):
            if c.get("fileId") == file_id:
                return c.get("relPath")
    except Exception:
        pass
    return None


def find_shape_on_disk(tree_dir, shape_id):
    for dp, _dirs, files in os.walk(tree_dir):
        for fn in files:
            if not fn.endswith(".json"):
                continue
            try:
                with open(os.path.join(dp, fn), "rb") as fh:
                    obj = json.loads(fh.read())
            except Exception:
                continue
            if isinstance(obj, dict) and obj.get("id") == shape_id:
                return obj
    return None


# ----------------------------------------------------------- author_sources


def author_sources(base, backend, token, out_dir):
    os.makedirs(out_dir, exist_ok=True)
    c = make_client(base, backend, token)
    prof = c.rpc("get-profile", {})
    project_id = prof["defaultProjectId"]

    # ---- library source file ------------------------------------------
    lib = c.rpc("create-file", {"name": "e6-button-kit-src",
                                "projectId": project_id})
    lib_id = lib["id"]
    page = lib["data"]["pages"][0]

    comp_btn = str(uuid.uuid4())
    variant_id = str(uuid.uuid4())
    btn_frame = str(uuid.uuid4())
    btn_bg = str(uuid.uuid4())
    btn_icon = str(uuid.uuid4())
    comp_badge = str(uuid.uuid4())
    badge_main = str(uuid.uuid4())

    changes = [
        # Button: FRAME main instance with two children (nested fixture)
        {"type": "add-obj", "id": btn_frame, "pageId": page,
         "frameId": ROOT, "parentId": ROOT,
         "obj": frame(btn_frame, "Button", 0, 0, extra={
             "componentId": comp_btn, "componentFile": lib_id,
             "componentRoot": True, "mainInstance": True,
             "variantId": variant_id})},
        {"type": "add-obj", "id": btn_bg, "pageId": page,
         "frameId": btn_frame, "parentId": btn_frame,
         "obj": rect(btn_bg, "BG", 4, 4, 152, 56, "#4c6ef5",
                     parent=btn_frame, frame=btn_frame)},
        {"type": "add-obj", "id": btn_icon, "pageId": page,
         "frameId": btn_frame, "parentId": btn_frame,
         "obj": rect(btn_icon, "Icon", 12, 16, 32, 32, "#12b886",
                     parent=btn_frame, frame=btn_frame)},
        {"type": "add-component", "id": comp_btn, "name": "Primary",
         "path": "Controls / Button",
         "mainInstanceId": btn_frame, "mainInstancePage": page,
         "variantId": variant_id,
         "variantProperties": [{"name": "State", "value": "Default"}]},
        # Badge: plain rect component (the E3-style root-only fixture)
        {"type": "add-obj", "id": badge_main, "pageId": page,
         "frameId": ROOT, "parentId": ROOT,
         "obj": rect(badge_main, "Badge", 300, 0, 64, 24, "#fa5252", extra={
             "componentId": comp_badge, "componentFile": lib_id,
             "componentRoot": True, "mainInstance": True})},
        {"type": "add-component", "id": comp_badge, "name": "Badge",
         "path": "Controls",
         "mainInstanceId": badge_main, "mainInstancePage": page},
        # exported assets (library-contract richness)
        {"type": "add-color", "color": {
            "id": str(uuid.uuid4()), "name": "Brand Teal",
            "color": "#12b886", "opacity": 1}},
        {"type": "add-typography", "typography": {
            "id": str(uuid.uuid4()), "name": "Heading XL",
            "fontId": "sourcesanspro", "fontFamily": "sourcesanspro",
            "fontVariantId": "bold", "fontSize": "36", "fontWeight": "700",
            "fontStyle": "normal", "lineHeight": "1.2",
            "letterSpacing": "0", "textTransform": "none"}},
    ]
    c.rpc("update-file", {
        "id": lib_id, "sessionId": str(uuid.uuid4()),
        "revn": lib["revn"], "vern": lib["vern"],
        "changes": changes, "skipValidate": True})

    lib_tree = os.path.join(out_dir, "e6-button-kit.penpot")
    export_tree(c, lib_id, lib_tree)

    # ---- consumer source file (empty) ----------------------------------
    cons = c.rpc("create-file", {"name": "e6-app-src",
                                 "projectId": project_id})
    cons_tree = os.path.join(out_dir, "e6-app.penpot")
    export_tree(c, cons["id"], cons_tree)

    # ---- capture the SOURCE-side identity (static, from the tree) ------
    libx = e6.extract_library(lib_tree)
    comps = []
    for key, comp in sorted(libx["components"].items()):
        sub = e6.subtree_in_order(libx["shapes"], comp.get("mainInstanceId"))
        comps.append({"key": json.loads(key), "componentId": comp["id"],
                      "mainInstanceId": comp.get("mainInstanceId"),
                      "subtreeShapeIds": [s["id"] for _, s in sub],
                      "subtreeNames": [s.get("name") for _, s in sub]})
    src = {"sourceLibFileId": libx["fileId"],
           "sourceConsumerFileId": cons["id"], "components": comps}
    with open(os.path.join(out_dir, "source-ids.json"), "w") as fh:
        json.dump(src, fh, indent=2, sort_keys=True)
    print("PASS: authored + exported the package source trees "
          f"(lib components: {len(comps)})")
    print(json.dumps(src, sort_keys=True))
    return 0


# -------------------------------------------------------------- capture_lib


def capture_lib(base, backend, token, vault, file_id, copy_dest, wait_s):
    import shutil
    deadline = time.time() + float(wait_s)
    rel = None
    while time.time() < deadline:
        rel = resolve_rel_path(base, file_id)
        if rel:
            tree = os.path.join(vault, rel)
            # tree is complete when its components dir exists on disk
            if os.path.isdir(os.path.join(tree, "files", file_id,
                                          "components")):
                break
        time.sleep(2)
    else:
        print(f"FAIL: library {file_id} never landed on disk within {wait_s}s")
        return 1
    if os.path.exists(copy_dest):
        shutil.rmtree(copy_dest)
    shutil.copytree(os.path.join(vault, rel), copy_dest)
    libx = e6.extract_library(copy_dest)
    comps = []
    for key, comp in sorted(libx["components"].items()):
        sub = e6.subtree_in_order(libx["shapes"], comp.get("mainInstanceId"))
        comps.append({"key": json.loads(key), "componentId": comp["id"],
                      "mainInstanceId": comp.get("mainInstanceId"),
                      "subtreeShapeIds": [s["id"] for _, s in sub],
                      "subtreeNames": [s.get("name") for _, s in sub]})
    colors = [{"key": json.loads(k), "id": c["id"]}
              for k, c in sorted(libx["colors"].items())]
    typos = [{"key": json.loads(k), "id": t["id"]}
             for k, t in sorted(libx["typographies"].items())]
    out = {"fileId": libx["fileId"], "relPath": rel, "components": comps,
           "colors": colors, "typographies": typos}
    print("PASS: captured materialized library identity "
          f"({len(comps)} components, {len(colors)} colors, "
          f"{len(typos)} typographies) from {rel}")
    print(json.dumps(out, sort_keys=True))
    return 0


# ---------------------------------------------------------- place_instances


def text_shape(sid, name, x, y, w, h, typo_id, typo_file, color_id,
               color_file, parent=ROOT, frame=ROOT):
    """A schema-valid text shape whose CONTENT nodes (paragraph + span) carry
    the library typography ref and whose span fill carries the library color
    ref — the asset-ref fixture (binfile import validates shapes, so the
    content must be structurally correct: root > paragraph-set > paragraph >
    span)."""
    font = {"fontId": "sourcesanspro", "fontFamily": "sourcesanspro",
            "fontVariantId": "bold", "fontSize": "36", "fontWeight": "700",
            "fontStyle": "normal", "textTransform": "none",
            "textDecoration": "none", "letterSpacing": "0"}
    span = dict(font)
    span.update({"text": "Styled by Heading XL",
                 "typographyRefId": typo_id, "typographyRefFile": typo_file,
                 "fills": [{"fillColor": "#000000", "fillOpacity": 1,
                            "fillColorRefId": color_id,
                            "fillColorRefFile": color_file}]})
    para = dict(font)
    para.update({"type": "paragraph",
                 "typographyRefId": typo_id, "typographyRefFile": typo_file,
                 "textAlign": "left", "lineHeight": "1.2",
                 "children": [span]})
    s = {"id": sid, "type": "text", "name": name,
         "parentId": parent, "frameId": frame,
         "growType": "auto-width", "fills": [], "strokes": [],
         "content": {"type": "root", "verticalAlign": "top",
                     "children": [{"type": "paragraph-set",
                                   "children": [para]}]}}
    s.update(geometry(x, y, w, h))
    return s


def place_instances(base, backend, token, cons_id, libcap_path):
    with open(libcap_path) as fh:
        cap = json.load(fh)
    lib_id = cap["fileId"]
    by_name = {c["key"]["name"]: c for c in cap["components"]}
    badge = by_name["Badge"]
    button = by_name["Primary"]
    color = {c["key"]["name"]: c for c in cap["colors"]}["Brand Teal"]
    typo = {t["key"]["name"]: t for t in cap["typographies"]}["Heading XL"]

    c = make_client(base, backend, token)
    cons = c.rpc("get-file", {"id": cons_id})
    page = next(iter(cons["data"]["pagesIndex"].keys()))

    root_inst = str(uuid.uuid4())
    nested_root = str(uuid.uuid4())
    styled_rect = str(uuid.uuid4())
    styled_text = str(uuid.uuid4())
    nested_children = [str(uuid.uuid4())
                       for _ in button["subtreeShapeIds"][1:]]

    changes = [
        # (1) E3-style root-only instance of Badge
        {"type": "add-obj", "id": root_inst, "pageId": page,
         "frameId": ROOT, "parentId": ROOT,
         "obj": rect(root_inst, "Badge (instance)", 40, 300, 64, 24,
                     "#fa5252", extra={
                         "componentId": badge["componentId"],
                         "componentFile": lib_id,
                         "componentRoot": True,
                         "shapeRef": badge["mainInstanceId"]})},
        # (2) NESTED subtree instance of Button: copied frame + children,
        # children carry bare shapeRefs to the corresponding library shapes
        {"type": "add-obj", "id": nested_root, "pageId": page,
         "frameId": ROOT, "parentId": ROOT,
         "obj": frame(nested_root, "Button (instance)", 40, 400, extra={
             "componentId": button["componentId"],
             "componentFile": lib_id,
             "componentRoot": True,
             "shapeRef": button["mainInstanceId"]})},
    ]
    child_names = button["subtreeNames"][1:]
    child_geo = [(44, 404, 152, 56, "#4c6ef5"), (52, 416, 32, 32, "#12b886")]
    for i, (cid, ref) in enumerate(
            zip(nested_children, button["subtreeShapeIds"][1:])):
        x, y, w, h, col = child_geo[i % len(child_geo)]
        changes.append(
            {"type": "add-obj", "id": cid, "pageId": page,
             "frameId": nested_root, "parentId": nested_root,
             "obj": rect(cid, child_names[i], x, y, w, h, col,
                         parent=nested_root, frame=nested_root,
                         extra={"shapeRef": ref})})
    # (3) a rect STYLED by the library color, on fill AND stroke
    changes.append(
        {"type": "add-obj", "id": styled_rect, "pageId": page,
         "frameId": ROOT, "parentId": ROOT,
         "obj": rect(styled_rect, "Styled Rect", 300, 300, 120, 48,
                     "#12b886", extra={
                         "fills": [{"fillColor": "#12b886",
                                    "fillOpacity": 1,
                                    "fillColorRefId": color["id"],
                                    "fillColorRefFile": lib_id}],
                         "strokes": [{"strokeColor": "#12b886",
                                      "strokeOpacity": 1, "strokeWidth": 2,
                                      "strokeAlignment": "center",
                                      "strokeStyle": "solid",
                                      "strokeColorRefId": color["id"],
                                      "strokeColorRefFile": lib_id}]})})
    # (4) a text shape STYLED by the library typography (content nodes)
    changes.append(
        {"type": "add-obj", "id": styled_text, "pageId": page,
         "frameId": ROOT, "parentId": ROOT,
         "obj": text_shape(styled_text, "Styled Text", 300, 380, 320, 48,
                           typo["id"], lib_id, color["id"], lib_id)})
    c.rpc("update-file", {
        "id": cons_id, "sessionId": str(uuid.uuid4()),
        "revn": cons["revn"], "vern": cons["vern"],
        "changes": changes, "skipValidate": True})

    # read back the crux
    back = c.rpc("get-file", {"id": cons_id})
    objs = {}
    for p in back["data"]["pagesIndex"].values():
        objs.update(p.get("objects", {}))
    st = objs.get(styled_rect, {})
    tx = objs.get(styled_text, {})
    try:
        tx_para = tx["content"]["children"][0]["children"][0]
        tx_span = tx_para["children"][0]
    except (KeyError, IndexError, TypeError):
        tx_para, tx_span = {}, {}
    fills = st.get("fills") or [{}]
    strokes = st.get("strokes") or [{}]
    ok = (objs.get(root_inst, {}).get("componentFile") == lib_id
          and objs.get(nested_root, {}).get("componentFile") == lib_id
          and all(objs.get(cid, {}).get("shapeRef") == ref
                  for cid, ref in zip(nested_children,
                                      button["subtreeShapeIds"][1:]))
          # the captured shapes really carry the asset refs (the fields
          # round-trip through the backend with these exact names)
          and fills[0].get("fillColorRefId") == color["id"]
          and fills[0].get("fillColorRefFile") == lib_id
          and strokes[0].get("strokeColorRefId") == color["id"]
          and strokes[0].get("strokeColorRefFile") == lib_id
          and tx_para.get("typographyRefId") == typo["id"]
          and tx_para.get("typographyRefFile") == lib_id
          and tx_span.get("typographyRefId") == typo["id"])
    print(("PASS: " if ok else "FAIL: ")
          + "placed root-only + nested-subtree instances + color/typography-"
            "styled shapes "
            f"(componentFile={objs.get(nested_root, {}).get('componentFile')},"
            f" fillColorRefFile={fills[0].get('fillColorRefFile')},"
            f" typographyRefFile={tx_para.get('typographyRefFile')})")
    print(json.dumps({"rootInstanceId": root_inst,
                      "nestedInstanceId": nested_root,
                      "nestedChildIds": nested_children,
                      "styledRectId": styled_rect,
                      "styledTextId": styled_text}, sort_keys=True))
    return 0 if ok else 1


# --------------------------------------------------------------- wait_ondisk


def _any_shape_with_component_file(tree_dir, lib_file_id):
    for dp, _dirs, files in os.walk(tree_dir):
        for fn in files:
            if not fn.endswith(".json"):
                continue
            try:
                with open(os.path.join(dp, fn), "rb") as fh:
                    obj = json.loads(fh.read())
            except Exception:
                continue
            if (isinstance(obj, dict)
                    and obj.get("componentFile") == lib_file_id):
                return obj
    return None


def wait_ondisk(base, vault, file_id, lib_file_id, inst_id, wait_s,
                expect_manifest_fid=None):
    """inst_id may be 'any': accept any shape carrying componentFile=<libId>
    (used after import-as-new remints the consumer's internal shape ids).
    With expect_manifest_fid, ALSO require the tree's embedded binfile
    manifest to carry that file id — proof the DAEMON re-exported the tree
    (a hand-dropped tree still carries its foreign embedded id)."""
    deadline = time.time() + float(wait_s)
    rel = None
    while time.time() < deadline:
        rel = _rel_from_manifest(vault, file_id) or \
            resolve_rel_path(base, file_id)
        if rel:
            tree = os.path.join(vault, rel)
            if expect_manifest_fid is not None:
                try:
                    with open(os.path.join(tree, "manifest.json")) as fh:
                        man = json.load(fh)
                    ids = [f.get("id") for f in man.get("files", [])]
                except Exception:
                    ids = []
                if ids != [expect_manifest_fid]:
                    time.sleep(2)
                    continue
            if inst_id == "any":
                shape = _any_shape_with_component_file(tree, lib_file_id)
            else:
                shape = find_shape_on_disk(tree, inst_id)
                if shape is not None and \
                        shape.get("componentFile") != lib_file_id:
                    shape = None
            if shape is not None:
                print("PASS: on-disk tree carries the instance with "
                      "componentFile=<libId> (surgical export)"
                      + (" and the DAEMON-minted embedded file id"
                         if expect_manifest_fid else ""))
                print(json.dumps({"relPath": rel}))
                return 0
        time.sleep(2)
    print(f"FAIL: instance never on disk with componentFile={lib_file_id} "
          f"within {wait_s}s (rel={rel}, "
          f"expectManifestFid={expect_manifest_fid})")
    return 1


def _rel_from_manifest(vault, file_id):
    try:
        with open(os.path.join(vault, ".penpot-sync.json")) as fh:
            man = json.load(fh)
        entry = (man.get("files") or {}).get(file_id)
        return entry.get("path") if entry else None
    except Exception:
        return None


def find_file_by_rel(vault, rel_path, wait_s):
    """Resolve the DB-AUTHORITATIVE file id for a vault-relative path from the
    sync manifest (.penpot-sync.json). NOT from /__api/vault/boards: the index
    is disk-derived, so for a hand-dropped tree it reports the tree's EMBEDDED
    (foreign) file id until the daemon re-exports — the manifest records the id
    the daemon's import actually minted."""
    manifest_path = os.path.join(vault, ".penpot-sync.json")
    deadline = time.time() + float(wait_s)
    while time.time() < deadline:
        try:
            with open(manifest_path) as fh:
                man = json.load(fh)
            for fid, entry in (man.get("files") or {}).items():
                if entry.get("path") == rel_path:
                    print(json.dumps({"fileId": fid}))
                    return 0
        except Exception:
            pass
        time.sleep(2)
    print(f"FAIL: {rel_path} never appeared in {manifest_path} "
          f"within {wait_s}s")
    return 1


# ------------------------------------------------------------ add_lock_entry


def add_lock_entry(vault, entry_id, file_id, name, lib_pkg_id, lib_file_id,
                   lib_version, lib_contract_hash):
    lock_path = os.path.join(vault, "lock.json")
    with open(lock_path) as fh:
        lock = json.load(fh)
    lock.setdefault("packages", {})[entry_id] = {
        "version": "0.0.0",
        "kind": "carried-consumer",
        "contentHash": "",
        "contractHash": "",
        "sourceGitUrl": "",
        "fileId": file_id,
        "name": name,
        "installedAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "libraryShared": False,
        "pluginProps": {},
        "links": [{
            "libraryFileId": lib_file_id,
            "libraryPackageId": lib_pkg_id,
            "version": lib_version,
            "contractHash": lib_contract_hash,
        }],
    }
    with open(lock_path, "w") as fh:
        json.dump(lock, fh, indent=2, sort_keys=True)
        fh.write("\n")
    print(f"PASS: lock.json entry {entry_id!r} pinned "
          f"(fileId={file_id}, link -> {lib_file_id})")
    return 0


def link(base, backend, token, cons_id, lib_id):
    c = make_client(base, backend, token)
    c.rpc("link-file-to-library", {"fileId": cons_id, "libraryId": lib_id})
    libs = c.rpc("get-file-libraries", {"fileId": cons_id}) or []
    ok = any(l.get("id") == lib_id for l in libs)
    print(("PASS: " if ok else "FAIL: ")
          + f"link-file-to-library + get-file-libraries lists the library "
            f"({[l.get('id') for l in libs]})")
    return 0 if ok else 1


def nudge(base, backend, token, file_id, shape_id, x):
    """shape_id may be 'auto': pick the first shape carrying componentFile
    (an instance root), else the first non-root object."""
    c = make_client(base, backend, token)
    meta = c.rpc("get-file", {"id": file_id})
    page = None
    if shape_id == "auto":
        fallback = None
        for pid, p in meta["data"]["pagesIndex"].items():
            for sid, s in (p.get("objects") or {}).items():
                if sid == ROOT:
                    continue
                if s.get("componentFile"):
                    shape_id, page = sid, pid
                    break
                if fallback is None:
                    fallback = (sid, pid)
            if page:
                break
        if page is None and fallback:
            shape_id, page = fallback
    else:
        for pid, p in meta["data"]["pagesIndex"].items():
            if shape_id in p.get("objects", {}):
                page = pid
                break
    if page is None:
        print(f"FAIL: shape {shape_id} not found in {file_id}")
        return 1
    c.rpc("update-file", {
        "id": file_id, "sessionId": str(uuid.uuid4()),
        "revn": meta["revn"], "vern": meta["vern"],
        "changes": [{"type": "mod-obj", "id": shape_id, "pageId": page,
                     "operations": [{"type": "set", "attr": "x",
                                     "val": float(x)}]}],
        "skipValidate": True})
    print(f"PASS: nudged {shape_id} (x={x}) to force a re-export")
    return 0


# -------------------------------------------------------------- verify_live


def verify_live(base, backend, token, cons_id, lib_id, wait_s="0",
                expect_relink=False):
    c = make_client(base, backend, token)
    deadline = time.time() + float(wait_s)
    last = None
    while True:
        try:
            last = _verify_once(c, cons_id, lib_id, expect_relink)
            if last["zeroDangling"] and (not expect_relink
                                         or last["relinked"]):
                print("PASS: ZERO dangling refs over RPC "
                      f"(instances={last['instanceRoots']}, "
                      f"assetRefs={last['assetRefs']}, "
                      f"refsChecked={last['refsChecked']}"
                      + (", file_library_rel re-derived"
                         if expect_relink else "") + ")")
                print(json.dumps(last, sort_keys=True))
                return 0
        except Exception as e:
            last = {"error": str(e)}
        if time.time() >= deadline:
            print(f"FAIL: dangling refs / missing relink: "
                  f"{json.dumps(last, sort_keys=True)}")
            return 1
        time.sleep(2)


def _iter_asset_refs(node):
    """Yield (fileField, idField, kind, refFile, refId) for every asset ref
    in a decoded shape value — recurses through fills, strokes and text
    CONTENT nodes (typography refs live on paragraph/span nodes). Shares
    e6_rewrite's field enumeration so live and static verify agree."""
    if isinstance(node, dict):
        for ff, fi, kind in e6.ASSET_REF_FIELDS:
            if ff in node or fi in node:
                yield (ff, fi, kind, node.get(ff), node.get(fi))
        for v in node.values():
            yield from _iter_asset_refs(v)
    elif isinstance(node, list):
        for v in node:
            yield from _iter_asset_refs(v)


def _verify_once(c, cons_id, lib_id, expect_relink):
    cons = c.rpc("get-file", {"id": cons_id})
    lib = c.rpc("get-file", {"id": lib_id})
    lib_components = {
        cid for cid, comp in (lib["data"].get("components") or {}).items()
        if not (isinstance(comp, dict) and comp.get("deleted"))}
    lib_shapes = set()
    for p in lib["data"]["pagesIndex"].values():
        lib_shapes.update((p.get("objects") or {}).keys())
    lib_assets = {
        "color": set((lib["data"].get("colors") or {}).keys()),
        "typography": set((lib["data"].get("typographies") or {}).keys()),
    }

    dangling, resolved, roots, asset_refs = [], 0, 0, 0
    for p in cons["data"]["pagesIndex"].values():
        for sid, s in (p.get("objects") or {}).items():
            cf = s.get("componentFile")
            sr = s.get("shapeRef")
            if cf is not None or sr is not None:
                bad = []
                if cf is not None:
                    roots += 1
                    if cf != lib_id:
                        bad.append("componentFile")
                    if s.get("componentId") not in lib_components:
                        bad.append("componentId")
                if sr is not None and sr not in lib_shapes:
                    bad.append("shapeRef")
                if bad:
                    dangling.append({"shapeId": sid, "danglingFields": bad,
                                     "componentFile": cf,
                                     "componentId": s.get("componentId"),
                                     "shapeRef": sr})
                else:
                    resolved += 1
            # library ASSET refs (fills / strokes / text content nodes);
            # refFile None or == the consumer's own id would be a local
            # asset — the fixture styles only from the library
            for ff, fi, kind, rf, rid in _iter_asset_refs(s):
                if rf is None or rf == cons_id:
                    continue
                asset_refs += 1
                bad = []
                if rf != lib_id:
                    bad.append(ff)
                if rid is not None and rid not in lib_assets[kind]:
                    bad.append(fi)
                if bad:
                    dangling.append({"shapeId": sid, "danglingFields": bad,
                                     "assetKind": kind,
                                     "refFile": rf, "refId": rid})
                else:
                    resolved += 1
    out = {"consumerId": cons_id, "libId": lib_id,
           "instanceRoots": roots, "assetRefs": asset_refs,
           "refsChecked": resolved + len(dangling),
           "resolved": resolved, "dangling": dangling,
           "zeroDangling": not dangling}
    if expect_relink:
        libs = c.rpc("get-file-libraries", {"fileId": cons_id}) or []
        out["relinked"] = any(l.get("id") == lib_id for l in libs)
    return out


def main():
    argv = sys.argv[1:]
    if not argv:
        print(__doc__, file=sys.stderr)
        return 2
    cmd, a = argv[0], argv[1:]
    if cmd == "author_sources":
        return author_sources(*a)
    if cmd == "capture_lib":
        return capture_lib(*a)
    if cmd == "place_instances":
        return place_instances(*a)
    if cmd == "wait_ondisk":
        return wait_ondisk(*a)
    if cmd == "find_file_by_rel":
        return find_file_by_rel(*a)
    if cmd == "add_lock_entry":
        return add_lock_entry(*a)
    if cmd == "link":
        return link(*a)
    if cmd == "nudge":
        return nudge(*a)
    if cmd == "verify_live":
        expect = "--expect-relink" in a
        a = [x for x in a if x != "--expect-relink"]
        return verify_live(*a, expect_relink=expect)
    print(f"unknown subcommand {cmd!r}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
