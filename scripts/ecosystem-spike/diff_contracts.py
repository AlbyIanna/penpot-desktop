#!/usr/bin/env python3
"""Ecosystem spike step 4: classify a contract change as patch / minor / major.

Contract per set = {variantNames, exposedProperties, tokensUsed}. Rules (from
docs/ecosystem-design.md):
  - patch : contract identical (only implementation changed).
  - minor : contract only GREW (elements added, none removed/renamed).
  - major : contract LOST or RENAMED any element (removed, or value changed).

A set present in one side but not the other is a whole-set add (minor) or
remove (major). The file-level bump is the max severity across all sets.

Usage: python3 diff_contracts.py <before-contracts.json> <after-contracts.json>
"""
import json, sys

FIELDS = ["variantNames", "exposedProperties", "tokensUsed"]
RANK = {"patch": 0, "minor": 1, "major": 2}


def by_set(doc):
    return {c["set"]: c for c in doc["contracts"]}


def classify_field(before, after):
    b, a = set(before), set(after)
    removed = b - a
    added = a - b
    if removed:
        return "major", {"removed": sorted(removed), "added": sorted(added)}
    if added:
        return "minor", {"added": sorted(added)}
    return "patch", {}


def classify(before_doc, after_doc):
    B, A = by_set(before_doc), by_set(after_doc)
    per_set = {}
    overall = "patch"
    for key in sorted(set(B) | set(A)):
        if key not in A:
            per_set[key] = {"bump": "major", "reason": "set removed"}
            overall = "major"
            continue
        if key not in B:
            per_set[key] = {"bump": "minor", "reason": "set added"}
            overall = max(overall, "minor", key=lambda x: RANK[x])
            continue
        field_bumps = {}
        setbump = "patch"
        for f in FIELDS:
            bump, detail = classify_field(B[key][f], A[key][f])
            if bump != "patch":
                field_bumps[f] = {"bump": bump, **detail}
            setbump = max(setbump, bump, key=lambda x: RANK[x])
        if field_bumps:
            per_set[key] = {"bump": setbump, "fields": field_bumps}
        overall = max(overall, setbump, key=lambda x: RANK[x])
    return overall, per_set


def main():
    before = json.load(open(sys.argv[1]))
    after = json.load(open(sys.argv[2]))
    overall, per_set = classify(before, after)
    print(f"OVERALL BUMP: {overall.upper()}")
    for k, v in per_set.items():
        print(f"  set {k!r}: {json.dumps(v)}")
    if not per_set:
        print("  (no contract changes — pure implementation delta)")


if __name__ == "__main__":
    main()
