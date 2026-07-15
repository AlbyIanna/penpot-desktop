#!/usr/bin/env python3
"""E3 de-risk spike: single-vault component-library linking (THROWAWAY).

Exercises the three never-before-run library RPCs live against a real 2.16.2
stack and captures the on-disk / rebuild behavior that gates the E3 build:

  set-file-shared / link-file-to-library / unlink-file-from-library
  + get-file-libraries + the :component-file resolution + export detach vs
    embed vs include-libraries + invariant-1 rebuild.

Two phases, sharing a work dir:

  author  <work_dir>
      Author a library file (component w/ variant set + exported color +
      typography), publish it (set-file-shared), create a consumer, link it,
      place an instance referencing the library, capture every request/
      response, export both trees (embedAssets=true, the daemon's flags) and
      also a plain + an include-libraries export for the contrast, then unlink.
      Writes state.json + lib.penpot/ + consumer.penpot/ + findings.json.

  rebuild <work_dir>
      (run after a delete-DB + reboot) Re-import both captured trees IN-PLACE
      (file-id preserved, M2), assert the instance's componentFile STILL
      resolves to the library id, assert file-library-rel is GONE (DB-only),
      re-run link-file-to-library to re-derive it, confirm it returns.

Uses scripts/roundtrip.py as the RPC client (rt.Client). Env: PENPOT_BACKEND,
PENPOT_FRONTEND, PENPOT_TOKEN.
"""
import io
import json
import os
import shutil
import sys
import uuid
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))  # scripts/ for roundtrip.py

import roundtrip as rt  # noqa: E402

BACKEND = os.environ["PENPOT_BACKEND"]
FRONTEND = os.environ["PENPOT_FRONTEND"]
TOKEN = os.environ["PENPOT_TOKEN"]
rt.BACKEND, rt.FRONTEND, rt.TOKEN = BACKEND, FRONTEND, TOKEN

ROOT = "00000000-0000-0000-0000-000000000000"
IDENT = {"a": 1.0, "b": 0.0, "c": 0.0, "d": 1.0, "e": 0.0, "f": 0.0}


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


def client_login():
    c = rt.Client()
    prof = c.rpc("get-profile", {})
    return c, prof


def export_tree(client, file_id, dest, *, embed=True, include_libs=False):
    """export-binfile -> unzip -> normalize -> return (files dict, manifest)."""
    end = client.rpc_sse("export-binfile", {
        "fileId": file_id, "includeLibraries": include_libs,
        "embedAssets": embed})
    zip_bytes = client.download(rt.parse_transit_uri(end))
    if os.path.exists(dest):
        shutil.rmtree(dest)
    rt.unzip_to(zip_bytes, dest)
    rt.normalize_tree(dest)
    files = rt.tree_files(dest)
    manifest = json.loads(files["manifest.json"])
    return files, manifest, zip_bytes


def find_shape_on_disk(files, shape_id):
    """Return (relpath, obj) for the file whose json has id==shape_id."""
    for rel, content in files.items():
        if not rel.endswith(".json"):
            continue
        try:
            obj = json.loads(content)
        except Exception:
            continue
        if isinstance(obj, dict) and obj.get("id") == shape_id:
            return rel, obj
    return None, None


# ---------------------------------------------------------------- phase: author

def author(work):
    os.makedirs(work, exist_ok=True)
    client, prof = client_login()
    project_id = prof["defaultProjectId"]
    out = {"projectId": project_id, "requests": {}, "responses": {}}

    # ============ 1. LIBRARY FILE with a component (variant set) + assets =====
    lib = client.rpc("create-file", {"name": "e3-library", "projectId": project_id})
    lib_id = lib["id"]
    lib_page = lib["data"]["pages"][0]
    comp_id = str(uuid.uuid4())
    variant_id = str(uuid.uuid4())
    main_inst = str(uuid.uuid4())   # main-instance shape (component root)

    lib_changes = [
        # main instance shape (component root, points at its own file = library)
        {"type": "add-obj", "id": main_inst, "pageId": lib_page,
         "frameId": ROOT, "parentId": ROOT,
         "obj": rect(main_inst, "Button / Primary", 0, 0, extra={
             "componentId": comp_id, "componentFile": lib_id,
             "componentRoot": True, "mainInstance": True,
             "variantId": variant_id})},
        # register the component (first-class variant set)
        {"type": "add-component", "id": comp_id, "name": "Primary",
         "path": "Controls / Button",
         "mainInstanceId": main_inst, "mainInstancePage": lib_page,
         "variantId": variant_id,
         "variantProperties": [{"name": "State", "value": "Default"}]},
        # exported color + typography (part of the library contract)
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
    upd_lib_req = {"id": lib_id, "sessionId": str(uuid.uuid4()),
                   "revn": lib["revn"], "vern": lib["vern"],
                   "changes": lib_changes, "skipValidate": True}
    client.rpc("update-file", upd_lib_req)

    # ---- set-file-shared (PUBLISH) ----
    sfs_req = {"id": lib_id, "isShared": True}
    sfs_resp = client.rpc("set-file-shared", sfs_req)
    out["requests"]["set-file-shared"] = sfs_req
    out["responses"]["set-file-shared"] = sfs_resp
    lib_meta = client.rpc("get-file", {"id": lib_id})
    out["responses"]["get-file(library).isShared"] = lib_meta.get("isShared")

    # ============ 2. CONSUMER FILE + link-file-to-library ====================
    cons = client.rpc("create-file", {"name": "e3-consumer", "projectId": project_id})
    cons_id = cons["id"]
    cons_page = cons["data"]["pages"][0]

    ltl_req = {"fileId": cons_id, "libraryId": lib_id}
    ltl_resp = client.rpc("link-file-to-library", ltl_req)
    out["requests"]["link-file-to-library"] = ltl_req
    out["responses"]["link-file-to-library"] = ltl_resp

    # ---- place an INSTANCE of the library component into the consumer ----
    inst_id = str(uuid.uuid4())
    inst_changes = [
        {"type": "add-obj", "id": inst_id, "pageId": cons_page,
         "frameId": ROOT, "parentId": ROOT,
         "obj": rect(inst_id, "Button / Primary (instance)", 40, 40, extra={
             "componentId": comp_id,
             "componentFile": lib_id,        # <-- the crux: library's file-id
             "componentRoot": True,
             "shapeRef": main_inst})},
    ]
    client.rpc("update-file", {
        "id": cons_id, "sessionId": str(uuid.uuid4()),
        "revn": cons["revn"], "vern": cons["vern"],
        "changes": inst_changes, "skipValidate": True})

    # ---- inspect the instance in the DB (get-file) ----
    cons_file = client.rpc("get-file", {"id": cons_id})
    inst_db = None
    for pid, page in cons_file["data"]["pagesIndex"].items():
        if inst_id in page["objects"]:
            inst_db = page["objects"][inst_id]
            out["instanceDbPath"] = f"data.pagesIndex.{pid}.objects.{inst_id}"
            break
    out["responses"]["instance.componentFile"] = inst_db.get("componentFile") if inst_db else None
    out["responses"]["instance.componentId"] = inst_db.get("componentId") if inst_db else None
    out["responses"]["instance.shapeRef"] = inst_db.get("shapeRef") if inst_db else None
    out["libId"] = lib_id
    out["consumerId"] = cons_id
    out["componentId"] = comp_id
    out["mainInstanceId"] = main_inst
    out["instanceId"] = inst_id

    # ---- get-file-libraries (the file-library-rel relation) ----
    gfl = client.rpc("get-file-libraries", {"fileId": cons_id})
    out["responses"]["get-file-libraries(consumer)"] = gfl

    # ============ 3. EXPORT: on-disk representation (3 flag combos) ==========
    lib_files, lib_manifest, _ = export_tree(client, lib_id,
                                             os.path.join(work, "lib.penpot"))
    cons_files, cons_manifest, _ = export_tree(
        client, cons_id, os.path.join(work, "consumer.penpot"))  # embed=True (daemon)
    rel, obj = find_shape_on_disk(cons_files, inst_id)
    out["onDisk"] = {
        "consumerInstanceRelPath": rel,
        "consumerInstanceComponentFile": obj.get("componentFile") if obj else None,
        "consumerInstanceShapeRef": obj.get("shapeRef") if obj else None,
        "consumerManifestRelations": cons_manifest.get("relations"),
        "consumerManifestFiles": [f.get("id") for f in cons_manifest.get("files", [])],
        "consumerFileCount_embed": len(cons_manifest.get("files", [])),
    }

    # plain export (embed=False, include=False) — the DETACH path
    plain_files, plain_manifest, _ = export_tree(
        client, cons_id, os.path.join(work, "consumer-plain.penpot"),
        embed=False, include_libs=False)
    prel, pobj = find_shape_on_disk(plain_files, inst_id)
    out["onDisk"]["plain_detach_componentFile"] = (
        pobj.get("componentFile") if pobj else None)
    out["onDisk"]["plain_manifestRelations"] = plain_manifest.get("relations")

    # include-libraries export (the ANTI-PATTERN) — inlines the library file
    incl_files, incl_manifest, _ = export_tree(
        client, cons_id, os.path.join(work, "consumer-incl.penpot"),
        embed=False, include_libs=True)
    out["onDisk"]["includeLibraries_fileCount"] = len(incl_manifest.get("files", []))
    out["onDisk"]["includeLibraries_fileIds"] = [
        f.get("id") for f in incl_manifest.get("files", [])]
    out["onDisk"]["includeLibraries_relations"] = incl_manifest.get("relations")

    # ============ 4. unlink-file-from-library ================================
    ufl_req = {"fileId": cons_id, "libraryId": lib_id}
    ufl_resp = client.rpc("unlink-file-from-library", ufl_req)
    out["requests"]["unlink-file-from-library"] = ufl_req
    out["responses"]["unlink-file-from-library"] = ufl_resp
    gfl_after = client.rpc("get-file-libraries", {"fileId": cons_id})
    out["responses"]["get-file-libraries(after-unlink)"] = gfl_after

    # persist state for the rebuild phase
    with open(os.path.join(work, "state.json"), "w") as fh:
        json.dump({"libId": lib_id, "consumerId": cons_id,
                   "componentId": comp_id, "instanceId": inst_id,
                   "mainInstanceId": main_inst}, fh, indent=2)
    with open(os.path.join(work, "findings-author.json"), "w") as fh:
        json.dump(out, fh, indent=2)
    print(json.dumps(out, indent=2))
    return 0


# --------------------------------------------------------------- phase: rebuild

def resurrect(client, project_id, file_id, name, tree_dir):
    """M2 resurrect recipe (crates/sync-daemon/src/engine.rs::import_in_place):
    create-file with the OLD id, then import the tree IN-PLACE onto it."""
    client.rpc("create-file", {"id": file_id, "name": name,
                               "projectId": project_id})
    zip_bytes = rt.zip_tree(tree_dir)
    return rt.import_binfile(client, project_id, zip_bytes,
                             file_id=file_id, name=name)


def trim_to_single_file(src_tree, keep_id, dest_tree):
    """Take an include-libraries export tree and keep ONLY the `keep_id` file
    subtree + a 1-file manifest with relations removed. This is the E3 on-disk
    representation: the consumer keeps componentFile=<libId> as a bare id
    reference WITHOUT the library inlined into its tree."""
    if os.path.exists(dest_tree):
        shutil.rmtree(dest_tree)
    os.makedirs(dest_tree)
    files = rt.tree_files(src_tree)
    manifest = json.loads(files["manifest.json"])
    manifest["files"] = [f for f in manifest["files"] if f["id"] == keep_id]
    manifest["relations"] = []
    for rel, content in files.items():
        if rel == "manifest.json":
            content = (json.dumps(manifest, sort_keys=True, indent=2,
                                  ensure_ascii=False) + "\n").encode()
        elif not (rel == f"files/{keep_id}.json"
                  or rel.startswith(f"files/{keep_id}/")):
            continue  # drop the inlined library subtree
        p = os.path.join(dest_tree, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "wb") as fh:
            fh.write(content)
    return dest_tree


def rebuild(work):
    with open(os.path.join(work, "state.json")) as fh:
        st = json.load(fh)
    lib_id, cons_id = st["libId"], st["consumerId"]
    inst_id = st["instanceId"]

    client, prof = client_login()
    project_id = prof["defaultProjectId"]
    out = {"projectId_after_wipe": project_id, "libId": lib_id,
           "consumerId": cons_id}

    # E3 on-disk consumer tree = the include-libraries export TRIMMED to the
    # consumer file only (componentFile=libId preserved, library NOT inlined).
    cons_tree = trim_to_single_file(
        os.path.join(work, "consumer-incl.penpot"), cons_id,
        os.path.join(work, "consumer-e3.penpot"))
    # sanity: the trimmed on-disk instance still carries the LIBRARY id.
    trimmed_files = rt.tree_files(cons_tree)
    _, tobj = find_shape_on_disk(trimmed_files, inst_id)
    out["e3TreeInstanceComponentFile"] = tobj.get("componentFile") if tobj else None

    # resurrect BOTH files by their ORIGINAL ids (M2 create-with-id recipe).
    rid_lib = resurrect(client, project_id, lib_id, "e3-library",
                        os.path.join(work, "lib.penpot"))
    rid_cons = resurrect(client, project_id, cons_id, "e3-consumer", cons_tree)
    out["reimportLibSameId"] = (rid_lib == lib_id)
    out["reimportConsumerSameId"] = (rid_cons == cons_id)

    # the crux after rebuild: does the instance STILL resolve to the library id?
    cons_file = client.rpc("get-file", {"id": cons_id})
    inst_db = None
    for page in cons_file["data"]["pagesIndex"].values():
        if inst_id in page["objects"]:
            inst_db = page["objects"][inst_id]
            break
    out["instance.componentFile_afterRebuild"] = (
        inst_db.get("componentFile") if inst_db else None)
    out["instanceStillResolves"] = bool(
        inst_db and inst_db.get("componentFile") == lib_id)

    # file-library-rel must be GONE (it lives only in the disposable DB, never
    # in the embedAssets binfile) -> proves it is derived/disposable.
    gfl_before = client.rpc("get-file-libraries", {"fileId": cons_id})
    out["get-file-libraries_afterRebuild_beforeRelink"] = gfl_before
    out["relGoneAfterWipe"] = (len(gfl_before) == 0)

    # re-derive it from the (would-be) lockfile entry: re-run link-file-to-library
    ltl = client.rpc("link-file-to-library",
                     {"fileId": cons_id, "libraryId": lib_id})
    out["relink_response"] = ltl
    gfl_after = client.rpc("get-file-libraries", {"fileId": cons_id})
    out["get-file-libraries_afterRelink"] = gfl_after
    out["relReDerivable"] = any(f.get("id") == lib_id for f in gfl_after)

    with open(os.path.join(work, "findings-rebuild.json"), "w") as fh:
        json.dump(out, fh, indent=2)
    print(json.dumps(out, indent=2))
    return 0


def main():
    if len(sys.argv) < 3:
        print("usage: e3_probe.py <author|rebuild> <work_dir>", file=sys.stderr)
        return 2
    cmd, work = sys.argv[1], sys.argv[2]
    if cmd == "author":
        return author(work)
    if cmd == "rebuild":
        return rebuild(work)
    print(f"unknown phase {cmd!r}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
