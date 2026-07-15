#!/usr/bin/env python3
"""Ecosystem spike step 2: extract a CONTRACT per component from a normalized
.penpot tree on disk.

Contract (per the ecosystem design) = {variant names, exposed properties,
tokens used} — implementation excluded. The reusable UNIT is a *variant set*
(the thing a consumer depends on), so we key contracts by set:

  - First-class variants (Penpot 2.7+ `variants/v1`): a set is keyed by the
    component `variantId`; its exposed properties are the DISTINCT
    `variantProperties[].name`, its variant names the per-component
    `variantProperties[].value` tuples (or `variantName`).
  - Legacy naming-convention sets (this is what the bundled design-system uses,
    data-model v67): a set is keyed by the shared component `path`; the variant
    names are the component `name`s under that path; exposed property NAMES are
    not declared on disk (fuzzy — see the report).

  - Tokens used: the union of `appliedTokens` token-refs (values) across every
    shape in every main instance of the set. `appliedTokens` maps a shape
    attribute (fill, columnGap, r1..r4, strokeColor, ...) → a token path.

Pure/offline: reads a `{relpath: bytes}` tree. No network, no server.

Usage: python3 extract_contract.py <tree-dir> [--json out.json]
"""
import json, os, sys, collections

def load_tree(root):
    files = {}
    for dp, _, fs in os.walk(root):
        for f in fs:
            p = os.path.join(dp, f)
            files[os.path.relpath(p, root).replace(os.sep, "/")] = p
    return files


def extract(root):
    files = load_tree(root)
    manifest = json.load(open(files["manifest.json"]))
    fid = manifest["files"][0]["id"]

    # 1. components: id -> meta (incl. optional first-class variant fields)
    components = {}
    cprefix = f"files/{fid}/components/"
    for rel, path in files.items():
        if rel.startswith(cprefix) and rel.endswith(".json"):
            c = json.load(open(path))
            if c.get("deleted"):
                continue
            components[c["id"]] = c

    # 2. index every page shape by (pageId, shapeId)
    shapes = collections.defaultdict(dict)  # pid -> sid -> shape
    sprefix = f"files/{fid}/pages/"
    for rel, path in files.items():
        if not (rel.startswith(sprefix) and rel.endswith(".json")):
            continue
        parts = rel[len(f"files/{fid}/"):-5].split("/")  # pages/<pid>/<sid>
        if len(parts) != 3:  # skip the page doc itself (pages/<pid>.json)
            continue
        _, pid, sid = parts
        s = json.load(open(path))
        if isinstance(s, dict) and "id" in s:
            shapes[pid][sid] = s

    def subtree_token_refs(pid, root_sid):
        """Union of appliedTokens values over the main-instance subtree."""
        refs = set()
        seen = set()
        stack = [root_sid]
        page = shapes.get(pid, {})
        while stack:
            sid = stack.pop()
            if sid in seen:
                continue
            seen.add(sid)
            s = page.get(sid)
            if not s:
                continue
            at = s.get("appliedTokens")
            if isinstance(at, dict):
                for v in at.values():
                    if isinstance(v, str):
                        refs.add(v)
            for child in s.get("shapes", []) or []:
                stack.append(child)
        return refs

    # 3. group components into variant sets
    sets = collections.defaultdict(lambda: {
        "components": [], "firstClass": False})
    for cid, c in components.items():
        if c.get("variantId"):
            key = ("variant-id", c["variantId"])
            sets[key]["firstClass"] = True
        else:
            key = ("path", c.get("path", ""))
        sets[key]["components"].append(c)

    contracts = []
    for key, grp in sets.items():
        comps = grp["components"]
        variant_names = []
        prop_names = set()
        tokens_used = set()
        for c in comps:
            # exposed properties = the first-class variant-property AXES (names).
            # Kept independent from variant names so that adding a property axis
            # reads as contract GROWTH (minor), not a variant rename (major).
            for vp in c.get("variantProperties") or []:
                if isinstance(vp, dict) and vp.get("name"):
                    prop_names.add(vp["name"])
            # variant names = the component's human label, for BOTH set kinds.
            variant_names.append(c.get("name", ""))
            tokens_used |= subtree_token_refs(c.get("mainInstancePage"), c.get("mainInstanceId"))

        kind, kval = key
        contracts.append({
            "set": kval,
            "setKind": "first-class-variant" if grp["firstClass"] else "path-convention",
            "variantNames": sorted(variant_names),
            "exposedProperties": sorted(prop_names),
            "tokensUsed": sorted(tokens_used),
            "componentCount": len(comps),
        })
    contracts.sort(key=lambda x: (x["setKind"], x["set"]))
    return {"fileId": fid, "contractCount": len(contracts), "contracts": contracts}


def main():
    root = sys.argv[1]
    out = extract(root)
    outpath = None
    if "--json" in sys.argv:
        outpath = sys.argv[sys.argv.index("--json") + 1]
        json.dump(out, open(outpath, "w"), indent=2)
    # summary
    fc = [c for c in out["contracts"] if c["setKind"] == "first-class-variant"]
    pc = [c for c in out["contracts"] if c["setKind"] == "path-convention"]
    tok = [c for c in out["contracts"] if c["tokensUsed"]]
    print(f"fileId={out['fileId']} contracts={out['contractCount']} "
          f"(first-class={len(fc)}, path-convention={len(pc)}, with-tokens={len(tok)})")
    if outpath:
        print(f"written {outpath}")


if __name__ == "__main__":
    main()
