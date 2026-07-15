#!/usr/bin/env python3
"""Ecosystem spike step 4b: from a baseline .penpot tree that contains a
first-class variant component (the post-roundtrip preserve tree), construct
THREE on-disk deltas and classify each with diff_contracts.

  (i)   PATCH  — implementation only: move the main-instance shape (x/y) and
         change an inline (non-token) fill color. Contract must be UNCHANGED.
  (ii)  MINOR  — contract grew: add a new exposed property (Theme=Dark) to the
         component's variantProperties. exposedProperties gains "Theme".
  (iii) MAJOR  — contract lost/renamed: change which token the component uses
         (appliedTokens fill layerBase.text -> layerOne.text) AND remove the
         "State" exposed property. tokensUsed + exposedProperties both change.

Pure on-disk edits (no server) — proves the extracted-contract diff, not Penpot.
Usage: python3 make_deltas.py <baseline-tree> <workdir> <component-id> <main-instance-page> <main-instance-shape>
"""
import json, os, shutil, subprocess, sys

BASE = sys.argv[1]
WORK = sys.argv[2]
CID = sys.argv[3]
PAGE = sys.argv[4]
SHAPE = sys.argv[5]
HERE = os.path.dirname(os.path.abspath(__file__))


def fid_of(tree):
    return json.load(open(os.path.join(tree, "manifest.json")))["files"][0]["id"]


def clone(name):
    dst = os.path.join(WORK, name)
    if os.path.exists(dst):
        shutil.rmtree(dst)
    shutil.copytree(BASE, dst)
    return dst


def load(tree, rel):
    return json.load(open(os.path.join(tree, rel)))


def save(tree, rel, obj):
    with open(os.path.join(tree, rel), "w") as fh:
        json.dump(obj, fh, sort_keys=True, indent=2, ensure_ascii=False)
        fh.write("\n")


def contracts(tree, out):
    subprocess.run([sys.executable, os.path.join(HERE, "extract_contract.py"),
                    tree, "--json", out], check=True, capture_output=True)


def diff(a, b, label):
    print(f"\n===== DELTA: {label} =====")
    subprocess.run([sys.executable, os.path.join(HERE, "diff_contracts.py"), a, b], check=True)


def main():
    fid = fid_of(BASE)
    crel = f"files/{fid}/components/{CID}.json"
    srel = f"files/{fid}/pages/{PAGE}/{SHAPE}.json"
    os.makedirs(WORK, exist_ok=True)

    base_c = os.path.join(WORK, "baseline-contracts.json")
    contracts(BASE, base_c)

    # (i) PATCH — implementation only
    t = clone("delta-patch")
    s = load(t, srel)
    s["x"] = s.get("x", 0) + 40           # move the shape
    s["y"] = s.get("y", 0) + 40
    fills = s.get("fills") or [{}]
    fills[0]["fillColor"] = "#123456"      # inline color change (not a token)
    s["fills"] = fills
    save(t, srel, s)
    c = os.path.join(WORK, "delta-patch-contracts.json")
    contracts(t, c)
    diff(base_c, c, "(i) implementation-only  -> expect PATCH")

    # (ii) MINOR — grow the contract (add an exposed property)
    t = clone("delta-minor")
    comp = load(t, crel)
    comp["variantProperties"] = comp.get("variantProperties", []) + [{"name": "Theme", "value": "Dark"}]
    save(t, crel, comp)
    c = os.path.join(WORK, "delta-minor-contracts.json")
    contracts(t, c)
    diff(base_c, c, "(ii) added exposed property 'Theme'  -> expect MINOR")

    # (iii) MAJOR — lose/rename (change token used + remove a property)
    t = clone("delta-major")
    comp = load(t, crel)
    comp["variantProperties"] = [vp for vp in comp.get("variantProperties", []) if vp.get("name") != "State"]
    save(t, crel, comp)
    s = load(t, srel)
    at = s.get("appliedTokens") or {}
    at["fill"] = "layerOne.text"           # component now depends on a DIFFERENT token
    s["appliedTokens"] = at
    save(t, srel, s)
    c = os.path.join(WORK, "delta-major-contracts.json")
    contracts(t, c)
    diff(base_c, c, "(iii) removed 'State' + changed token used  -> expect MAJOR")


if __name__ == "__main__":
    main()
