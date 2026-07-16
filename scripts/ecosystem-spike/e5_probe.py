#!/usr/bin/env python3
"""E5 spike live probe (gate helper for scripts/e5-tokens-spike.sh):
mirror-and-surface token packages on a real Penpot 2.16.2 stack.

Executes, in one boot (the gate script owns the harness):

  A. Import tokens-starter-kit (the bundled template zip) as a real file =
     the PACKAGE. Export + normalize its tree (the resolver's grounding dump).
  B. Author a small CONSUMER file (create-file + a shape carrying
     appliedTokens). MIRROR a subset of the package's DTCG sets into the
     consumer's files/<fid>/tokens.json by editing the exported tree +
     in-place re-import (the explicit tooling verb — never the SPA), stamped
     with provenance:
       - a theme in group "penpot:package" whose "id" (-> tokens_lib
         :external-id) carries source+version and whose selectedTokenSets
         enumerate the mirrored sets                     [expect: SURVIVES]
       - the same theme's "description"                  [expect: BLANKED —
         parse-multi-set-dtcg-json calls make-token-theme without
         :description, so the id is the only theme-level stamp]
       - a "$description" on the consumer's own token    [expect: SURVIVES]
       - an extra "$metadata" key                        [expect: DROPPED]
       - a set-root "$description"                       [expect: REJECTED
         at import by check-multi-set-dtcg-data]
     Prove the merged consumer ROUND-TRIPS A=B (roundtrip.py semantics:
     export -> normalize -> in-place re-import -> re-export -> equal
     SEMANTIC tree hashes) and capture which stamps survived.
  C. COLLISION: two consumer sets define the same bare path with different
     $values; capture that the later set in tokenSetOrder wins, then FLIP the
     order via edit+reimport and capture that the resolved value flips
     (order-is-contract). Winner read from the file's own exported data by
     the static resolver AND the order captured via RPC get-file.
  D. THEME-DEPENDENT RESOLUTION: on the package file, flip
     $metadata.activeThemes "Color mode/Light" -> "Color mode/Dark"
     (edit+reimport, same file id); capture layerBase.text resolving to a
     DIFFERENT color within one file, and the RPC-side activeThemes.
  E. Static resolver headless over the starter-kit dump: (a) per-token deps
     listing, (b) the PRE-EXISTING dangling baseline pinned, (c) a synthetic
     dropped token flagged MAJOR with baseline noise excluded; plus
     shadowing-add (reads-minor-behaves-major) and the live pairs from C/D
     classified (order-flip=MAJOR, theme-only=MAJOR-BEHAVIORAL).
  F. DRIFT: statically mutate a mirrored set in a copy of the consumer tree;
     the drift detector must name the set DRIFTED and prescribe the conflict
     copy (overwrite neither side).

Uses scripts/roundtrip.py as RPC client (rt.Client). Env: PENPOT_BACKEND,
PENPOT_FRONTEND, PENPOT_TOKEN. Usage: e5_probe.py run <work_dir>
"""
import copy
import json
import os
import shutil
import sys
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))  # scripts/ for roundtrip.py
sys.path.insert(0, HERE)

import roundtrip as rt  # noqa: E402
import e5_resolver as res  # noqa: E402

BACKEND = os.environ["PENPOT_BACKEND"]
FRONTEND = os.environ["PENPOT_FRONTEND"]
TOKEN = os.environ["PENPOT_TOKEN"]
rt.BACKEND, rt.FRONTEND, rt.TOKEN = BACKEND, FRONTEND, TOKEN

REPO = os.path.dirname(os.path.dirname(HERE))
TEMPLATE = os.path.join(rt.REPO, "runtime", "backend", "builtin-templates",
                        "tokens-starter-kit")
ROOT = "00000000-0000-0000-0000-000000000000"

MIRROR_SETS = ["Foundations - Fixed", "Foundations - Colors", "Modular Scale",
               "Color theme - Vibrant", "Light - Base", "Dark - Base"]
PKG_VERSION = "tokens-starter-kit@2.16.2-kit"
PROVENANCE_ID = f"pkg:{PKG_VERSION}"

CHECKS = []


def check(name, ok, detail=""):
    CHECKS.append({"name": name, "ok": bool(ok), "detail": detail})
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + (f" — {detail}" if detail else ""))


def export_tree(client, file_id, dest):
    """export-binfile -> unzip -> normalize (M0 spec). Returns dest."""
    zip_bytes = rt.export_binfile(client, file_id)
    rt.unzip_to(zip_bytes, dest)
    rt.normalize_tree(dest)
    return dest


def semantic_hash(tree):
    return rt.tree_hash(rt.semantic_files(rt.tree_files(tree)))


def import_in_place(client, project_id, tree, file_id, name):
    rid = rt.import_binfile(client, project_id, rt.zip_tree(tree),
                            file_id=file_id, name=name)
    assert rid == file_id, f"in-place import churned id {file_id} -> {rid}"
    return rid


def tokens_json_path(tree, fid):
    return os.path.join(tree, "files", fid, "tokens.json")


def read_json(path):
    with open(path) as fh:
        return json.load(fh)


def write_json(path, data):
    with open(path, "w") as fh:
        json.dump(data, fh, sort_keys=True, indent=2, ensure_ascii=False)
        fh.write("\n")


def ensure_tokens_feature(tree, fid):
    """The template ships feature 'design-tokens/v1'; make sure the consumer
    declares it too (manifest + files/<fid>.json) before carrying tokens."""
    for p in (os.path.join(tree, "manifest.json"),
              os.path.join(tree, "files", f"{fid}.json")):
        data = read_json(p)
        if "files" in data and isinstance(data.get("files"), list):
            for f in data["files"]:
                feats = f.setdefault("features", [])
                if "design-tokens/v1" not in feats:
                    feats.append("design-tokens/v1")
        else:
            feats = data.setdefault("features", [])
            if "design-tokens/v1" not in feats:
                feats.append("design-tokens/v1")
        write_json(p, data)


def rpc_tokens_lib_meta(client, file_id):
    """The RPC-observable side: get-file returns data.tokensLib JSON-encoded
    through export-dtcg-json, so tokenSetOrder/activeThemes are the file's
    own data over the wire too."""
    f = client.rpc("get-file", {"id": file_id})
    lib = (f.get("data") or {}).get("tokensLib")
    if isinstance(lib, dict):
        return lib.get("$metadata")
    return None


def rect(sid, name, x, y, extra=None):
    ident = {"a": 1.0, "b": 0.0, "c": 0.0, "d": 1.0, "e": 0.0, "f": 0.0}
    s = {"id": sid, "type": "rect", "name": name,
         "x": x, "y": y, "width": 120, "height": 48, "rotation": 0,
         "selrect": {"x": x, "y": y, "width": 120, "height": 48,
                     "x1": x, "y1": y, "x2": x + 120, "y2": y + 48},
         "points": [{"x": x, "y": y}, {"x": x + 120, "y": y},
                    {"x": x + 120, "y": y + 48}, {"x": x, "y": y + 48}],
         "transform": ident, "transformInverse": ident,
         "parentId": ROOT, "frameId": ROOT,
         "fills": [{"fillColor": "#4c6ef5", "fillOpacity": 1}], "strokes": []}
    if extra:
        s.update(extra)
    return s


def run(work):
    os.makedirs(work, exist_ok=True)
    client = rt.Client()
    prof = client.rpc("get-profile", {})
    project_id = prof["defaultProjectId"]
    out = {"projectId": project_id}

    # ================= A. package import + grounding dump ===================
    with open(TEMPLATE, "rb") as fh:
        template_zip = fh.read()
    pkg_id = rt.import_binfile(client, project_id, template_zip,
                               file_id=None, name="e5-pkg-tokens-starter-kit")
    out["packageFileId"] = pkg_id
    pkg_tree = export_tree(client, pkg_id, os.path.join(work, "pkg.penpot"))
    pkg_tokens = read_json(tokens_json_path(pkg_tree, pkg_id))
    check("A.package-imported-and-dumped",
          len([k for k in pkg_tokens if not k.startswith("$")]) == 14,
          f"file={pkg_id} sets={len(pkg_tokens) - 2}")

    # ================= B. consumer + mirror + round-trip A=B ================
    cons = client.rpc("create-file", {"name": "e5-consumer",
                                      "projectId": project_id})
    cons_id = cons["id"]
    cons_page = cons["data"]["pages"][0]
    sid = str(uuid.uuid4())
    client.rpc("update-file", {
        "id": cons_id, "sessionId": str(uuid.uuid4()),
        "revn": cons["revn"], "vern": cons["vern"], "skipValidate": True,
        "changes": [{"type": "add-obj", "id": sid, "pageId": cons_page,
                     "frameId": ROOT, "parentId": ROOT,
                     "obj": rect(sid, "e5-consumer-rect", 10, 10, extra={
                         "appliedTokens": {"fill": "layerBase.text",
                                           "width": "modular.xl"}})}]})
    out["consumerFileId"] = cons_id

    tree0 = export_tree(client, cons_id, os.path.join(work, "cons0.penpot"))

    # ---- build the mirrored tokens.json (the MIRROR verb, by tooling) ----
    tokens = {}
    for name in MIRROR_SETS:                       # verbatim copies
        tokens[name] = copy.deepcopy(pkg_tokens[name])
    tokens["Consumer"] = {
        "app": {"cta": {"$type": "color", "$value": "{layerBase.highlight}",
                        "$description": f"consumer token; mirrors from {PROVENANCE_ID}"}}}
    # scripted collision: same bare path, two sets, different $values
    tokens["E5 Collide - PkgA"] = {
        "collide": {"winner": {"$type": "color", "$value": "#111111"}}}
    tokens["E5 Collide - PkgB"] = {
        "collide": {"winner": {"$type": "color", "$value": "#222222"}}}

    order = MIRROR_SETS + ["Consumer", "E5 Collide - PkgA", "E5 Collide - PkgB"]
    active_sets = ["Foundations - Fixed", "Foundations - Colors",
                   "Modular Scale", "Color theme - Vibrant", "Light - Base",
                   "Consumer", "E5 Collide - PkgA", "E5 Collide - PkgB"]
    # mirror the package's Light/Dark themes (filtered to mirrored sets) so
    # the consumer is theme-flippable too; + the PROVENANCE theme
    themes = []
    for t in pkg_tokens["$themes"]:
        if t["group"] == "Color mode":
            themes.append({"id": t["id"], "name": t["name"],
                           "group": t["group"], "description": "",
                           "isSource": False,
                           "selectedTokenSets": {
                               s: "enabled"
                               for s in t["selectedTokenSets"]
                               if s in MIRROR_SETS}})
    themes.append({
        "id": PROVENANCE_ID,                      # -> tokens_lib :external-id
        "name": PKG_VERSION,
        "group": res.PROVENANCE_THEME_GROUP,
        "description": json.dumps({"source": "builtin-templates/tokens-starter-kit",
                                   "version": PKG_VERSION, "readOnly": True}),
        "isSource": False,
        "selectedTokenSets": {s: "enabled" for s in MIRROR_SETS}})
    tokens["$themes"] = themes
    tokens["$metadata"] = {
        "tokenSetOrder": order,
        "activeThemes": ["Color mode/Light"],
        "activeSets": active_sets,
        "penpotPackages": {PROVENANCE_ID: MIRROR_SETS},  # negative probe
    }
    ensure_tokens_feature(tree0, cons_id)

    # negative stamp probe FIRST: a set-root "$description" string is not a
    # legal ::node for check-multi-set-dtcg-data — the server REJECTS the
    # whole import (stronger than "dropped"). Capture the live rejection.
    tokens_bad = copy.deepcopy(tokens)
    tokens_bad["E5 Collide - PkgA"]["$description"] = "stamp-on-set-root"
    write_json(tokens_json_path(tree0, cons_id), tokens_bad)
    set_root_rejected, set_root_err = False, ""
    try:
        import_in_place(client, project_id, tree0, cons_id, "e5-consumer")
    except RuntimeError as e:
        set_root_rejected = True
        set_root_err = str(e)[:180]
    check("B.set-root-$description-rejected-at-import(expected)",
          set_root_rejected, set_root_err.replace("\n", " ")[:120])

    # now the real mirror (no set-root stamp)
    write_json(tokens_json_path(tree0, cons_id), tokens)
    import_in_place(client, project_id, tree0, cons_id, "e5-consumer")

    # ---- the A=B round trip (roundtrip.py semantics) ----
    tree_a = export_tree(client, cons_id, os.path.join(work, "consA.penpot"))
    hash_a = semantic_hash(tree_a)
    import_in_place(client, project_id, tree_a, cons_id, "e5-consumer")
    tree_b = export_tree(client, cons_id, os.path.join(work, "consB.penpot"))
    hash_b = semantic_hash(tree_b)
    out["consumerSemanticHashA"] = hash_a
    out["consumerSemanticHashB"] = hash_b
    check("B.merged-consumer-roundtrips-A=B", hash_a == hash_b,
          f"A={hash_a[:16]}… B={hash_b[:16]}…")

    # ---- which provenance stamps survived? ----
    tok_b = read_json(tokens_json_path(tree_b, cons_id))
    themes_b = {t.get("name"): t for t in tok_b.get("$themes", [])}
    prov = themes_b.get(PKG_VERSION)
    out["provenance"] = {
        "themeId": prov.get("id") if prov else None,
        "themeGroup": prov.get("group") if prov else None,
        "themeDescription": prov.get("description") if prov else None,
        "themeSets": sorted((prov or {}).get("selectedTokenSets", {})),
        "tokenDescription": tok_b.get("Consumer", {}).get("app", {})
                                 .get("cta", {}).get("$description"),
        "metadataExtraKeySurvived": "penpotPackages" in tok_b.get("$metadata", {}),
        "setRootDescriptionRejectedAtImport": set_root_rejected,
        "setRootDescriptionError": set_root_err,
    }
    check("B.provenance-theme-id-survives",
          prov is not None and prov.get("id") == PROVENANCE_ID,
          f"$themes[].id={prov.get('id') if prov else None}")
    check("B.provenance-theme-description-blanked(expected)",
          prov is not None and (prov.get("description") or "") == "",
          "2.16.2 import omits :description on make-token-theme — theme id "
          "(external-id) is the only surviving theme-level stamp")
    check("B.provenance-theme-set-list-survives",
          sorted((prov or {}).get("selectedTokenSets", {})) == sorted(MIRROR_SETS))
    check("B.token-$description-survives",
          out["provenance"]["tokenDescription"] is not None,
          repr(out["provenance"]["tokenDescription"]))
    check("B.metadata-extra-key-dropped(expected)",
          not out["provenance"]["metadataExtraKeySurvived"],
          "$metadata.penpotPackages did not survive (as predicted)")
    mirrored_ok = all(
        res.flatten_set(tok_b.get(s, {})) == res.flatten_set(pkg_tokens[s])
        for s in MIRROR_SETS)
    check("B.mirrored-sets-flatten-equal-to-package", mirrored_ok,
          f"{len(MIRROR_SETS)} sets flatten-equal ({{path:{{type,value,description}}}}) "
          "after round trip — the proved contract is flatten-level equality, not "
          "raw-byte equality (canonical content is the settled export)")

    # ================= C. collision + order flip =============================
    tf_b = res.TokenFile(read_json(tokens_json_path(tree_b, cons_id)),
                         res.collect_applied_tokens(tree_b))
    rep_b = res.build_report(tf_b)
    winner1 = rep_b["tokens"].get("collide.winner")
    out["collision"] = {"orderBefore": tf_b.order,
                        "winnerBefore": winner1}
    check("C.later-set-in-tokenSetOrder-wins",
          winner1 and winner1["set"] == "E5 Collide - PkgB"
          and winner1["resolved"] == "#222222",
          f"collide.winner={winner1['resolved']} from {winner1['set']}"
          if winner1 else "token missing")
    meta_rpc1 = rpc_tokens_lib_meta(client, cons_id)
    out["collision"]["rpcTokenSetOrderBefore"] = (meta_rpc1 or {}).get("tokenSetOrder")

    # flip the order (edit + in-place re-import; explicit tooling)
    tree_c = os.path.join(work, "consC.penpot")
    if os.path.exists(tree_c):
        shutil.rmtree(tree_c)
    shutil.copytree(tree_b, tree_c)
    tok_c = read_json(tokens_json_path(tree_c, cons_id))
    o = tok_c["$metadata"]["tokenSetOrder"]
    ia, ib = o.index("E5 Collide - PkgA"), o.index("E5 Collide - PkgB")
    o[ia], o[ib] = o[ib], o[ia]
    write_json(tokens_json_path(tree_c, cons_id), tok_c)
    import_in_place(client, project_id, tree_c, cons_id, "e5-consumer")
    tree_d = export_tree(client, cons_id, os.path.join(work, "consD.penpot"))
    tf_d = res.TokenFile(read_json(tokens_json_path(tree_d, cons_id)),
                         res.collect_applied_tokens(tree_d))
    rep_d = res.build_report(tf_d)
    winner2 = rep_d["tokens"].get("collide.winner")
    out["collision"]["orderAfter"] = tf_d.order
    out["collision"]["winnerAfter"] = winner2
    meta_rpc2 = rpc_tokens_lib_meta(client, cons_id)
    out["collision"]["rpcTokenSetOrderAfter"] = (meta_rpc2 or {}).get("tokenSetOrder")
    check("C.order-flip-flips-resolved-value",
          winner2 and winner2["set"] == "E5 Collide - PkgA"
          and winner2["resolved"] == "#111111",
          f"collide.winner={winner2['resolved']} from {winner2['set']}"
          if winner2 else "token missing")
    check("C.flipped-order-survives-export",
          tf_d.order.index("E5 Collide - PkgB") < tf_d.order.index("E5 Collide - PkgA"),
          f"exported tokenSetOrder tail={tf_d.order[-3:]}")
    check("C.rpc-observes-flipped-order",
          out["collision"]["rpcTokenSetOrderAfter"] is not None
          and out["collision"]["rpcTokenSetOrderAfter"] == tf_d.order,
          "get-file data.tokensLib.$metadata.tokenSetOrder == exported order")
    bump_c = res.classify_bump(tf_b, tf_d)
    out["collision"]["bump"] = bump_c
    check("C.classifier-order-flip=MAJOR",
          bump_c["bump"] == "MAJOR" and any(
              r["rule"] == "order-flip-on-collision" for r in bump_c["reasons"]),
          f"bump={bump_c['bump']}")

    # ================= D. theme-dependent resolution (one file) =============
    tf_p_light = res.TokenFile(read_json(tokens_json_path(pkg_tree, pkg_id)),
                               res.collect_applied_tokens(pkg_tree))
    rep_light = res.build_report(tf_p_light)
    light_val = rep_light["tokens"]["layerBase.text"]
    meta_rpc_l = rpc_tokens_lib_meta(client, pkg_id)

    tree_pd = os.path.join(work, "pkg-dark.penpot")
    if os.path.exists(tree_pd):
        shutil.rmtree(tree_pd)
    shutil.copytree(pkg_tree, tree_pd)
    tok_pd = read_json(tokens_json_path(tree_pd, pkg_id))
    tok_pd["$metadata"]["activeThemes"] = [
        "Color mode/Dark" if t == "Color mode/Light" else t
        for t in tok_pd["$metadata"]["activeThemes"]]
    write_json(tokens_json_path(tree_pd, pkg_id), tok_pd)
    import_in_place(client, project_id, tree_pd, pkg_id,
                    "e5-pkg-tokens-starter-kit")
    tree_pd2 = export_tree(client, pkg_id, os.path.join(work, "pkg-dark2.penpot"))
    tf_p_dark = res.TokenFile(read_json(tokens_json_path(tree_pd2, pkg_id)),
                              res.collect_applied_tokens(tree_pd2))
    rep_dark = res.build_report(tf_p_dark)
    dark_val = rep_dark["tokens"]["layerBase.text"]
    meta_rpc_d = rpc_tokens_lib_meta(client, pkg_id)
    out["themeFlip"] = {
        "fileId": pkg_id,
        "light": light_val, "dark": dark_val,
        "rpcActiveThemesLight": (meta_rpc_l or {}).get("activeThemes"),
        "rpcActiveThemesDark": (meta_rpc_d or {}).get("activeThemes"),
    }
    check("D.same-path-resolves-differently-by-theme",
          light_val["resolved"] != dark_val["resolved"]
          and light_val["set"] != dark_val["set"],
          f"layerBase.text: Light={light_val['resolved']} ({light_val['set']}) "
          f"vs Dark={dark_val['resolved']} ({dark_val['set']})")
    check("D.rpc-observes-active-theme-change",
          meta_rpc_l and meta_rpc_d
          and "Color mode/Light" in (meta_rpc_l or {}).get("activeThemes", [])
          and "Color mode/Dark" in (meta_rpc_d or {}).get("activeThemes", []),
          f"RPC activeThemes {meta_rpc_l and meta_rpc_l.get('activeThemes')} "
          f"-> {meta_rpc_d and meta_rpc_d.get('activeThemes')}")
    # static-only cross-check: activating Dark on the LIGHT dump reproduces
    # the live-flipped resolution (theme flip is pure file data).
    # HONESTY NOTE: this check is NOT discriminating for one-theme-per-group in
    # THIS fixture — "Dark - Base" follows "Light - Base" in tokenSetOrder, so a
    # plain later-set-wins merge would land the same value even without applying
    # activate-theme's group-exclusivity rule. It confirms our resolver matches
    # the live bytes; it does NOT by itself prove the one-active-theme-per-group
    # semantics. That rule is jar-confirmed (tokens_lib.cljc), not discriminated
    # here. See token-resolver.md §4 and caveat "resolution-semantics drift".
    rep_static_dark = res.build_report(tf_p_light, ["Color mode/Dark"])
    check("D.static-activation-matches-live-flip(non-discriminating)",
          rep_static_dark["tokens"]["layerBase.text"]["resolved"]
          == dark_val["resolved"])
    bump_d = res.classify_bump(tf_p_light, tf_p_dark)
    out["themeFlip"]["bump"] = bump_d
    check("D.classifier-theme-only=MAJOR-BEHAVIORAL",
          bump_d["bump"] == "MAJOR-BEHAVIORAL" and any(
              r["rule"] == "theme-only-change" for r in bump_d["reasons"]),
          f"bump={bump_d['bump']}")

    # ================= E. resolver headless over the dump ===================
    out["resolver"] = {
        "tokenCount": rep_light["tokenCount"],
        "appliedTokenPaths": rep_light["appliedTokenPaths"],
        "appliedTokenRefs": rep_light["appliedTokenRefs"],
        "freeVariables": rep_light["freeVariables"],
        "danglingApplied": rep_light["danglingApplied"],
        "danglingBaseline": rep_light["danglingBaseline"],
        "sampleDeps": {p: rep_light["tokens"][p]
                       for p in ("layerBase.text", "modular.xl", "body")
                       if p in rep_light["tokens"]},
    }
    with open(os.path.join(work, "pkg-report.json"), "w") as fh:
        json.dump(rep_light, fh, indent=2)
    deps_listed = all(rep_light["tokens"][p]["refs"]
                      for p in rep_light["tokens"]
                      if "{" in str(rep_light["tokens"][p]["value"]))
    check("E.deps-listing-over-dump", deps_listed,
          f"{rep_light['tokenCount']} tokens; e.g. modular.xl -> "
          f"{rep_light['tokens']['modular.xl']['refs']}")
    check("E.pre-existing-dangling-baseline-pinned",
          len(rep_light["danglingApplied"]) > 0
          and not rep_light["danglingValueRefs"],
          f"{len(rep_light['danglingApplied'])} applied paths dangle "
          f"({sum(rep_light['danglingApplied'].values())} refs), "
          f"0 value-ref dangles: {sorted(rep_light['danglingApplied'])[:4]}…")

    # synthetic dropped token: remove layerBase.text from Light - Base
    tree_drop = os.path.join(work, "pkg-dropped.penpot")
    if os.path.exists(tree_drop):
        shutil.rmtree(tree_drop)
    shutil.copytree(pkg_tree, tree_drop)
    tok_drop = read_json(tokens_json_path(tree_drop, pkg_id))
    del tok_drop["Light - Base"]["layerBase"]["text"]
    write_json(tokens_json_path(tree_drop, pkg_id), tok_drop)
    tf_drop = res.TokenFile(read_json(tokens_json_path(tree_drop, pkg_id)),
                            res.collect_applied_tokens(tree_drop))
    bump_e = res.classify_bump(tf_p_light, tf_drop)
    out["resolver"]["droppedTokenBump"] = bump_e
    baseline_excluded = all(p not in bump_e["newDangling"]
                            for p in rep_light["danglingBaseline"])
    check("E.synthetic-dropped-token=MAJOR",
          bump_e["bump"] == "MAJOR"
          and any(r["rule"] == "dropped-token-you-depend-on"
                  and r["path"] == "layerBase.text" for r in bump_e["reasons"])
          and bump_e["newDangling"] == ["layerBase.text"] and baseline_excluded,
          f"bump={bump_e['bump']} newDangling={bump_e['newDangling']} "
          f"(baseline {len(bump_e['danglingBaseline'])} paths excluded)")

    # shadowing-add: new set appended, redefines layerBase.text
    tree_shadow = os.path.join(work, "pkg-shadow.penpot")
    if os.path.exists(tree_shadow):
        shutil.rmtree(tree_shadow)
    shutil.copytree(pkg_tree, tree_shadow)
    tok_sh = read_json(tokens_json_path(tree_shadow, pkg_id))
    tok_sh["E5 Brand Overrides"] = {
        "layerBase": {"text": {"$type": "color", "$value": "#ff0000"}}}
    tok_sh["$metadata"]["tokenSetOrder"].append("E5 Brand Overrides")
    tok_sh["$metadata"]["activeSets"].append("E5 Brand Overrides")
    write_json(tokens_json_path(tree_shadow, pkg_id), tok_sh)
    tf_sh = res.TokenFile(read_json(tokens_json_path(tree_shadow, pkg_id)),
                          res.collect_applied_tokens(tree_shadow))
    bump_f = res.classify_bump(tf_p_light, tf_sh)
    out["resolver"]["shadowingAddBump"] = bump_f
    check("E.shadowing-add=READS-MINOR-BEHAVES-MAJOR",
          bump_f["bump"] == "READS-MINOR-BEHAVES-MAJOR"
          and not bump_f["removedTokens"],
          f"bump={bump_f['bump']} (adds only: "
          f"{len(bump_f['addedTokens'])} added, 0 removed; layerBase.text "
          f"now resolves #ff0000)")

    # ===== G. offline classifier adversarial pairs + resolver hardening =====
    # Pure static (no live stack): synthetic before/after trees exercise the
    # multi-axis breakage the single-axis live pairs (C/D/E) do not. Each was a
    # confirmed classifier miss (PATCH/MINOR) before the resolved-view refound.
    import time as _time

    def _bump(before, after, ap_b=None, ap_a=None):
        return res.classify_bump(res.TokenFile(before, ap_b or {}),
                                 res.TokenFile(after, ap_a or {}))

    # (i) pure $value edit on an APPLIED token -> behavioral breakage, NOT PATCH
    g_base = {"S": {"color": {"primary": {"$type": "color", "$value": "#111111"}}},
              "$metadata": {"tokenSetOrder": ["S"], "activeSets": ["S"]}}
    g_after = copy.deepcopy(g_base)
    g_after["S"]["color"]["primary"]["$value"] = "#ff0000"
    g_val = _bump(g_base, g_after, {"color.primary": 1}, {"color.primary": 1})
    check("G.value-edit-on-applied-token-not-PATCH",
          g_val["bump"] == "MAJOR-BEHAVIORAL"
          and any(r["rule"] == "value-changed" for r in g_val["reasons"]),
          f"in-place #111111->#ff0000 on applied color.primary => {g_val['bump']}")

    # (ii) dropping the WINNING colliding definition -> MAJOR, NOT PATCH
    g_col = {"Old": {"c": {"w": {"$type": "color", "$value": "#000000"}}},
             "New": {"c": {"w": {"$type": "color", "$value": "#ffffff"}}},
             "$metadata": {"tokenSetOrder": ["Old", "New"],
                           "activeSets": ["Old", "New"]}}
    g_col_after = copy.deepcopy(g_col)
    del g_col_after["New"]["c"]["w"]
    g_wd = _bump(g_col, g_col_after)
    check("G.winning-definition-drop-not-PATCH",
          g_wd["bump"] == "MAJOR"
          and any(r["rule"] == "winning-definition-drop" for r in g_wd["reasons"]),
          f"drop winner New(#ffffff) => resolves #000000, {g_wd['bump']}")

    # (iii) theme flip + one HARMLESS added token -> still MAJOR-BEHAVIORAL, NOT MINOR
    g_thm = {"Light": {"fg": {"text": {"$type": "color", "$value": "#000000"}}},
             "Dark": {"fg": {"text": {"$type": "color", "$value": "#ffffff"}}},
             "$themes": [{"id": "tl", "name": "Light", "group": "mode",
                          "selectedTokenSets": {"Light": "enabled"}},
                         {"id": "td", "name": "Dark", "group": "mode",
                          "selectedTokenSets": {"Dark": "enabled"}}],
             "$metadata": {"tokenSetOrder": ["Light", "Dark"], "activeSets": [],
                           "activeThemes": ["mode/Light"]}}
    g_thm_after = copy.deepcopy(g_thm)
    g_thm_after["$metadata"]["activeThemes"] = ["mode/Dark"]
    g_thm_after["Extra"] = {"misc": {"pad": {"$type": "dimension", "$value": "4"}}}
    g_thm_after["$metadata"]["tokenSetOrder"] = ["Light", "Dark", "Extra"]
    g_tf = _bump(g_thm, g_thm_after)
    check("G.theme-flip-plus-add-not-MINOR",
          g_tf["bump"] == "MAJOR-BEHAVIORAL"
          and any(r["rule"] in ("theme-change", "theme-only-change")
                  for r in g_tf["reasons"]),
          f"theme flip + harmless add => {g_tf['bump']} "
          f"(rules {[r['rule'] for r in g_tf['reasons']]})")

    # (iv) baseline sanity: identical => PATCH; pure add, no resolved change => MINOR
    g_id = _bump(g_base, copy.deepcopy(g_base))
    g_padd_after = copy.deepcopy(g_base)
    g_padd_after["S"]["color"]["secondary"] = {"$type": "color", "$value": "#654321"}
    g_padd = _bump(g_base, g_padd_after)
    check("G.baseline-identical=PATCH-and-pure-add=MINOR",
          g_id["bump"] == "PATCH" and g_padd["bump"] == "MINOR"
          and any(r["rule"] == "token-added" for r in g_padd["reasons"]),
          f"identical={g_id['bump']} pure-add={g_padd['bump']}")

    # (v) resolver hardening: '99**999999' math must NOT hang (bounded < 2s), and
    #     composite (dict-valued) $values must not mint phantom free variables.
    t0 = _time.time()
    hostile = res.resolve_value({}, "99**999999", set())
    math_dt = _time.time() - t0
    comp_tokens = {
        "Typo": {"heading": {"$type": "typography",
                             "$value": {"fontFamily": "{font.base}", "fontSize": "16"}}},
        "Plain": {"misc": {"tag": {"$type": "typography",
                                   "$value": {"fontFamily": "Inter", "fontWeight": "bold"}}}},
        "Base": {"font": {"base": {"$type": "fontFamily", "$value": "Inter"}}},
        "$metadata": {"tokenSetOrder": ["Base", "Typo", "Plain"],
                      "activeSets": ["Base", "Typo", "Plain"]}}
    comp_rep = res.build_report(res.TokenFile(comp_tokens))
    check("G.hostile-math-bounded-and-composite-no-phantom-refs",
          math_dt < 2.0 and hostile == "99**999999"
          and comp_rep["freeVariables"] == []
          and comp_rep["tokens"]["heading"]["refs"] == ["font.base"]
          and comp_rep["tokens"]["heading"]["resolved"]["fontFamily"] == "Inter",
          f"'99**999999' left unresolved in {math_dt:.3f}s (no eval hang); "
          f"composite freeVars={comp_rep['freeVariables']} "
          f"heading.refs={comp_rep['tokens']['heading']['refs']}")

    # ================= F. drift = conflict copy, overwrite neither ==========
    drift_clean = res.check_drift(tf_d, tf_p_light)
    tree_drift = os.path.join(work, "cons-drifted.penpot")
    if os.path.exists(tree_drift):
        shutil.rmtree(tree_drift)
    shutil.copytree(tree_d, tree_drift)
    tok_dr = read_json(tokens_json_path(tree_drift, cons_id))
    tok_dr["Light - Base"]["layerBase"]["text"]["$value"] = "#bada55"
    write_json(tokens_json_path(tree_drift, cons_id), tok_dr)
    tf_drift = res.TokenFile(read_json(tokens_json_path(tree_drift, cons_id)),
                             res.collect_applied_tokens(tree_drift))
    drift_dirty = res.check_drift(tf_drift, tf_p_light)
    out["drift"] = {"clean": drift_clean, "drifted": drift_dirty}
    all_clean = all(s["state"] == "clean" for s in drift_clean["sets"].values())
    dirty = drift_dirty["sets"].get("Light - Base", {})
    check("F.drift-detector-clean-mirror", all_clean and drift_clean["sets"],
          f"{len(drift_clean['sets'])} package-owned sets clean "
          f"(declared by provenance theme {PROVENANCE_ID})")
    check("F.drift-detector-flags-mutated-set",
          dirty.get("state") == "DRIFTED"
          and "conflict-copy" in dirty.get("action", "")
          and dirty.get("changedPaths") == ["layerBase.text"],
          f"Light - Base -> {dirty.get('state')}; action={dirty.get('action')}")

    # ---------------------------------------------------------------- wrap
    out["checks"] = CHECKS
    fails = [c for c in CHECKS if not c["ok"]]
    with open(os.path.join(work, "findings.json"), "w") as fh:
        json.dump(out, fh, indent=2)
    print(f"\n== e5_probe: {len(CHECKS) - len(fails)}/{len(CHECKS)} checks passed ==")
    return 1 if fails else 0


def main():
    if len(sys.argv) < 3 or sys.argv[1] != "run":
        print("usage: e5_probe.py run <work_dir>", file=sys.stderr)
        return 2
    return run(sys.argv[2])


if __name__ == "__main__":
    sys.exit(main())
