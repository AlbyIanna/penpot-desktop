#!/usr/bin/env python3
"""E1 authored combined fixture + delta matrix.

No shipped Penpot template combines components + variants + applied tokens +
a tokens.json (penpot-design-system is 100% legacy / 0 tokens; the
tokens-starter-kit is 0 components — PLAN3 risk 5). So the E1 gate needs an
AUTHORED fixture. This synthesizes a schema-shaped normalized `.penpot` tree
(the same on-disk layout the sync daemon writes and the ledger hashes) with
BOTH variant models present, applied tokens, exported colors/typographies, and
a DTCG tokens.json — then derives the curated delta matrix by clone-and-edit,
exactly the spike's make_deltas approach so the python oracle can classify the
same trees the Rust extractor does.

Trees written under <workdir>/:
  baseline/                 combined library (first-class set + legacy set + tokens)
  delta-patch/              impl only: move a shape + inline fill + a token $value
  delta-minor/              added exposed property "Theme"  -> minor
  delta-major-removed/      removed exposed property "State" -> major
  delta-major-renamed/      renamed a variant "Default"->"Primary" -> major
  delta-major-typechanged/  a token $type flip (borderRadius->dimension) -> major (E1 ext)
  delta-migration/          legacy set -> first-class (same path/names) -> migration (E1 ext)

Usage: python3 e1-fixture.py <workdir>
"""
import json, os, shutil, sys

# Fixed uuids (the churn gate proves the contract is invariant to these).
FID  = "e1e1e1e1-0000-4000-8000-000000000001"
PID  = "e1e1e1e1-0000-4000-8000-0000000000a0"
ROOT = "00000000-0000-0000-0000-000000000000"
VID  = "e1e1e1e1-0000-4000-8000-0000000000f1"   # first-class variant set id
C1   = "e1e1e1e1-0000-4000-8000-0000000000c1"
C2   = "e1e1e1e1-0000-4000-8000-0000000000c2"
MI1  = "e1e1e1e1-0000-4000-8000-0000000000d1"
CH1  = "e1e1e1e1-0000-4000-8000-0000000000d2"
MI2  = "e1e1e1e1-0000-4000-8000-0000000000d3"
# legacy set components
L1   = "e1e1e1e1-0000-4000-8000-0000000000b1"
L2   = "e1e1e1e1-0000-4000-8000-0000000000b2"
L3   = "e1e1e1e1-0000-4000-8000-0000000000b3"
LMI1 = "e1e1e1e1-0000-4000-8000-0000000000b4"
LMI2 = "e1e1e1e1-0000-4000-8000-0000000000b5"
LMI3 = "e1e1e1e1-0000-4000-8000-0000000000b6"
COL1 = "e1e1e1e1-0000-4000-8000-000000000c01"
COL2 = "e1e1e1e1-0000-4000-8000-000000000c02"
TYP1 = "e1e1e1e1-0000-4000-8000-000000000701"

FC_PATH     = "Controls / Button"
LEGACY_PATH = "Legacy / Combobox"


def w(tree, rel, obj):
    p = os.path.join(tree, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, "w") as fh:
        json.dump(obj, fh, sort_keys=True, indent=2, ensure_ascii=False)
        fh.write("\n")


def baseline(tree):
    if os.path.exists(tree):
        shutil.rmtree(tree)
    w(tree, "manifest.json", {
        "version": 3,
        "files": [{"id": FID, "name": "combo-lib"}],
    })
    w(tree, f"files/{FID}.json", {"id": FID, "name": "combo-lib",
                                  "features": ["variants/v1"]})
    w(tree, f"files/{FID}/pages/{PID}.json", {"id": PID, "name": "Page 1", "index": 0})
    w(tree, f"files/{FID}/pages/{PID}/{ROOT}.json",
      {"id": ROOT, "type": "frame", "name": "Root Frame", "shapes": [MI1, MI2, LMI1, LMI2, LMI3]})

    # --- first-class variant set "Controls / Button" (variantId VID) ---
    w(tree, f"files/{FID}/components/{C1}.json", {
        "id": C1, "name": "Default", "path": FC_PATH, "variantId": VID,
        "variantProperties": [{"name": "Size", "value": "Small"},
                              {"name": "State", "value": "Default"}],
        "mainInstancePage": PID, "mainInstanceId": MI1,
    })
    w(tree, f"files/{FID}/components/{C2}.json", {
        "id": C2, "name": "Large", "path": FC_PATH, "variantId": VID,
        "variantProperties": [{"name": "Size", "value": "Large"},
                              {"name": "State", "value": "Default"}],
        "mainInstancePage": PID, "mainInstanceId": MI2,
    })
    w(tree, f"files/{FID}/pages/{PID}/{MI1}.json", {
        "id": MI1, "type": "frame", "name": "Default", "mainInstance": True,
        "componentId": C1, "componentFile": FID, "variantId": VID,
        "variantName": "Size=Small, State=Default", "frameId": ROOT,
        "x": 0, "y": 0, "fills": [{"fillColor": "#eeeeee", "fillOpacity": 1}],
        "appliedTokens": {"fill": "layerBase.text"}, "shapes": [CH1],
    })
    w(tree, f"files/{FID}/pages/{PID}/{CH1}.json", {
        "id": CH1, "type": "text", "name": "Label", "frameId": MI1,
        "appliedTokens": {"columnGap": "spacing.sm"},
        "content": {"type": "root", "children": []},
    })
    w(tree, f"files/{FID}/pages/{PID}/{MI2}.json", {
        "id": MI2, "type": "frame", "name": "Large", "mainInstance": True,
        "componentId": C2, "componentFile": FID, "variantId": VID,
        "variantName": "Size=Large, State=Default", "frameId": ROOT,
        "x": 200, "y": 0, "appliedTokens": {"fill": "layerBase.text"},
    })

    # --- legacy naming-convention set "Legacy / Combobox" ---
    for cid, mid, name in [(L1, LMI1, "Active"), (L2, LMI2, "Default"), (L3, LMI3, "Disabled")]:
        w(tree, f"files/{FID}/components/{cid}.json", {
            "id": cid, "name": name, "path": LEGACY_PATH,
            "mainInstancePage": PID, "mainInstanceId": mid,
        })
        w(tree, f"files/{FID}/pages/{PID}/{mid}.json", {
            "id": mid, "type": "frame", "name": name, "mainInstance": True,
            "componentId": cid, "frameId": ROOT, "x": 0, "y": 300,
        })

    # --- exported library assets ---
    w(tree, f"files/{FID}/colors/{COL1}.json", {"id": COL1, "name": "Brand Teal", "color": "#12b886", "opacity": 1})
    w(tree, f"files/{FID}/colors/{COL2}.json", {"id": COL2, "name": "Accent", "color": "#ff4400", "opacity": 1})
    w(tree, f"files/{FID}/typographies/{TYP1}.json", {"id": TYP1, "name": "Heading XL", "fontFamily": "Inter", "fontSize": "36"})

    # --- DTCG tokens.json (the vocabulary the components consume) ---
    w(tree, f"files/{FID}/tokens.json", {
        "$metadata": {"tokenSetOrder": ["Foundations", "Semantic"]},
        "$themes": [],
        "Foundations": {
            "spacing": {"sm": {"$type": "spacing", "$value": "4"},
                        "md": {"$type": "spacing", "$value": "8"}},
            "radius": {"lg": {"$type": "borderRadius", "$value": "12"}},
        },
        "Semantic": {
            "layerBase": {"text": {"$type": "color", "$value": "#111111"}},
            "layerOne": {"text": {"$type": "color", "$value": "#222222"}},
        },
    })


def load(tree, rel):
    with open(os.path.join(tree, rel)) as fh:
        return json.load(fh)


def clone(base, work, name):
    dst = os.path.join(work, name)
    if os.path.exists(dst):
        shutil.rmtree(dst)
    shutil.copytree(base, dst)
    return dst


def main():
    work = sys.argv[1]
    os.makedirs(work, exist_ok=True)
    base = os.path.join(work, "baseline")
    baseline(base)

    # (i) PATCH — implementation only: move a shape, change an inline fill, and
    # change a token $value (all implementation, contract untouched).
    t = clone(base, work, "delta-patch")
    s = load(t, f"files/{FID}/pages/{PID}/{MI1}.json")
    s["x"] += 40; s["y"] += 40
    s["fills"] = [{"fillColor": "#123456", "fillOpacity": 1}]
    w(t, f"files/{FID}/pages/{PID}/{MI1}.json", s)
    tok = load(t, f"files/{FID}/tokens.json")
    tok["Semantic"]["layerBase"]["text"]["$value"] = "#000000"   # $value only
    w(t, f"files/{FID}/tokens.json", tok)

    # (ii) MINOR — grow the contract: add an exposed property "Theme".
    t = clone(base, work, "delta-minor")
    for cid in (C1, C2):
        c = load(t, f"files/{FID}/components/{cid}.json")
        c["variantProperties"] = c["variantProperties"] + [{"name": "Theme", "value": "Dark"}]
        w(t, f"files/{FID}/components/{cid}.json", c)

    # (iii) MAJOR (removed) — drop the "State" exposed property.
    t = clone(base, work, "delta-major-removed")
    for cid in (C1, C2):
        c = load(t, f"files/{FID}/components/{cid}.json")
        c["variantProperties"] = [vp for vp in c["variantProperties"] if vp["name"] != "State"]
        w(t, f"files/{FID}/components/{cid}.json", c)

    # (iv) MAJOR (renamed) — rename a variant name.
    t = clone(base, work, "delta-major-renamed")
    c = load(t, f"files/{FID}/components/{C1}.json"); c["name"] = "Primary"
    w(t, f"files/{FID}/components/{C1}.json", c)
    s = load(t, f"files/{FID}/pages/{PID}/{MI1}.json"); s["name"] = "Primary"
    w(t, f"files/{FID}/pages/{PID}/{MI1}.json", s)

    # (v) MAJOR (type-changed, E1 extension beyond the oracle) — flip a token $type.
    t = clone(base, work, "delta-major-typechanged")
    tok = load(t, f"files/{FID}/tokens.json")
    tok["Foundations"]["radius"]["lg"]["$type"] = "dimension"
    w(t, f"files/{FID}/tokens.json", tok)

    # (vi) MIGRATION (E1 extension) — legacy set -> first-class, SAME path/names.
    t = clone(base, work, "delta-migration")
    mvid = "e1e1e1e1-0000-4000-8000-0000000000f2"
    for cid, val in [(L1, "Active"), (L2, "Default"), (L3, "Disabled")]:
        c = load(t, f"files/{FID}/components/{cid}.json")
        c["variantId"] = mvid
        c["variantProperties"] = [{"name": "State", "value": val}]
        w(t, f"files/{FID}/components/{cid}.json", c)

    print(f"[e1-fixture] wrote baseline + 6 deltas under {work}")


if __name__ == "__main__":
    main()
