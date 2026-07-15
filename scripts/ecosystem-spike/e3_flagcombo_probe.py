#!/usr/bin/env python3
"""E3 flag-combo follow-up (THROWAWAY). ONE design question:

  export_binfile takes TWO independent booleans (include_libraries, embed_assets).
  The daemon uses (include=false, embed=true). The E3 spike proved
  (include=true, embed=false)+trim preserves componentFile=<libId> but loses the
  consumer's own media. The untested config is BOTH true. Does
  (include=true, embed=true)+trim-to-own-subtree give BOTH the library link AND
  the consumer's own embedded media -> a single uniform daemon flag set?

Reuses e3_probe.py's author-phase helpers (rect, export_tree, find_shape_on_disk,
trim_to_single_file) and n5_vaults_helper's raster upload path. Single boot, no
DB wipe. Captures four on-disk exports and answers Q1..Q4 from real bytes.
"""
import hashlib
import io
import json
import os
import struct
import sys
import urllib.request
import uuid
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))      # scripts/ for roundtrip.py
sys.path.insert(0, HERE)                            # for e3_probe.py

import roundtrip as rt          # noqa: E402
import e3_probe as e3           # noqa: E402  (author-phase helpers)

BACKEND = os.environ["PENPOT_BACKEND"]
TOKEN = os.environ["PENPOT_TOKEN"]
ROOT = e3.ROOT


# ---------------------------------------------------------------- raster media
# (mirrors scripts/n5_vaults_helper.py tiny_png + upload_media, kebab multipart)

def tiny_png(seed):
    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d))
    ihdr = chunk(b"IHDR", struct.pack(">IIBBBBB", 8, 8, 8, 0, 0, 0, 0))
    raw = b"".join(
        b"\x00" + bytes(((x * 30) + seed * 7) % 256 for x in range(8)) for _ in range(8))
    return b"\x89PNG\r\n\x1a\n" + ihdr + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")


def upload_media(file_id, name, png):
    """upload-file-media-object (multipart, kebab-case fields, image/png)."""
    boundary = "e3media" + uuid.uuid4().hex
    buf = io.BytesIO()
    for n, v in [("file-id", file_id), ("is-local", "true"), ("name", name)]:
        buf.write(f"--{boundary}\r\n".encode())
        buf.write(f'Content-Disposition: form-data; name="{n}"\r\n\r\n{v}\r\n'.encode())
    buf.write(f"--{boundary}\r\n".encode())
    buf.write((f'Content-Disposition: form-data; name="content"; filename="{name}"\r\n'
               "Content-Type: image/png\r\n\r\n").encode())
    buf.write(png + b"\r\n")
    buf.write(f"--{boundary}--\r\n".encode())
    req = urllib.request.Request(
        f"{BACKEND}/api/rpc/command/upload-file-media-object", data=buf.getvalue(),
        headers={"Content-Type": f"multipart/form-data; boundary={boundary}",
                 "Accept": "application/json", "Authorization": "Token " + TOKEN})
    with urllib.request.urlopen(req) as resp:
        return json.loads(resp.read())


def image_fill_rect(sid, name, x, y, up):
    """A rect whose fill is the uploaded media object (forces export to carry
    the blob — unreferenced media may be pruned)."""
    return e3.rect(sid, name, x, y, extra={"fills": [{"fillOpacity": 1, "fillImage": {
        "id": up["id"], "name": up["name"], "width": up["width"],
        "height": up["height"], "mtype": up["mtype"], "keepAspectRatio": False}}]})


def blobs(files):
    """Non-.json files in a tree = the embedded media blobs (m2 convention)."""
    return {rel: content for rel, content in files.items() if not rel.endswith(".json")}


def own_media_present(files, png_sha):
    """Is the consumer's OWN uploaded raster present on disk? Reported both by
    exact-byte match of the uploaded PNG and by raw blob-file count."""
    bl = blobs(files)
    matching = [rel for rel, c in bl.items() if hashlib.sha256(c).hexdigest() == png_sha]
    return {"blobPaths": sorted(bl), "blobCount": len(bl),
            "exactPngMatchPaths": matching, "present": bool(matching)}


def sem_hash(files):
    return rt.tree_hash(rt.semantic_files(files))


# --------------------------------------------------------------------- probe

def run(work):
    os.makedirs(work, exist_ok=True)
    client, prof = e3.client_login()
    project_id = prof["defaultProjectId"]
    out = {"projectId": project_id}

    # ===== 1. LIBRARY (variant-set component + color + typography), published ==
    lib = client.rpc("create-file", {"name": "e3fc-library", "projectId": project_id})
    lib_id = lib["id"]
    lib_page = lib["data"]["pages"][0]
    comp_id, variant_id, main_inst = (str(uuid.uuid4()) for _ in range(3))
    lib_changes = [
        {"type": "add-obj", "id": main_inst, "pageId": lib_page,
         "frameId": ROOT, "parentId": ROOT,
         "obj": e3.rect(main_inst, "Button / Primary", 0, 0, extra={
             "componentId": comp_id, "componentFile": lib_id, "componentRoot": True,
             "mainInstance": True, "variantId": variant_id})},
        {"type": "add-component", "id": comp_id, "name": "Primary",
         "path": "Controls / Button", "mainInstanceId": main_inst,
         "mainInstancePage": lib_page, "variantId": variant_id,
         "variantProperties": [{"name": "State", "value": "Default"}]},
        {"type": "add-color", "color": {"id": str(uuid.uuid4()), "name": "Brand Teal",
                                        "color": "#12b886", "opacity": 1}},
        {"type": "add-typography", "typography": {
            "id": str(uuid.uuid4()), "name": "Heading XL", "fontId": "sourcesanspro",
            "fontFamily": "sourcesanspro", "fontVariantId": "bold", "fontSize": "36",
            "fontWeight": "700", "fontStyle": "normal", "lineHeight": "1.2",
            "letterSpacing": "0", "textTransform": "none"}},
    ]
    client.rpc("update-file", {"id": lib_id, "sessionId": str(uuid.uuid4()),
                               "revn": lib["revn"], "vern": lib["vern"],
                               "changes": lib_changes, "skipValidate": True})
    client.rpc("set-file-shared", {"id": lib_id, "isShared": True})

    # ===== 2. CONSUMER: linked + instance(componentFile=libId) + OWN media =====
    cons = client.rpc("create-file", {"name": "e3fc-consumer", "projectId": project_id})
    cons_id = cons["id"]
    cons_page = cons["data"]["pages"][0]
    client.rpc("link-file-to-library", {"fileId": cons_id, "libraryId": lib_id})

    cons_png = tiny_png(11)
    cons_png_sha = hashlib.sha256(cons_png).hexdigest()
    cons_up = upload_media(cons_id, "consumer-raster.png", cons_png)

    inst_id = str(uuid.uuid4())
    img_id = str(uuid.uuid4())
    client.rpc("update-file", {
        "id": cons_id, "sessionId": str(uuid.uuid4()),
        "revn": cons["revn"], "vern": cons["vern"], "skipValidate": True, "changes": [
            {"type": "add-obj", "id": inst_id, "pageId": cons_page,
             "frameId": ROOT, "parentId": ROOT,
             "obj": e3.rect(inst_id, "Button / Primary (instance)", 40, 40, extra={
                 "componentId": comp_id, "componentFile": lib_id,
                 "componentRoot": True, "shapeRef": main_inst})},
            {"type": "add-obj", "id": img_id, "pageId": cons_page,
             "frameId": ROOT, "parentId": ROOT,
             "obj": image_fill_rect(img_id, "Consumer Raster", 40, 200, cons_up)},
        ]})
    out.update({"libId": lib_id, "consumerId": cons_id, "instanceId": inst_id,
                "consumerMediaId": cons_up["id"], "consumerPngSha256": cons_png_sha})

    # ===== Q0: is (include=true, embed=true) even a legal export? =============
    # The follow-up's premise was that BOTH booleans true is the untested combo.
    # Capture whether the SERVER accepts it at all before assuming Q1/Q2.
    bothTrueError = None
    try:
        e3.export_tree(client, cons_id, os.path.join(work, "cons-both-true.penpot"),
                       embed=True, include_libs=True)
        bothTrueAccepted = True
    except Exception as exc:  # noqa: BLE001
        bothTrueAccepted = False
        bothTrueError = str(exc)
    out["Q0_bothTrue"] = {"accepted": bothTrueAccepted, "error": bothTrueError}

    # ===== Q1 + Q2: what each ACHIEVABLE combo does to the linked consumer =====
    # A) (include=true, embed=false)+trim  — the prior spike's link-preserving recipe
    inclA_files, inclA_manifest, _ = e3.export_tree(
        client, cons_id, os.path.join(work, "cons-incl.penpot"),
        embed=False, include_libs=True)
    trimA_dir = e3.trim_to_single_file(
        os.path.join(work, "cons-incl.penpot"), cons_id,
        os.path.join(work, "cons-incl-trimmed.penpot"))
    trimA_files = rt.tree_files(trimA_dir)
    _, aobj = e3.find_shape_on_disk(trimA_files, inst_id)
    a_cf = aobj.get("componentFile") if aobj else None
    a_media = own_media_present(trimA_files, cons_png_sha)

    # B) (include=false, embed=true)  — today's daemon flags
    embedB_files, embedB_manifest, _ = e3.export_tree(
        client, cons_id, os.path.join(work, "cons-embed.penpot"),
        embed=True, include_libs=False)
    _, bobj = e3.find_shape_on_disk(embedB_files, inst_id)
    b_cf = bobj.get("componentFile") if bobj else None
    b_media = own_media_present(embedB_files, cons_png_sha)

    out["Q1"] = {
        "note": "(include=true,embed=true) rejected by server; capturing the two achievable combos",
        "incLibs_embedFalse_trim": {
            "instanceComponentFile_onDisk": a_cf,
            "isLibId": a_cf == lib_id, "isConsumerId": a_cf == cons_id},
        "incFalse_embedTrue_daemon": {
            "instanceComponentFile_onDisk": b_cf,
            "isLibId": b_cf == lib_id, "isConsumerId": b_cf == cons_id},
        "answer_libIdCombo": "incLibs_embedFalse_trim -> " + (
            "libId(GOOD)" if a_cf == lib_id else f"{a_cf}")}

    out["Q2"] = {
        "incLibs_embedFalse_trim_ownMedia": a_media,
        "incFalse_embedTrue_daemon_ownMedia": b_media,
        "linkPreservingCombo_embedsOwnMedia": a_media["present"],
        "answer": "YES" if a_media["present"] else "NO",
        "tradeoff": ("link+media both preserved" if (a_cf == lib_id and a_media["present"])
                     else "NO single export preserves BOTH link and own media")}

    # ===== Q3 + Q4: an UNLINKED file WITH uploaded media =======================
    unl = client.rpc("create-file", {"name": "e3fc-unlinked", "projectId": project_id})
    unl_id = unl["id"]
    unl_page = unl["data"]["pages"][0]
    unl_png = tiny_png(22)
    unl_png_sha = hashlib.sha256(unl_png).hexdigest()
    unl_up = upload_media(unl_id, "unlinked-raster.png", unl_png)
    unl_img = str(uuid.uuid4())
    client.rpc("update-file", {
        "id": unl_id, "sessionId": str(uuid.uuid4()),
        "revn": unl["revn"], "vern": unl["vern"], "skipValidate": True, "changes": [
            {"type": "add-obj", "id": unl_img, "pageId": unl_page,
             "frameId": ROOT, "parentId": ROOT,
             "obj": image_fill_rect(unl_img, "Unlinked Raster", 40, 40, unl_up)},
        ]})
    out["unlinkedId"] = unl_id
    out["unlinkedMediaId"] = unl_up["id"]

    # today's daemon flags: (include=false, embed=true)
    daemon_files, daemon_manifest, _ = e3.export_tree(
        client, unl_id, os.path.join(work, "unl-daemon.penpot"),
        embed=True, include_libs=False)
    # The follow-up's candidate uniform was (include=true, embed=true)+trim, but
    # the server REJECTS both-true (Q0). The only achievable include-libraries
    # export is (include=true, embed=false)+trim — so THAT is the uniform
    # candidate we can actually test as a single-flag-set for all files.
    uniform_raw_files, uniform_manifest, _ = e3.export_tree(
        client, unl_id, os.path.join(work, "unl-uniform.penpot"),
        embed=False, include_libs=True)
    uniform_trim_dir = e3.trim_to_single_file(
        os.path.join(work, "unl-uniform.penpot"), unl_id,
        os.path.join(work, "unl-uniform-trimmed.penpot"))
    uniform_files = rt.tree_files(uniform_trim_dir)

    h_daemon = sem_hash(daemon_files)
    h_uniform = sem_hash(uniform_files)
    out["Q3"] = {
        "note": "(include=true,embed=true) is server-rejected (Q0); uniform candidate is the achievable (include=true,embed=false)+trim",
        "daemon_flags": "(include=false, embed=true)",
        "uniform_flags": "(include=true, embed=false)+trim",
        "semanticHash_daemon": h_daemon,
        "semanticHash_uniform": h_uniform,
        "equal": h_daemon == h_uniform,
        "answer": "EQUAL" if h_daemon == h_uniform else "NOT-EQUAL",
        "daemon_embedsMedia": own_media_present(daemon_files, unl_png_sha)["present"],
        "uniform_embedsMedia": own_media_present(uniform_files, unl_png_sha)["present"]}
    if h_daemon != h_uniform:
        out["Q3"]["diff"] = rt.diff_trees(
            rt.semantic_files(daemon_files), rt.semantic_files(uniform_files),
            "daemon", "uniform")

    # Q4: is trim a strict no-op for the unlinked include-export?
    uniform_manifest_files = [f.get("id") for f in uniform_manifest.get("files", [])]
    uniform_relations = uniform_manifest.get("relations")
    other_subtrees = sorted(
        rel for rel in uniform_raw_files
        if rel.startswith("files/") and not (
            rel == f"files/{unl_id}.json" or rel.startswith(f"files/{unl_id}/")))
    raw_h = sem_hash(uniform_raw_files)
    trim_h = h_uniform
    out["Q4"] = {
        "manifestFiles_preTrim": uniform_manifest_files,
        "manifestFilesCount_preTrim": len(uniform_manifest_files),
        "manifestRelations_preTrim": uniform_relations,
        "otherFileSubtreesToDrop": other_subtrees,
        "semanticHash_preTrim": raw_h,
        "semanticHash_postTrim": trim_h,
        "trimIsNoOp": (len(uniform_manifest_files) == 1
                       and uniform_relations in ([], None)
                       and not other_subtrees
                       and raw_h == trim_h),
        "answer": "YES" if (len(uniform_manifest_files) == 1
                            and uniform_relations in ([], None)
                            and not other_subtrees
                            and raw_h == trim_h) else "NO"}

    # ===== recommendation ======================================================
    # UNIFORM would require: both-true accepted AND that export keeps libId AND
    # embeds own media AND unlinked hash unchanged. Both-true is server-rejected,
    # so UNIFORM is only possible if a single ACHIEVABLE flag set works for all
    # files: i.e. the link-preserving combo also embeds own media (Q2 YES) and
    # the unlinked daemon/uniform hashes match (Q3 EQUAL).
    uniform_ok = (out["Q0_bothTrue"]["accepted"]
                  and out["Q1"]["incLibs_embedFalse_trim"]["isLibId"]
                  and out["Q2"]["answer"] == "YES"
                  and out["Q3"]["equal"])
    # A weaker uniform is still viable if the achievable include-combo preserves
    # both link and media AND matches the daemon export for unlinked files.
    weak_uniform_ok = (out["Q2"]["answer"] == "YES" and out["Q3"]["equal"]
                       and out["Q1"]["incLibs_embedFalse_trim"]["isLibId"])
    out["recommendation"] = "UNIFORM" if (uniform_ok or weak_uniform_ok) else "SURGICAL"

    with open(os.path.join(work, "findings-flagcombo.json"), "w") as fh:
        json.dump(out, fh, indent=2, sort_keys=True)
    print(json.dumps(out, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    work = sys.argv[1] if len(sys.argv) > 1 else "."
    sys.exit(run(work))
