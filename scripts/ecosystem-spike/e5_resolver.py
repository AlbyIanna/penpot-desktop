#!/usr/bin/env python3
"""E5 spike: STATIC cross-package token resolver (PLAN3 E5, open question #2).

Offline files in, report out — NEVER injected (ecosystem invariant 3). Input is
a normalized binfile-v3 tree (manifest.json + files/<fid>/tokens.json + shape
jsons) or a bare tokens.json. The resolution semantics mirror Penpot 2.16.2's
app/common/types/tokens_lib.cljc, read from runtime/backend/penpot.jar:

  * set order        = $metadata.tokenSetOrder ∪ remaining set keys (in order)
                       (parse-multi-set-dtcg-json `ordered-set-names`)
  * hidden theme     = always active, sets = $metadata.activeSets
                       (make-tokens-lib / parse `update-theme hidden-theme-id`)
  * active themes    = hidden + $metadata.activeThemes ("Group/Name" paths);
                       activate-theme keeps ONE active theme per group
  * active set names = union of :sets over active themes
                       (get-active-themes-set-names)
  * resolution       = get-tokens-in-active-sets: walk ALL set names in order,
                       keep the active ones, `merge` their (name -> token)
                       maps — so the LATER set in tokenSetOrder wins a bare-path
                       collision. Order is part of the contract.

Aliases `{path}` are dereferenced against the merged map; token math like
`{modular.xl}*{density}` is substituted and arithmetic-evaluated when safe.

Subcommands (all print JSON to stdout):

  report <tree|tokens.json> [--activate Group/Name]...
      Per-token free-variable deps ({refs} in $value minus own exports),
      resolved values under the file's own activation (optionally overriding
      one theme per group, exactly like activate-theme), dangling value-refs,
      appliedTokens census + dangling applied paths (the noise baseline).

  bump <before-tree> <after-tree>
      Classify the before/after pair off a RESOLVED-VIEW DIFF (the flattened
      {dotted-path: resolved-value} under each tree's own active sets+themes):
      any existing path whose resolved value moved is behavioral breakage, no
      matter how it was authored. Specific causes are named when identifiable:
        order-flip-on-collision            -> MAJOR (both sets still define the
                                              path; their relative order flipped)
        winning-definition-drop            -> MAJOR (the previously-winning set
                                              dropped its definition; a lower set
                                              now wins the same path)
        dropped-token-you-depend-on        -> MAJOR (path no longer resolvable;
                                              only NEW dangling paths count —
                                              pre-existing ones are baseline noise)
        new-dangling-ref                   -> MAJOR (a $value now points at
                                              nothing, beyond the baseline)
        value-changed                      -> MAJOR-BEHAVIORAL (in-place $value
                                              edit; still resolves, renders anew)
        theme-only / theme-change          -> MAJOR-BEHAVIORAL (activation flip)
        shadowing-add                      -> READS-MINOR-BEHAVES-MAJOR
                                              (token analogue of E1 caveat 2)
        additions only, no resolved change -> MINOR
        nothing contract-visible           -> PATCH
      Rank (ties resolved in favour of the structural bump):
        MAJOR > MAJOR-BEHAVIORAL > READS-MINOR-BEHAVES-MAJOR > MINOR > PATCH.

  drift <consumer-tree|tokens.json> <package-tokens.json>
      Compare the consumer's package-owned mirrored sets (declared by the
      provenance theme, group "penpot:package") against the package source.
      A drifted mirrored set = the exact conflict rule: export a
      .conflict-<ts> copy, overwrite NEITHER side. This tool only detects;
      it never rewrites.

Stdlib only. House style: scripts/ecosystem-spike/ E1 oracle lineage.
"""
import json
import os
import re
import sys

REF_RE = re.compile(r"\{([^{}]+)\}")
MATH_SAFE_RE = re.compile(r"^[0-9.\s+*/()-]+$")
MATH_MAX_LEN = 100       # longest arithmetic expression we will evaluate
MATH_MAX_DIGITS = 6      # longest numeric literal (a 7-digit operand is rejected)
PROVENANCE_THEME_GROUP = "penpot:package"


# ------------------------------------------------------------------ loading

def find_tokens_json(root):
    """In a binfile-v3 tree, return (relpath, decoded tokens.json) for the
    first file that has one."""
    for dirpath, _dirs, files in sorted(os.walk(root)):
        for fn in sorted(files):
            if fn == "tokens.json":
                p = os.path.join(dirpath, fn)
                with open(p) as fh:
                    return os.path.relpath(p, root), json.load(fh)
    return None, None


def collect_applied_tokens(root):
    """{token-path: ref-count} over every shape json's appliedTokens map."""
    applied = {}
    for dirpath, _dirs, files in os.walk(root):
        if f"{os.sep}pages" not in dirpath:
            continue
        for fn in files:
            if not fn.endswith(".json"):
                continue
            try:
                with open(os.path.join(dirpath, fn)) as fh:
                    obj = json.load(fh)
            except Exception:
                continue
            if isinstance(obj, dict) and isinstance(obj.get("appliedTokens"), dict):
                for _attr, path in obj["appliedTokens"].items():
                    applied[path] = applied.get(path, 0) + 1
    return applied


def load_input(path):
    """Accept a tree dir or a bare tokens.json. Returns (tokens-json dict,
    appliedTokens census)."""
    if os.path.isdir(path):
        _rel, data = find_tokens_json(path)
        if data is None:
            raise SystemExit(f"no tokens.json under {path}")
        return data, collect_applied_tokens(path)
    with open(path) as fh:
        return json.load(fh), {}


# -------------------------------------------------------------- token model

def flatten_set(node, prefix=""):
    """DTCG set subtree -> ordered {dotted-path: {type, value, description}}.
    Mirrors flatten-nested-tokens-json: a map without $type is a group; keys
    are joined with '.'; non-map / typeless leaves are discarded."""
    out = {}
    if not isinstance(node, dict):
        return out
    for k, v in node.items():
        if k.startswith("$"):
            continue  # e.g. a set-root $description: upstream discards it
        path = f"{prefix}.{k}" if prefix else k
        if isinstance(v, dict) and "$type" not in v:
            out.update(flatten_set(v, path))
        elif isinstance(v, dict) and "$type" in v:
            out[path] = {"type": v.get("$type"), "value": v.get("$value"),
                         "description": v.get("$description")}
    return out


class TokenFile:
    """Parsed tokens.json with tokens_lib.cljc activation semantics."""

    def __init__(self, data, applied=None):
        self.raw = data
        self.applied = applied or {}
        meta = data.get("$metadata") or {}
        set_keys = [k for k in data if not k.startswith("$")]
        order = [n for n in (meta.get("tokenSetOrder") or []) if n in set_keys]
        order += [n for n in set_keys if n not in order]
        self.order = order
        self.sets = {n: flatten_set(data[n]) for n in order}
        self.active_sets_meta = [n for n in (meta.get("activeSets") or [])
                                 if n in self.sets]
        self.themes = []
        for t in data.get("$themes") or []:
            self.themes.append({
                "id": t.get("id"),
                "group": t.get("group") or "",
                "name": t.get("name"),
                "description": t.get("description"),
                "path": f'{t.get("group") or ""}/{t.get("name")}',
                "sets": [s for s, st in (t.get("selectedTokenSets") or {}).items()
                         if st == "enabled" and s in self.sets],
            })
        self.active_theme_paths = list(meta.get("activeThemes") or [])

    def active_themes(self, overrides=None):
        """Active theme list applying activate-theme's one-per-group rule for
        each override path."""
        active = list(self.active_theme_paths)
        by_path = {t["path"]: t for t in self.themes}
        for ov in overrides or []:
            if ov not in by_path:
                raise SystemExit(f"unknown theme path {ov!r}")
            group = by_path[ov]["group"]
            active = [p for p in active
                      if p not in by_path or by_path[p]["group"] != group]
            active.append(ov)
        return active

    def active_set_names(self, overrides=None):
        """get-active-themes-set-names: hidden theme (activeSets) + every
        active theme's enabled sets, kept in tokenSetOrder order."""
        names = set(self.active_sets_meta)
        active = set(self.active_themes(overrides))
        for t in self.themes:
            if t["path"] in active:
                names.update(t["sets"])
        return [n for n in self.order if n in names]

    def merged(self, overrides=None):
        """get-tokens-in-active-sets: ordered merge, later set wins.
        Returns {path: {set, type, value, description}}."""
        merged = {}
        for name in self.active_set_names(overrides):
            for path, tok in self.sets[name].items():
                merged[path] = dict(tok, set=name)
        return merged

    def all_defined(self):
        out = set()
        for toks in self.sets.values():
            out.update(toks)
        return out


def _string_leaves(value):
    """Yield every STRING leaf of a $value (scalars, or the strings nested in a
    composite dict/list $value). Only string leaves can carry a `{ref}`; scanning
    them individually avoids the phantom refs a json.dumps of a dict would mint
    (the object's own braces / quoted keys are not aliases)."""
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for v in value.values():
            yield from _string_leaves(v)
    elif isinstance(value, list):
        for v in value:
            yield from _string_leaves(v)


def refs_of(value):
    """All {refs} inside a token $value. Composite (dict/list) $values are
    recursed and only their STRING leaves are scanned — minimal composite
    support (leaf-string refs), a chapter-4 refinement."""
    refs = []
    for leaf in _string_leaves(value):
        refs.extend(REF_RE.findall(leaf))
    return refs


def _safe_arith(expr):
    """Evaluate token math like "8*1.25", or return None if the expression is
    unsafe or pathological. Rejects '**' (the power operator — "99**999999"
    reaches eval and does unbounded work), overlong expressions, and oversized
    operands BEFORE eval ever runs, so a hostile $value degrades to unresolved
    (left symbolic) instead of hanging the resolver."""
    if len(expr) > MATH_MAX_LEN:
        return None
    if "**" in expr:                       # power operator -> resource exhaustion
        return None
    if not MATH_SAFE_RE.match(expr) or not re.search(r"[+*/-]", expr):
        return None
    if re.search(r"\d{%d,}" % (MATH_MAX_DIGITS + 1), expr):   # cap operand magnitude
        return None
    # SAFETY: gated by MATH_SAFE_RE — ONLY digits, '.', whitespace and
    # + - * / ( ). No letters/underscores/brackets means no names, no attribute
    # access, no calls; builtins are emptied as belt-and-braces. '**' is already
    # rejected above, and operand length/magnitude are bounded.
    try:
        return eval(expr, {"__builtins__": {}}, {})
    except Exception:
        return None


def resolve_value(merged, value, dangling, _seen=None):
    """Dereference {refs} against the merged map; evaluate safe arithmetic.
    Composite (dict/list) $values are recursed into so their inner string-leaf
    refs resolve too. Unresolvable refs are recorded in `dangling`, left symbolic."""
    if isinstance(value, dict):
        return {k: resolve_value(merged, v, dangling, _seen) for k, v in value.items()}
    if isinstance(value, list):
        return [resolve_value(merged, v, dangling, _seen) for v in value]
    if not isinstance(value, str):
        return value
    seen = _seen or frozenset()

    def deref(m):
        path = m.group(1)
        if path in seen:
            return m.group(0)  # cycle: leave symbolic
        tok = merged.get(path)
        if tok is None:
            dangling.add(path)
            return m.group(0)
        r = resolve_value(merged, tok["value"], dangling, seen | {path})
        return r if isinstance(r, str) else json.dumps(r)

    out = REF_RE.sub(deref, value)
    if "{" not in out and re.search(r"[+*/-]", out):
        val = _safe_arith(out)
        if val is not None:
            out = f"{val:.6f}".rstrip("0").rstrip(".")
    return out


# ------------------------------------------------------------------ report

def build_report(tf, overrides=None):
    merged = tf.merged(overrides)
    defined = tf.all_defined()
    tokens = {}
    dangling_refs = {}
    for path in sorted(merged):
        tok = merged[path]
        refs = refs_of(tok["value"])
        d = set()
        resolved = resolve_value(merged, tok["value"], d)
        for ref in d:
            dangling_refs.setdefault(ref, []).append(path)
        tokens[path] = {
            "set": tok["set"], "type": tok["type"], "value": tok["value"],
            "refs": sorted(set(refs)),
            "freeRefs": sorted(set(refs) - defined),
            "resolved": resolved,
        }
    dangling_applied = {p: c for p, c in sorted(tf.applied.items())
                        if p not in merged}
    free_vars = sorted({r for t in tokens.values() for r in t["freeRefs"]})
    return {
        "tokenSetOrder": tf.order,
        "activeThemes": tf.active_themes(overrides),
        "activeSets": tf.active_set_names(overrides),
        "themes": [{k: t[k] for k in ("path", "id", "description")}
                   for t in tf.themes],
        "tokenCount": len(tokens),
        "tokens": tokens,
        "freeVariables": free_vars,
        "danglingValueRefs": {k: sorted(v) for k, v in sorted(dangling_refs.items())},
        "appliedTokenPaths": len(tf.applied),
        "appliedTokenRefs": sum(tf.applied.values()),
        "danglingApplied": dangling_applied,
        "danglingBaseline": sorted(set(dangling_refs) | set(dangling_applied)),
    }


# ------------------------------------------------------------------- bump

SEVERITY = {"PATCH": 0, "MINOR": 1, "READS-MINOR-BEHAVES-MAJOR": 2,
            "MAJOR-BEHAVIORAL": 3, "MAJOR": 3}


def depended_on(tf, path):
    """Is `path` referenced by any appliedTokens entry or any token $value?"""
    if path in tf.applied:
        return True
    for toks in tf.sets.values():
        for p, tok in toks.items():
            if p != path and path in refs_of(tok["value"]):
                return True
    return False


def classify_bump(before_tf, after_tf):
    """Classify a before/after tokens pair off a RESOLVED-VIEW DIFF: the
    flattened `{dotted-path: resolved-value}` view under each tree's own active
    sets+themes. Any existing path whose resolved value changed is behavioral
    breakage (MAJOR-BEHAVIORAL at minimum); the specific named cause is kept
    when identifiable. This is the load-bearing property — a change is never
    PATCH/MINOR if it moved a resolved value, no matter how it was authored
    (in-place $value edit, dropped winning definition, theme flip, order flip)."""
    b, a = build_report(before_tf), build_report(after_tf)
    bt, at = b["tokens"], a["tokens"]
    reasons = []

    sets_equal = before_tf.sets == after_tf.sets
    order_equal = before_tf.order == after_tf.order
    themes_equal = ([{k: t[k] for k in ("path", "sets")} for t in before_tf.themes]
                    == [{k: t[k] for k in ("path", "sets")} for t in after_tf.themes]
                    and before_tf.active_theme_paths == after_tf.active_theme_paths
                    and before_tf.active_sets_meta == after_tf.active_sets_meta)

    removed_tokens = []
    for name, toks in before_tf.sets.items():
        after_toks = after_tf.sets.get(name, {})
        removed_tokens += [(name, p) for p in toks if p not in after_toks]
    removed_sets = [n for n in before_tf.sets if n not in after_tf.sets]
    added_tokens = []
    for name, toks in after_tf.sets.items():
        before_toks = before_tf.sets.get(name, {})
        added_tokens += [(name, p) for p in toks if p not in before_toks]

    # ---- the resolved-view diff (foundation) ----
    common = set(bt) & set(at)
    resolution_changed = {p: (bt[p]["resolved"], at[p]["resolved"])
                          for p in common if bt[p]["resolved"] != at[p]["resolved"]}
    resolved_removed = sorted(set(bt) - set(at))   # path no longer resolvable
    b_active, a_active = set(b["activeSets"]), set(a["activeSets"])
    attributed = set()   # changed paths already explained by a specific rule

    # 1. order-flip-on-collision: both sets still define p AND are active in
    #    both trees, and the winner flipped because their RELATIVE ORDER did
    #    -> MAJOR. (A winner change from activation is theme territory, rule 5.)
    for p in sorted(resolution_changed):
        bw, aw = bt[p]["set"], at[p]["set"]
        if bw == aw:
            continue
        if not {bw, aw} <= (b_active & a_active):
            continue
        if p not in before_tf.sets.get(aw, {}) or p not in after_tf.sets.get(bw, {}):
            continue
        bo, ao = before_tf.order, after_tf.order
        if bw in bo and aw in bo and bw in ao and aw in ao and \
                (bo.index(bw) > bo.index(aw)) != (ao.index(bw) > ao.index(aw)):
            reasons.append({"rule": "order-flip-on-collision", "bump": "MAJOR",
                            "path": p, "winnerBefore": bw, "winnerAfter": aw})
            attributed.add(p)

    # 2. winning-definition-drop: p still resolves, but its resolved value moved
    #    because the previously-WINNING set dropped its definition and a lower
    #    set now wins -> MAJOR. (Without this, deleting the winning colliding
    #    definition silently flipped resolution and classified PATCH.)
    for p in sorted(resolution_changed):
        if p in attributed:
            continue
        bw, aw = bt[p]["set"], at[p]["set"]
        if bw != aw and p in before_tf.sets.get(bw, {}) \
                and p not in after_tf.sets.get(bw, {}):
            reasons.append({"rule": "winning-definition-drop", "bump": "MAJOR",
                            "path": p, "winnerBefore": bw, "winnerAfter": aw,
                            "before": resolution_changed[p][0],
                            "after": resolution_changed[p][1]})
            attributed.add(p)

    # 3. dropped-token-you-depend-on -> MAJOR; only NEW dangling is breakage
    baseline = set(b["danglingBaseline"])
    new_dangling = sorted(set(a["danglingBaseline"]) - baseline)
    dropped_paths = set()
    for p in resolved_removed:
        if depended_on(before_tf, p) or depended_on(after_tf, p):
            reasons.append({"rule": "dropped-token-you-depend-on",
                            "bump": "MAJOR", "path": p,
                            "newDangling": p in new_dangling})
            dropped_paths.add(p)

    # 4. shadowing-add: pure additions, but a pre-existing path now resolves
    #    differently because an added set/token wins -> READS-MINOR-BEHAVES-MAJOR
    if not removed_tokens and not removed_sets and added_tokens:
        shadowed = {p: v for p, v in resolution_changed.items()
                    if p not in attributed and (
                        (at[p]["set"], p) in added_tokens
                        or at[p]["set"] not in before_tf.sets)}
        if shadowed:
            reasons.append({"rule": "shadowing-add",
                            "bump": "READS-MINOR-BEHAVES-MAJOR",
                            "shadowed": {p: {"before": v[0], "after": v[1],
                                             "winner": at[p]["set"]}
                                         for p, v in sorted(shadowed.items())}})
            attributed.update(shadowed)

    # 5. everything still unexplained that moved a resolved value is behavioral
    #    breakage. Split by identifiable cause: an in-place $value edit (same
    #    winning set, different definition) vs a theme/activation-driven flip.
    remaining = [p for p in sorted(resolution_changed) if p not in attributed]
    value_edits, theme_driven = {}, {}
    for p in remaining:
        bw, aw = bt[p]["set"], at[p]["set"]
        bdef = before_tf.sets.get(bw, {}).get(p)
        adef = after_tf.sets.get(aw, {}).get(p)
        if bw == aw and bdef != adef:
            value_edits[p] = resolution_changed[p]
        else:
            theme_driven[p] = resolution_changed[p]
    if value_edits:
        reasons.append({"rule": "value-changed", "bump": "MAJOR-BEHAVIORAL",
                        "resolutionChanged": {p: {"before": v[0], "after": v[1]}
                                              for p, v in sorted(value_edits.items())}})
    # theme/activation-driven flip. The empty-resolution fallback (activeSets
    # changed but moved no value) only counts as a pure theme change when sets
    # and order are unchanged — otherwise the activeSets delta is a side effect
    # of an add/remove (e.g. shadowing-add), not a theme flip.
    emit_theme = bool(theme_driven) or (
        not themes_equal and sets_equal and order_equal
        and b["activeSets"] != a["activeSets"])
    if emit_theme:
        reasons.append({
            "rule": "theme-only-change" if (sets_equal and order_equal)
            else "theme-change",
            "bump": "MAJOR-BEHAVIORAL",
            "resolutionChanged": {p: {"before": v[0], "after": v[1]}
                                  for p, v in sorted(theme_driven.items())}})

    # 6. any NEW dangling ref beyond the baseline that a dropped-token reason
    #    did not already name (e.g. a $value edited to point at nothing) -> MAJOR
    extra_dangling = [d for d in new_dangling if d not in dropped_paths]
    if extra_dangling:
        reasons.append({"rule": "new-dangling-ref", "bump": "MAJOR",
                        "paths": extra_dangling})

    # 7. additions only, nothing above them -> MINOR
    if added_tokens and not any(r["bump"] != "MINOR" for r in reasons):
        reasons.append({"rule": "token-added", "bump": "MINOR",
                        "count": len(added_tokens)})

    overall = "PATCH"
    for r in reasons:
        if SEVERITY[r["bump"]] > SEVERITY[overall]:
            overall = r["bump"]
    return {
        "bump": overall,
        "reasons": reasons,
        "danglingBaseline": sorted(baseline),
        "newDangling": new_dangling,
        "removedTokens": [f"{s}:{p}" for s, p in removed_tokens],
        "addedTokens": [f"{s}:{p}" for s, p in added_tokens],
    }


# ------------------------------------------------------------------- drift

def check_drift(consumer_tf, package_tf):
    """Mirrored-set drift detection. The provenance theme (group
    'penpot:package') declares which consumer sets are package-owned; each is
    compared (flattened tokens: type+value+description — all three survive the
    round trip per the survival matrix, so an edit to a mirrored token's
    $description is real drift) against the package source. Drift -> conflict
    copy, overwrite neither side."""
    result = {"provenanceThemes": [], "sets": {}}
    strip = lambda toks: {p: {"type": t["type"], "value": t["value"],
                              "description": t["description"]}
                          for p, t in toks.items()}
    for theme in consumer_tf.themes:
        if theme["group"] != PROVENANCE_THEME_GROUP:
            continue
        result["provenanceThemes"].append(
            {"path": theme["path"], "externalId": theme["id"],
             "description": theme["description"], "sets": theme["sets"]})
        for name in theme["sets"]:
            mine = strip(consumer_tf.sets.get(name, {}))
            theirs = strip(package_tf.sets.get(name, {}))
            if name not in package_tf.sets:
                state = "gone-upstream"
            elif mine == theirs:
                state = "clean"
            else:
                state = "DRIFTED"
            result["sets"][name] = {
                "state": state,
                "action": ("conflict-copy (.conflict-<ts>), overwrite neither"
                           if state == "DRIFTED" else "none"),
                "changedPaths": sorted(
                    p for p in set(mine) | set(theirs)
                    if mine.get(p) != theirs.get(p)),
            }
    return result


# -------------------------------------------------------------------- main

def main():
    args = sys.argv[1:]
    if not args:
        print(__doc__)
        return 2
    cmd, rest = args[0], args[1:]
    if cmd == "report":
        overrides = []
        paths = []
        i = 0
        while i < len(rest):
            if rest[i] == "--activate":
                overrides.append(rest[i + 1])
                i += 2
            else:
                paths.append(rest[i])
                i += 1
        data, applied = load_input(paths[0])
        print(json.dumps(build_report(TokenFile(data, applied),
                                      overrides or None), indent=2))
        return 0
    if cmd == "bump":
        b_data, b_applied = load_input(rest[0])
        a_data, a_applied = load_input(rest[1])
        print(json.dumps(classify_bump(TokenFile(b_data, b_applied),
                                       TokenFile(a_data, a_applied)), indent=2))
        return 0
    if cmd == "drift":
        c_data, c_applied = load_input(rest[0])
        p_data, _ = load_input(rest[1])
        print(json.dumps(check_drift(TokenFile(c_data, c_applied),
                                     TokenFile(p_data)), indent=2))
        return 0
    print(f"unknown subcommand {cmd!r}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
