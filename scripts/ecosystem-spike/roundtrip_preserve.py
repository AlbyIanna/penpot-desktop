#!/usr/bin/env python3
"""Ecosystem spike step 3 (stability / preservation, verified by execution):

A. REAL-DATA token stability: take the already-imported tokens-starter-kit file,
   export -> normalize -> record a specific shape's appliedTokens -> re-import
   IN-PLACE -> re-export -> confirm that shape's appliedTokens is byte-identical.
   Proves `appliedTokens` (the "tokens used" leg) survives a round-trip.

B. INJECTION preservation: into the exported tree, inject a SCHEMA-VALID
   first-class variant component:
     - promote an existing frame F to a variant main instance: set
       mainInstance/componentRoot/componentId + variantId + variantName +
       variantProperties[{name,value}] and an appliedTokens entry;
     - add files/<fid>/components/<cid>.json carrying variantId +
       variantProperties.
   Re-zip -> import IN-PLACE -> re-export -> check the component + its
   variantProperties + the shape's variantName/appliedTokens SURVIVED. Answers
   the GO/NO-GO question: does 2.16.2's binfile round-trip preserve first-class
   variant properties written to disk?

Reuses roundtrip.py Client + helpers. Takes the file id from the earlier import.
Usage: python3 roundtrip_preserve.py <tokens-starter-kit file_id> <workdir>
"""
import json, os, sys, uuid
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import roundtrip as rt

FILE_ID = sys.argv[1]
WORK = sys.argv[2]
os.makedirs(WORK, exist_ok=True)


def export_tree(client, fid, name):
    z = rt.export_binfile(client, fid)
    d = os.path.join(WORK, name)
    rt.unzip_to(z, d)
    rt.normalize_tree(d)
    return d


def find_fid(tree):
    m = json.load(open(os.path.join(tree, "manifest.json")))
    return m["files"][0]["id"]


def main():
    client = rt.Client()
    profile = client.login()
    project_id = profile["defaultProjectId"]

    # ---- A. token stability (real data) ----
    t0 = export_tree(client, FILE_ID, "pre")
    fid = find_fid(t0)
    # pick a shape with appliedTokens
    sample = None
    pdir = os.path.join(t0, "files", fid, "pages")
    for pg in os.listdir(pdir):
        pp = os.path.join(pdir, pg)
        if not os.path.isdir(pp):
            continue
        for f in os.listdir(pp):
            s = json.load(open(os.path.join(pp, f)))
            if isinstance(s, dict) and s.get("appliedTokens"):
                sample = (pg, s["id"], dict(s["appliedTokens"]))
                break
        if sample:
            break
    print(f"[A] sample shape {sample[1]} appliedTokens={sample[2]}")

    # ---- B. inject a first-class variant component ----
    # promote an existing frame F on some page to a variant main instance
    inj_page = inj_shape = None
    for pg in os.listdir(pdir):
        pp = os.path.join(pdir, pg)
        if not os.path.isdir(pp):
            continue
        for f in os.listdir(pp):
            s = json.load(open(os.path.join(pp, f)))
            if isinstance(s, dict) and s.get("type") == "frame" and s.get("id") != "00000000-0000-0000-0000-000000000000":
                inj_page, inj_shape = pg, s
                break
        if inj_shape:
            break
    new_cid = str(uuid.uuid4())
    new_vid = str(uuid.uuid4())
    inj_shape["mainInstance"] = True
    inj_shape["componentRoot"] = True
    inj_shape["componentId"] = new_cid
    inj_shape["componentFile"] = fid
    inj_shape["variantId"] = new_vid
    inj_shape["variantName"] = "Size=Large, State=Default"
    at = inj_shape.get("appliedTokens") or {}
    at["fill"] = "layerBase.text"
    inj_shape["appliedTokens"] = at
    with open(os.path.join(pdir, inj_page, inj_shape["id"] + ".json"), "w") as fh:
        json.dump(inj_shape, fh, sort_keys=True, indent=2, ensure_ascii=False)
        fh.write("\n")
    comp = {
        "id": new_cid,
        "name": "Default",
        "path": "Spike / InjectedVariantSet",
        "mainInstanceId": inj_shape["id"],
        "mainInstancePage": inj_page,
        "variantId": new_vid,
        "variantProperties": [{"name": "Size", "value": "Large"},
                              {"name": "State", "value": "Default"}],
    }
    cdir = os.path.join(t0, "files", fid, "components")
    os.makedirs(cdir, exist_ok=True)
    with open(os.path.join(cdir, new_cid + ".json"), "w") as fh:
        json.dump(comp, fh, sort_keys=True, indent=2, ensure_ascii=False)
        fh.write("\n")
    print(f"[B] injected component {new_cid} (variantId {new_vid}) on page {inj_page}, main instance {inj_shape['id']}")

    # re-zip and import IN-PLACE
    z = rt.zip_tree(t0)
    rid = rt.import_binfile(client, project_id, z, file_id=FILE_ID, name="tokens-starter-kit")
    print(f"[B] in-place import returned {rid} (same id: {rid == FILE_ID})")

    # re-export and inspect
    t1 = export_tree(client, FILE_ID, "post")
    fid1 = find_fid(t1)
    # A result: appliedTokens of the sample shape preserved?
    sp = os.path.join(t1, "files", fid1, "pages", sample[0], sample[1] + ".json")
    a_ok = False
    if os.path.exists(sp):
        s = json.load(open(sp))
        a_ok = s.get("appliedTokens") == sample[2]
    print(f"[A RESULT] sample appliedTokens preserved after round-trip: {a_ok}")

    # B result: did the injected component survive with variantProperties?
    cpath = os.path.join(t1, "files", fid1, "components", new_cid + ".json")
    b_comp_ok = os.path.exists(cpath)
    b_props = None
    if b_comp_ok:
        c = json.load(open(cpath))
        b_props = {"variantId": c.get("variantId"),
                   "variantProperties": c.get("variantProperties"),
                   "name": c.get("name"), "path": c.get("path")}
    # shape-level variant fields survive?
    ip = os.path.join(t1, "files", fid1, "pages", inj_page, inj_shape["id"] + ".json")
    b_shape = None
    if os.path.exists(ip):
        s = json.load(open(ip))
        b_shape = {"variantId": s.get("variantId"), "variantName": s.get("variantName"),
                   "mainInstance": s.get("mainInstance"), "componentId": s.get("componentId"),
                   "appliedTokens": s.get("appliedTokens")}
    print(f"[B RESULT] component json survived: {b_comp_ok}")
    print(f"[B RESULT] component variant fields: {json.dumps(b_props)}")
    print(f"[B RESULT] shape variant/token fields: {json.dumps(b_shape)}")

    json.dump({
        "tokenStability": {"shape": sample[1], "before": sample[2], "preserved": a_ok},
        "injection": {"componentSurvived": b_comp_ok, "componentFields": b_props,
                      "shapeFields": b_shape},
    }, open(os.path.join(WORK, "result.json"), "w"), indent=2)


if __name__ == "__main__":
    main()
