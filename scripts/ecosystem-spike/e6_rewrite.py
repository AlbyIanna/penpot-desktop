#!/usr/bin/env python3
"""E6 — the cross-vault id-remap REWRITE TOOL (PLAN3 E6). Stdlib-only, STATIC:
normalized `.penpot` trees in, rewritten tree + JSON report out. Never touches
a live stack, never drives the SPA (ecosystem invariant 3).

The problem it solves: `import-binfile` WITHOUT a file-id (import-as-new) is
per-DB non-deterministic — the SAME library package installed in two vaults
mints a DIFFERENT `:component-file` id AND different ids for every component /
shape / color / typography inside the library file. A consumer file authored
in vault A and carried to vault B therefore dangles every instance AND every
library-styled fill/stroke/typography. This tool rewrites the consumer's
library refs from vault-A ids to vault-B ids using an identity map JOINED ON
UUID-STABLE KEYS (E1's proven keying — never any uuid):

  components:    path + name + sorted variantProperties
  colors:        path + name  (exported color names are E1 identity)
  typographies:  path + name  (exported typography names are E1 identity)

Rewritten ref fields (all of them, or the tool refuses):
  componentFile / componentId / shapeRef          (instance machinery)
  fillColorRefFile / fillColorRefId               (library color on a fill)
  strokeColorRefFile / strokeColorRefId           (library color on a stroke)
  typographyRefFile / typographyRefId             (library typography — lives
                                                   on text CONTENT nodes:
                                                   paragraph + text spans)

Identity map mechanics
----------------------
For each library tree (the OLD one the consumer references and the NEW one in
the target vault), extract:

  components:    files/<fid>/components/<cid>.json    (skip tombstones)
  colors:        files/<fid>/colors/<cid>.json
  typographies:  files/<fid>/typographies/<tid>.json
  shapes:        files/<fid>/pages/<pid>/<sid>.json   (id -> shape)

Then, per component key present in both libs:
  componentId(old) -> componentId(new)
and for the component's MAIN-INSTANCE SUBTREE, walk both subtrees in lockstep
(depth-first, children in the parent's `shapes` order) and pair shape ids BY
STRUCTURAL POSITION, sanity-checking (name, type) at every position. That
positional pairing is what maps NESTED `shapeRef`s (a real copied instance
subtree carries one per shape). Mismatched structure at any position is
reported, and that component's subtree map is dropped (honesty over guesses).
Colors and typographies join 1:1 on their keys into colorIdMap /
typographyIdMap.

REFUSAL CONTRACT ("refuses rather than guesses"):
  - a key duplicated within EITHER tree (same path+name+variantProperties on
    two live components, or same path+name on two colors/typographies) is
    EXCLUDED from the map and recorded in `duplicates` — the surviving
    representative would otherwise be os.walk-order nondeterministic. If a
    consumer ref needs an excluded key, `rewrite` records it unmappable (with
    the duplicate named) and exits nonzero. `derive-map` standalone exits
    nonzero whenever duplicates exist (a map with silently ambiguous keys is
    not a map).
  - structural mismatch / subtree size difference / key missing on either
    side -> named problem, nonzero exit.

KNOWN HOLE (named, not handled): component-swap slots. `touched` entries of
the form `swap-slot-<uuid>` embed a component id that this tool neither
rewrites nor verifies. See docs/ecosystem-spikes/library-portability.md
caveat 1.

Rewrite pass
------------
Over every `.json` in the consumer tree, recursively:
  - dict with componentFile == oldLibFileId:
        componentFile -> newLibFileId
        componentId   -> mapped (else recorded unmappable)
        shapeRef      -> mapped (else recorded unmappable)
  - dict with a bare shapeRef in the shape map (nested copy shape):
        shapeRef      -> mapped (a bare shapeRef hitting an EXCLUDED
                         duplicate-key subtree is recorded unmappable)
  - dict with *RefFile == oldLibFileId (fill / stroke / text content node):
        *RefFile      -> newLibFileId
        *RefId        -> mapped through colorIdMap / typographyIdMap
                         (else recorded unmappable)
Everything else — including the consumer's own file id and internal shape ids
(import-as-new remaps those anyway) — is untouched.

CLI
---
  e6_rewrite.py derive-map <old_lib_tree> <new_lib_tree> [-o map.json]
  e6_rewrite.py rewrite    <consumer_tree_in> <old_lib_tree> <new_lib_tree>
                           <out_tree> [-o report.json]
  e6_rewrite.py verify     <consumer_tree> <lib_tree> [-o report.json]
      Static zero-dangling check: every componentFile equals the lib tree's
      file id, every componentId is a live component there, every shapeRef is
      a real shape there, every asset *RefFile equals the lib file id and
      every *RefId is a live color/typography there. The mainInstance
      exemption applies ONLY when componentFile is the consumer's own file id
      (mirrors the live verifier's strictness). Exit 0 iff zero dangling.
  e6_rewrite.py selftest
      Offline, no-stack, tempdir-only proof that the mapping DISCRIMINATES:
      synthetic old/new library trees with genuinely different ids for every
      component/shape/color/typography; asserts correct pairing, the
      dropped-child refusal, the duplicate-key refusal, and the asset-ref
      rewrite + verify on a synthetic consumer. Wired into the E6 gate as an
      early offline leg.
"""
import argparse
import json
import os
import sys

# (refFileField, refIdField, kind) — kind selects the id map / lib id set.
ASSET_REF_FIELDS = (
    ("fillColorRefFile", "fillColorRefId", "color"),
    ("strokeColorRefFile", "strokeColorRefId", "color"),
    ("typographyRefFile", "typographyRefId", "typography"),
)
ASSET_MAP_NAME = {"color": "colorIdMap", "typography": "typographyIdMap"}

# ---------------------------------------------------------------- tree access


def tree_files(root):
    """{relpath: bytes} for every file under root ('/' separators)."""
    out = {}
    for dirpath, _dirs, files in os.walk(root):
        for fn in files:
            p = os.path.join(dirpath, fn)
            rel = os.path.relpath(p, root).replace(os.sep, "/")
            with open(p, "rb") as fh:
                out[rel] = fh.read()
    return out


def load_json(files, rel):
    return json.loads(files[rel].decode("utf-8"))


def lib_file_id(files):
    """The tree's own file id, from the binfile manifest."""
    man = load_json(files, "manifest.json")
    ids = [f["id"] for f in man.get("files", [])]
    if len(ids) != 1:
        raise SystemExit(f"expected a single-file tree, manifest has {ids}")
    return ids[0]


# ------------------------------------------------------- library extraction


def component_key(comp):
    """E1's uuid-stable component identity: path + name + variantProperties.
    NEVER an id/variantId (those are remapped by import-as-new)."""
    props = comp.get("variantProperties") or []
    props = sorted(
        [(str(p.get("name", "")), str(p.get("value", ""))) for p in props]
    )
    return json.dumps(
        {"path": comp.get("path") or "", "name": comp.get("name") or "",
         "variantProperties": props},
        sort_keys=True)


def asset_key(obj):
    """E1's uuid-stable exported color/typography identity: path + name."""
    return json.dumps(
        {"path": obj.get("path") or "", "name": obj.get("name") or ""},
        sort_keys=True)


def _split_buckets(buckets):
    """buckets {key: [objs]} -> (uniq {key: obj}, dup_keys {key: [ids]}).
    Duplicate keys are EXCLUDED from uniq — a silently-picked survivor would
    be os.walk-order nondeterministic (the tool refuses, never guesses)."""
    uniq, dups = {}, {}
    for k, lst in buckets.items():
        if len(lst) == 1:
            uniq[k] = lst[0]
        else:
            dups[k] = sorted(str(o.get("id")) for o in lst)
    return uniq, dups


def extract_library(tree_dir):
    """-> {fileId, components, colors, typographies, shapes, dupKeys,
    componentIds, colorIds, typographyIds, buckets}. The keyed dicts contain
    UNIQUE keys only; duplicated keys land in dupKeys[kind] (see
    _split_buckets). The *Ids sets cover ALL live objects (dups included) —
    the verify half checks liveness by id, not by key."""
    files = tree_files(tree_dir)
    fid = lib_file_id(files)
    cprefix = f"files/{fid}/components/"
    colprefix = f"files/{fid}/colors/"
    typrefix = f"files/{fid}/typographies/"
    pprefix = f"files/{fid}/pages/"
    buckets = {"component": {}, "color": {}, "typography": {}}
    shapes = {}
    for rel in files:
        if not rel.endswith(".json"):
            continue
        if rel.startswith(cprefix):
            c = load_json(files, rel)
            if isinstance(c, dict) and not c.get("deleted"):
                buckets["component"].setdefault(
                    component_key(c), []).append(c)
        elif rel.startswith(colprefix):
            c = load_json(files, rel)
            if isinstance(c, dict) and not c.get("deleted"):
                buckets["color"].setdefault(asset_key(c), []).append(c)
        elif rel.startswith(typrefix):
            t = load_json(files, rel)
            if isinstance(t, dict) and not t.get("deleted"):
                buckets["typography"].setdefault(asset_key(t), []).append(t)
        elif rel.startswith(pprefix):
            # shapes are files/<fid>/pages/<pid>/<sid>.json (5 segments);
            # the page doc itself is pages/<pid>.json (4 segments).
            if len(rel.split("/")) != 5:
                continue
            s = load_json(files, rel)
            if isinstance(s, dict) and "id" in s:
                shapes[s["id"]] = s
    components, cdup = _split_buckets(buckets["component"])
    colors, coldup = _split_buckets(buckets["color"])
    typographies, tdup = _split_buckets(buckets["typography"])
    return {
        "fileId": fid,
        "components": components,
        "colors": colors,
        "typographies": typographies,
        "shapes": shapes,
        "dupKeys": {"component": cdup, "color": coldup, "typography": tdup},
        "componentIds": {o["id"] for lst in buckets["component"].values()
                         for o in lst},
        "colorIds": {o["id"] for lst in buckets["color"].values()
                     for o in lst},
        "typographyIds": {o["id"] for lst in buckets["typography"].values()
                          for o in lst},
        "buckets": buckets,
    }


def subtree_in_order(shapes, root_id):
    """Depth-first (root, then children in the parent's `shapes` vector order)
    list of (positionPath, shape). Deterministic — the structural spine the
    nested-shapeRef pairing rides on."""
    out = []

    def walk(sid, pos):
        shape = shapes.get(sid)
        if shape is None:
            return
        out.append((pos, shape))
        for i, child in enumerate(shape.get("shapes") or []):
            walk(child, f"{pos}/{i}")

    walk(root_id, "0")
    return out


# ------------------------------------------------------------- map derivation


def _collect_duplicates(old, new):
    """Duplicate-key report entries + the excluded-old-id reason index.
    excluded maps an OLD-side id -> human reason, per ref class, so the
    rewrite can name the duplicate when it refuses."""
    duplicates = []
    excluded = {"componentId": {}, "shapeRef": {}, "color": {},
                "typography": {}}
    for kind in ("component", "color", "typography"):
        keys = set(old["dupKeys"][kind]) | set(new["dupKeys"][kind])
        for key in sorted(keys):
            where = [side for side, lib in (("old", old), ("new", new))
                     if key in lib["dupKeys"][kind]]
            duplicates.append({
                "kind": kind, "key": json.loads(key), "where": where,
                "oldIds": old["dupKeys"][kind].get(key, []),
                "newIds": new["dupKeys"][kind].get(key, []),
            })
            reason = (f"identity key duplicated in {'/'.join(where)} "
                      f"library ({kind}): {key}")
            # every OLD-side object under this key is unmappable-by-refusal
            old_objs = old["buckets"][kind].get(key, [])
            for obj in old_objs:
                if kind == "component":
                    excluded["componentId"][obj["id"]] = reason
                    for _, sh in subtree_in_order(
                            old["shapes"], obj.get("mainInstanceId")):
                        excluded["shapeRef"][sh["id"]] = reason
                else:
                    excluded[kind][obj["id"]] = reason
    return duplicates, excluded


def derive_map(old_dir, new_dir):
    """Join the two libraries' components/colors/typographies on uuid-stable
    keys and pair every main-instance subtree shape by structural position.
    Keys duplicated in either tree are excluded (see _collect_duplicates)."""
    old = extract_library(old_dir)
    new = extract_library(new_dir)
    file_id_map = {old["fileId"]: new["fileId"]}
    component_id_map, shape_id_map = {}, {}
    color_id_map, typography_id_map = {}, {}
    entries, problems = [], []
    duplicates, excluded = _collect_duplicates(old, new)
    excluded_keys = {
        kind: set(old["dupKeys"][kind]) | set(new["dupKeys"][kind])
        for kind in ("component", "color", "typography")}

    for key, oc in sorted(old["components"].items()):
        if key in excluded_keys["component"]:
            continue  # duplicate key: excluded, reported in `duplicates`
        nc = new["components"].get(key)
        if nc is None:
            problems.append({"key": json.loads(key),
                             "problem": "component missing in new library"})
            continue
        component_id_map[oc["id"]] = nc["id"]
        osub = subtree_in_order(old["shapes"], oc.get("mainInstanceId"))
        nsub = subtree_in_order(new["shapes"], nc.get("mainInstanceId"))
        entry = {
            "key": json.loads(key),
            "old": {"componentId": oc["id"],
                    "mainInstanceId": oc.get("mainInstanceId"),
                    "subtreeShapeIds": [s["id"] for _, s in osub]},
            "new": {"componentId": nc["id"],
                    "mainInstanceId": nc.get("mainInstanceId"),
                    "subtreeShapeIds": [s["id"] for _, s in nsub]},
            "structuralMatch": True,
            "subtreeDepth": max(
                (p.count("/") + 1 for p, _ in osub), default=0),
        }
        if len(osub) != len(nsub):
            entry["structuralMatch"] = False
            problems.append({"key": json.loads(key),
                             "problem": f"subtree size differs "
                                        f"({len(osub)} vs {len(nsub)})"})
        else:
            for (opos, oshape), (npos, nshape) in zip(osub, nsub):
                if (opos != npos
                        or oshape.get("name") != nshape.get("name")
                        or oshape.get("type") != nshape.get("type")):
                    entry["structuralMatch"] = False
                    problems.append({
                        "key": json.loads(key),
                        "problem": "structural mismatch at position "
                                   f"{opos}: ({oshape.get('name')!r},"
                                   f" {oshape.get('type')!r}) vs"
                                   f" ({nshape.get('name')!r},"
                                   f" {nshape.get('type')!r})"})
                    break
            if entry["structuralMatch"]:
                for (_, oshape), (_, nshape) in zip(osub, nsub):
                    shape_id_map[oshape["id"]] = nshape["id"]
        entries.append(entry)

    for key in sorted(new["components"]):
        if (key not in old["components"]
                and key not in excluded_keys["component"]):
            problems.append({"key": json.loads(key),
                             "problem": "component missing in old library"})

    for kind, field, id_map in (("color", "colors", color_id_map),
                                ("typography", "typographies",
                                 typography_id_map)):
        for key, oa in sorted(old[field].items()):
            if key in excluded_keys[kind]:
                continue
            na = new[field].get(key)
            if na is None:
                problems.append({"key": json.loads(key),
                                 "problem": f"{kind} missing in new library"})
                continue
            id_map[oa["id"]] = na["id"]
        for key in sorted(new[field]):
            if (key not in old[field]
                    and key not in excluded_keys[kind]):
                problems.append({"key": json.loads(key),
                                 "problem": f"{kind} missing in old library"})

    return {
        "oldFileId": old["fileId"],
        "newFileId": new["fileId"],
        "fileIdMap": file_id_map,
        "componentIdMap": component_id_map,
        "shapeIdMap": shape_id_map,
        "colorIdMap": color_id_map,
        "typographyIdMap": typography_id_map,
        "components": entries,
        "problems": problems,
        "duplicates": duplicates,
        "excluded": excluded,
    }


# ------------------------------------------------------------------ rewrite


def _unmappable(stats, idmap, field, value, kind):
    ent = {"field": field, "value": value}
    reason = idmap.get("excluded", {}).get(kind, {}).get(value)
    if reason:
        ent["reason"] = reason
    stats["unmappable"].append(ent)


def _visit(node, idmap, stats):
    """Recursively rewrite one decoded JSON value in place."""
    if isinstance(node, dict):
        cf = node.get("componentFile")
        if cf is not None and cf in idmap["fileIdMap"]:
            node["componentFile"] = idmap["fileIdMap"][cf]
            stats["componentFileRewritten"] += 1
            cid = node.get("componentId")
            if cid is not None:
                mapped = idmap["componentIdMap"].get(cid)
                if mapped is None:
                    _unmappable(stats, idmap, "componentId", cid,
                                "componentId")
                else:
                    node["componentId"] = mapped
                    stats["componentIdRewritten"] += 1
            sref = node.get("shapeRef")
            if sref is not None:
                mapped = idmap["shapeIdMap"].get(sref)
                if mapped is None:
                    _unmappable(stats, idmap, "shapeRef", sref, "shapeRef")
                else:
                    node["shapeRef"] = mapped
                    stats["shapeRefRewritten"] += 1
        else:
            # nested copy shape: bare shapeRef into the library subtree
            sref = node.get("shapeRef")
            if sref is not None:
                if sref in idmap["shapeIdMap"]:
                    node["shapeRef"] = idmap["shapeIdMap"][sref]
                    stats["shapeRefRewritten"] += 1
                elif sref in idmap.get("excluded", {}).get("shapeRef", {}):
                    # a bare ref into an EXCLUDED duplicate-key subtree must
                    # refuse loudly, not silently pass through
                    _unmappable(stats, idmap, "shapeRef", sref, "shapeRef")
        # library ASSET refs (fills / strokes / text content nodes)
        for ff, fi, kind in ASSET_REF_FIELDS:
            rf = node.get(ff)
            if rf is not None and rf in idmap["fileIdMap"]:
                node[ff] = idmap["fileIdMap"][rf]
                stats["assetRefFileRewritten"] += 1
                rid = node.get(fi)
                if rid is not None:
                    mapped = idmap[ASSET_MAP_NAME[kind]].get(rid)
                    if mapped is None:
                        _unmappable(stats, idmap, fi, rid, kind)
                    else:
                        node[fi] = mapped
                        stats["assetRefIdRewritten"] += 1
        for v in node.values():
            _visit(v, idmap, stats)
    elif isinstance(node, list):
        for v in node:
            _visit(v, idmap, stats)


def rewrite_tree(consumer_in, out_dir, idmap):
    """Copy consumer_in -> out_dir with every library ref rewritten. Returns
    the rewrite stats. Output .json files keep the normalization spec
    (sorted keys, 2-space indent, ensure_ascii=False, LF, trailing \\n)."""
    files = tree_files(consumer_in)
    stats = {"componentFileRewritten": 0, "componentIdRewritten": 0,
             "shapeRefRewritten": 0, "assetRefFileRewritten": 0,
             "assetRefIdRewritten": 0, "filesTouched": 0, "unmappable": []}
    if os.path.exists(out_dir):
        import shutil
        shutil.rmtree(out_dir)
    for rel, content in sorted(files.items()):
        if rel.endswith(".json"):
            data = json.loads(content.decode("utf-8"))
            before = json.dumps(data, sort_keys=True)
            _visit(data, idmap, stats)
            if json.dumps(data, sort_keys=True) != before:
                stats["filesTouched"] += 1
            content = (json.dumps(data, sort_keys=True, indent=2,
                                  ensure_ascii=False) + "\n").encode("utf-8")
        p = os.path.join(out_dir, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "wb") as fh:
            fh.write(content)
    return stats


# ------------------------------------------------------------------- verify


def collect_refs(node, own_file_id, acc):
    """Collect every library ref in a decoded JSON value: dicts carrying
    componentFile (instance roots), dicts carrying a bare shapeRef (nested
    copy shapes), and dicts carrying asset *RefFile/*RefId pairs (fills,
    strokes, text content nodes). Refs into the file itself are recorded but
    flagged `own`."""
    if isinstance(node, dict):
        if "componentFile" in node:
            acc.append({
                "shapeId": node.get("id"),
                "componentFile": node.get("componentFile"),
                "componentId": node.get("componentId"),
                "shapeRef": node.get("shapeRef"),
                "own": node.get("componentFile") == own_file_id,
                "mainInstance": bool(node.get("mainInstance")),
            })
        elif "shapeRef" in node:
            acc.append({"shapeId": node.get("id"), "componentFile": None,
                        "componentId": None, "shapeRef": node.get("shapeRef"),
                        "own": False, "mainInstance": False})
        for ff, fi, kind in ASSET_REF_FIELDS:
            if ff in node or fi in node:
                # *RefFile absent with *RefId present does not occur in
                # Penpot-authored data (a local-library ref carries the own
                # file id, not nil); refFile None is treated as own/local.
                rf = node.get(ff)
                acc.append({"asset": True, "kind": kind,
                            "fileField": ff, "idField": fi,
                            "shapeId": node.get("id"),
                            "refFile": rf, "refId": node.get(fi),
                            "own": rf is None or rf == own_file_id})
        for v in node.values():
            collect_refs(v, own_file_id, acc)
    elif isinstance(node, list):
        for v in node:
            collect_refs(v, own_file_id, acc)


def verify_tree(consumer_dir, lib_dir):
    """Zero-dangling check of a consumer tree against a library tree. A ref is
    dangling when componentFile != the library's file id, componentId is not a
    live component there, shapeRef is not a real shape there, or an asset
    *RefFile/*RefId pair does not resolve to a live color/typography there.
    Own-file refs are exempt; the mainInstance exemption applies ONLY when
    componentFile equals the consumer's own file id (i.e. it is subsumed by
    `own` — a mainInstance flag on a FOREIGN componentFile is NOT exempt,
    mirroring the live verifier)."""
    lib = extract_library(lib_dir)
    lib_component_ids = lib["componentIds"]
    lib_shape_ids = set(lib["shapes"].keys())
    lib_asset_ids = {"color": lib["colorIds"],
                     "typography": lib["typographyIds"]}
    files = tree_files(consumer_dir)
    own_id = lib_file_id(files)

    refs = []
    for rel, content in sorted(files.items()):
        if not rel.endswith(".json"):
            continue
        collect_refs(json.loads(content.decode("utf-8")), own_id, refs)

    dangling, resolved = [], 0
    for r in refs:
        if r["own"]:
            continue  # the file's own component/asset machinery
        bad = []
        if r.get("asset"):
            if r["refFile"] is not None and r["refFile"] != lib["fileId"]:
                bad.append(r["fileField"])
            if (r["refId"] is not None
                    and r["refId"] not in lib_asset_ids[r["kind"]]):
                bad.append(r["idField"])
        else:
            if (r["componentFile"] is not None
                    and r["componentFile"] != lib["fileId"]):
                bad.append("componentFile")
            if (r["componentId"] is not None
                    and r["componentId"] not in lib_component_ids):
                bad.append("componentId")
            if r["shapeRef"] is not None and r["shapeRef"] not in lib_shape_ids:
                bad.append("shapeRef")
        if bad:
            dangling.append({**r, "danglingFields": bad})
        else:
            resolved += 1
    return {
        "libFileId": lib["fileId"],
        "consumerFileId": own_id,
        "refsChecked": resolved + len(dangling),
        "assetRefs": sum(1 for r in refs if r.get("asset") and not r["own"]),
        "resolved": resolved,
        "dangling": dangling,
        "zeroDangling": len(dangling) == 0,
    }


# ------------------------------------------------------------------ selftest


def _st_write_tree(root, files):
    for rel, obj in files.items():
        p = os.path.join(root, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "w", encoding="utf-8") as fh:
            json.dump(obj, fh, sort_keys=True, indent=2, ensure_ascii=False)
            fh.write("\n")


def _st_lib_files(ids, drop_icon=False, dup_badge=False):
    """Synthetic library tree content for one vault's minting `ids`:
    {fid, pid, btnComp, btnMain, btnBg, btnIcon, badgeComp, badgeMain,
    colorId, typoId} (+ badgeComp2/badgeMain2 when dup_badge)."""
    fid, pid = ids["fid"], ids["pid"]
    btn_children = [ids["btnBg"]] + ([] if drop_icon else [ids["btnIcon"]])
    files = {
        "manifest.json": {"files": [{"id": fid}]},
        f"files/{fid}/pages/{pid}.json": {"id": pid, "name": "Page 1"},
        f"files/{fid}/pages/{pid}/{ids['btnMain']}.json": {
            "id": ids["btnMain"], "type": "frame", "name": "Button",
            "shapes": btn_children, "mainInstance": True,
            "componentFile": fid, "componentId": ids["btnComp"]},
        f"files/{fid}/pages/{pid}/{ids['btnBg']}.json": {
            "id": ids["btnBg"], "type": "rect", "name": "BG"},
        f"files/{fid}/pages/{pid}/{ids['badgeMain']}.json": {
            "id": ids["badgeMain"], "type": "rect", "name": "Badge",
            "mainInstance": True, "componentFile": fid,
            "componentId": ids["badgeComp"]},
        f"files/{fid}/components/{ids['btnComp']}.json": {
            "id": ids["btnComp"], "name": "Primary",
            "path": "Controls / Button", "mainInstanceId": ids["btnMain"],
            "variantProperties": [{"name": "State", "value": "Default"}]},
        f"files/{fid}/components/{ids['badgeComp']}.json": {
            "id": ids["badgeComp"], "name": "Badge", "path": "Controls",
            "mainInstanceId": ids["badgeMain"]},
        f"files/{fid}/colors/{ids['colorId']}.json": {
            "id": ids["colorId"], "name": "Brand Teal",
            "color": "#12b886", "opacity": 1},
        f"files/{fid}/typographies/{ids['typoId']}.json": {
            "id": ids["typoId"], "name": "Heading XL",
            "fontId": "sourcesanspro", "fontSize": "36"},
    }
    if not drop_icon:
        files[f"files/{fid}/pages/{pid}/{ids['btnIcon']}.json"] = {
            "id": ids["btnIcon"], "type": "rect", "name": "Icon"}
    if dup_badge:
        files[f"files/{fid}/components/{ids['badgeComp2']}.json"] = {
            "id": ids["badgeComp2"], "name": "Badge", "path": "Controls",
            "mainInstanceId": ids["badgeMain2"]}
        files[f"files/{fid}/pages/{pid}/{ids['badgeMain2']}.json"] = {
            "id": ids["badgeMain2"], "type": "rect", "name": "Badge",
            "mainInstance": True, "componentFile": fid,
            "componentId": ids["badgeComp2"]}
    return files


def _st_consumer_files(cons_fid, lib):
    """Synthetic consumer referencing the OLD library: a root-only instance,
    a nested-subtree instance, a color-styled rect (fill + stroke), and a
    text shape whose content nodes carry typography + fill refs."""
    pid = "cccccccc-0000-0000-0000-0000000000aa"
    i_root = "cccccccc-0000-0000-0000-000000000001"
    i_nest = "cccccccc-0000-0000-0000-000000000002"
    i_bg = "cccccccc-0000-0000-0000-000000000003"
    i_icon = "cccccccc-0000-0000-0000-000000000004"
    i_styled = "cccccccc-0000-0000-0000-000000000005"
    i_text = "cccccccc-0000-0000-0000-000000000006"
    fid = lib["fid"]
    return {
        "manifest.json": {"files": [{"id": cons_fid}]},
        f"files/{cons_fid}/pages/{pid}.json": {"id": pid, "name": "Page 1"},
        f"files/{cons_fid}/pages/{pid}/{i_root}.json": {
            "id": i_root, "type": "rect", "name": "Badge (instance)",
            "componentRoot": True, "componentFile": fid,
            "componentId": lib["badgeComp"], "shapeRef": lib["badgeMain"]},
        f"files/{cons_fid}/pages/{pid}/{i_nest}.json": {
            "id": i_nest, "type": "frame", "name": "Button (instance)",
            "componentRoot": True, "componentFile": fid,
            "componentId": lib["btnComp"], "shapeRef": lib["btnMain"],
            "shapes": [i_bg, i_icon]},
        f"files/{cons_fid}/pages/{pid}/{i_bg}.json": {
            "id": i_bg, "type": "rect", "name": "BG",
            "shapeRef": lib["btnBg"]},
        f"files/{cons_fid}/pages/{pid}/{i_icon}.json": {
            "id": i_icon, "type": "rect", "name": "Icon",
            "shapeRef": lib["btnIcon"]},
        f"files/{cons_fid}/pages/{pid}/{i_styled}.json": {
            "id": i_styled, "type": "rect", "name": "Styled",
            "fills": [{"fillColor": "#12b886", "fillOpacity": 1,
                       "fillColorRefId": lib["colorId"],
                       "fillColorRefFile": fid}],
            "strokes": [{"strokeColor": "#12b886", "strokeOpacity": 1,
                         "strokeWidth": 2, "strokeAlignment": "center",
                         "strokeStyle": "solid",
                         "strokeColorRefId": lib["colorId"],
                         "strokeColorRefFile": fid}]},
        f"files/{cons_fid}/pages/{pid}/{i_text}.json": {
            "id": i_text, "type": "text", "name": "Styled Text",
            "content": {"type": "root", "children": [
                {"type": "paragraph-set", "children": [
                    {"type": "paragraph",
                     "typographyRefId": lib["typoId"],
                     "typographyRefFile": fid,
                     "children": [
                         {"text": "Styled",
                          "typographyRefId": lib["typoId"],
                          "typographyRefFile": fid,
                          "fills": [{"fillColor": "#000000",
                                     "fillOpacity": 1,
                                     "fillColorRefId": lib["colorId"],
                                     "fillColorRefFile": fid}]}]}]}]}},
    }


def selftest():
    """Offline no-stack proof that the mapping DISCRIMINATES and every
    refusal fires. Exit 0 iff all checks pass."""
    import shutil
    import tempfile
    failures = []

    def check(cond, msg):
        print(("PASS: " if cond else "FAIL: ") + "selftest: " + msg)
        if not cond:
            failures.append(msg)

    old_ids = {"fid": "aaaaaaaa-0000-0000-0000-0000000000ff",
               "pid": "aaaaaaaa-0000-0000-0000-0000000000aa",
               "btnComp": "aaaaaaaa-0000-0000-0000-000000000001",
               "btnMain": "aaaaaaaa-0000-0000-0000-000000000002",
               "btnBg": "aaaaaaaa-0000-0000-0000-000000000003",
               "btnIcon": "aaaaaaaa-0000-0000-0000-000000000004",
               "badgeComp": "aaaaaaaa-0000-0000-0000-000000000005",
               "badgeMain": "aaaaaaaa-0000-0000-0000-000000000006",
               "colorId": "aaaaaaaa-0000-0000-0000-000000000007",
               "typoId": "aaaaaaaa-0000-0000-0000-000000000008",
               "badgeComp2": "aaaaaaaa-0000-0000-0000-000000000009",
               "badgeMain2": "aaaaaaaa-0000-0000-0000-000000000010"}
    # genuinely different ids for EVERYTHING on the new side
    new_ids = {k: v.replace("aaaaaaaa", "bbbbbbbb")
               for k, v in old_ids.items()}
    cons_fid = "cccccccc-0000-0000-0000-0000000000ff"

    tmp = tempfile.mkdtemp(prefix="e6-selftest.")
    try:
        d_old = os.path.join(tmp, "old.penpot")
        d_new = os.path.join(tmp, "new.penpot")
        d_cons = os.path.join(tmp, "consumer.penpot")
        d_out = os.path.join(tmp, "rewritten.penpot")
        _st_write_tree(d_old, _st_lib_files(old_ids))
        _st_write_tree(d_new, _st_lib_files(new_ids))
        _st_write_tree(d_cons, _st_consumer_files(cons_fid, old_ids))

        # (i) derive_map pairs genuinely different ids correctly
        m = derive_map(d_old, d_new)
        check(not m["problems"] and not m["duplicates"],
              "clean fixture derives with no problems/duplicates")
        check(m["componentIdMap"] == {
                  old_ids["btnComp"]: new_ids["btnComp"],
                  old_ids["badgeComp"]: new_ids["badgeComp"]},
              "componentIdMap pairs across genuinely different ids")
        check(m["shapeIdMap"] == {
                  old_ids["btnMain"]: new_ids["btnMain"],
                  old_ids["btnBg"]: new_ids["btnBg"],
                  old_ids["btnIcon"]: new_ids["btnIcon"],
                  old_ids["badgeMain"]: new_ids["badgeMain"]},
              "shapeIdMap lockstep-pairs the main-instance subtrees")
        check(m["colorIdMap"] == {old_ids["colorId"]: new_ids["colorId"]},
              "colorIdMap joins exported colors by name")
        check(m["typographyIdMap"] == {
                  old_ids["typoId"]: new_ids["typoId"]},
              "typographyIdMap joins exported typographies by name")

        # (ii) the verifier DISCRIMINATES: the unrewritten consumer dangles,
        # and the dangle set enumerates the asset-ref fields
        pre = verify_tree(d_cons, d_new)
        pre_fields = {f for d in pre["dangling"] for f in d["danglingFields"]}
        check(not pre["zeroDangling"],
              f"unrewritten consumer dangles against the new library "
              f"({len(pre['dangling'])} refs)")
        check({"componentFile", "componentId", "shapeRef",
               "fillColorRefFile", "fillColorRefId",
               "strokeColorRefFile", "strokeColorRefId",
               "typographyRefFile", "typographyRefId"} <= pre_fields,
              "static verify enumerates instance AND asset ref fields")

        # (iii) rewrite maps all six asset fields + instance machinery
        stats = rewrite_tree(d_cons, d_out, m)
        post = verify_tree(d_out, d_new)
        check(not stats["unmappable"] and post["zeroDangling"],
              "rewrite -> zero dangling (static) with nothing unmappable")
        check(stats["assetRefFileRewritten"] == 5
              and stats["assetRefIdRewritten"] == 5,
              f"asset refs rewritten (file={stats['assetRefFileRewritten']}"
              f" id={stats['assetRefIdRewritten']}; fill+stroke+2xtypo+"
              "content-fill span two shapes)")
        styled = json.load(open(os.path.join(
            d_out, f"files/{cons_fid}/pages/"
                   "cccccccc-0000-0000-0000-0000000000aa/"
                   "cccccccc-0000-0000-0000-000000000005.json")))
        text = json.load(open(os.path.join(
            d_out, f"files/{cons_fid}/pages/"
                   "cccccccc-0000-0000-0000-0000000000aa/"
                   "cccccccc-0000-0000-0000-000000000006.json")))
        para = text["content"]["children"][0]["children"][0]
        span = para["children"][0]
        check(styled["fills"][0]["fillColorRefId"] == new_ids["colorId"]
              and styled["fills"][0]["fillColorRefFile"] == new_ids["fid"]
              and styled["strokes"][0]["strokeColorRefId"]
              == new_ids["colorId"],
              "fill/stroke color refs point at the NEW library's color")
        check(para["typographyRefId"] == new_ids["typoId"]
              and span["typographyRefId"] == new_ids["typoId"]
              and span["fills"][0]["fillColorRefId"] == new_ids["colorId"],
              "typography + fill refs inside text CONTENT nodes rewritten")

        # (iv) dropped child -> subtree-size refusal
        d_drop = os.path.join(tmp, "new-dropped.penpot")
        _st_write_tree(d_drop, _st_lib_files(new_ids, drop_icon=True))
        md = derive_map(d_old, d_drop)
        check(any("subtree size differs" in p["problem"]
                  for p in md["problems"]),
              "dropped child refused with 'subtree size differs'")
        check(old_ids["btnMain"] not in md["shapeIdMap"],
              "mismatched subtree contributes NO shape pairs")

        # (v) duplicate E1 key -> exclusion + refusal when referenced
        d_dup = os.path.join(tmp, "old-dup.penpot")
        _st_write_tree(d_dup, _st_lib_files(old_ids, dup_badge=True))
        mdup = derive_map(d_dup, d_new)
        dup_kinds = {d["kind"] for d in mdup["duplicates"]}
        check("component" in dup_kinds
              and old_ids["badgeComp"] not in mdup["componentIdMap"],
              "duplicate component key excluded from the map + reported")
        d_out2 = os.path.join(tmp, "rewritten-dup.penpot")
        stats2 = rewrite_tree(d_cons, d_out2, mdup)
        dup_unmap = [u for u in stats2["unmappable"]
                     if "duplicated" in u.get("reason", "")]
        refused = bool(mdup["problems"]) or bool(stats2["unmappable"])
        check(refused and dup_unmap,
              "consumer ref into the duplicated key REFUSED (unmappable "
              "names the duplicate; rewrite exits nonzero)")

        # (vi) duplicate color name in the NEW tree -> asset refusal
        dup2_ids = dict(new_ids)
        d_dupcol = os.path.join(tmp, "new-dupcol.penpot")
        files = _st_lib_files(dup2_ids)
        files[f"files/{dup2_ids['fid']}/colors/"
              "bbbbbbbb-0000-0000-0000-000000000099.json"] = {
            "id": "bbbbbbbb-0000-0000-0000-000000000099",
            "name": "Brand Teal", "color": "#0ca678", "opacity": 1}
        _st_write_tree(d_dupcol, files)
        mcol = derive_map(d_old, d_dupcol)
        d_out3 = os.path.join(tmp, "rewritten-dupcol.penpot")
        stats3 = rewrite_tree(d_cons, d_out3, mcol)
        check(old_ids["colorId"] not in mcol["colorIdMap"]
              and any(u["field"] == "fillColorRefId"
                      and "duplicated" in u.get("reason", "")
                      for u in stats3["unmappable"]),
              "duplicate color name refused when a consumer fill needs it")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    if failures:
        print(f"SELFTEST: {len(failures)} FAILURE(S)")
        return 1
    print("SELFTEST: ALL PASS")
    return 0


# ---------------------------------------------------------------------- CLI


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)

    d = sub.add_parser("derive-map")
    d.add_argument("old_lib")
    d.add_argument("new_lib")
    d.add_argument("-o", "--out")

    r = sub.add_parser("rewrite")
    r.add_argument("consumer_in")
    r.add_argument("old_lib")
    r.add_argument("new_lib")
    r.add_argument("out_tree")
    r.add_argument("-o", "--out")

    v = sub.add_parser("verify")
    v.add_argument("consumer")
    v.add_argument("lib")
    v.add_argument("-o", "--out")

    sub.add_parser("selftest")

    args = ap.parse_args()

    if args.cmd == "selftest":
        return selftest()

    if args.cmd == "derive-map":
        report = derive_map(args.old_lib, args.new_lib)
        # standalone map derivation refuses on ambiguity too: a map with
        # silently-excluded duplicate keys is not a usable map
        ok = not report["problems"] and not report["duplicates"]
    elif args.cmd == "rewrite":
        idmap = derive_map(args.old_lib, args.new_lib)
        stats = rewrite_tree(args.consumer_in, args.out_tree, idmap)
        post = verify_tree(args.out_tree, args.new_lib)
        report = {"map": idmap, "rewrite": stats, "postVerify": post}
        # duplicates refuse CONDITIONALLY here: only when a consumer ref
        # actually needs an excluded key (it then lands in unmappable)
        ok = (not idmap["problems"] and not stats["unmappable"]
              and post["zeroDangling"])
    else:  # verify
        report = verify_tree(args.consumer, args.lib)
        ok = report["zeroDangling"]

    text = json.dumps(report, indent=2, sort_keys=True)
    if args.out:
        with open(args.out, "w") as fh:
            fh.write(text + "\n")
    print(text)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
