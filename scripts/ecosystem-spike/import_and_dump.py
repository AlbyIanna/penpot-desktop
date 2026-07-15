#!/usr/bin/env python3
"""Ecosystem spike step 1: import the two grounding templates as NEW files into
the m0 docker stack, export the normalized .penpot tree, and dump it to disk for
inspection.

Reuses scripts/roundtrip.py's Client + normalize helpers (same normalizer the
daemon uses semantically). Writes each template's normalized tree under
  <OUT>/<template-id>/tree/
and prints the file-id + a quick census (pages, components, tokens.json?).

Usage: python3 import_and_dump.py <template-id> [<template-id> ...]
Env: OUT (default scratchpad), PENPOT_BACKEND/PENPOT_FRONTEND as roundtrip.py.
"""
import os, sys, json
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import roundtrip as rt  # noqa: E402

OUT = os.environ.get("ECO_OUT", "/private/tmp/claude-501/-Users-albertoiannaccone-Workspace-penpot-desktop/cc12bf5c-379e-45b4-8c51-251b7d62edfd/scratchpad/eco")
TEMPLATES_DIR = os.path.join(rt.REPO, "runtime", "backend", "builtin-templates")


def census(tree_dir):
    files = rt.tree_files(tree_dir)
    rels = sorted(files)
    manifest = json.loads(files["manifest.json"])
    fid = manifest["files"][0]["id"]
    pages = [r for r in rels if f"files/{fid}/pages/" in r and r.count("/") == 3]
    comps = [r for r in rels if f"files/{fid}/components/" in r]
    colors = [r for r in rels if f"files/{fid}/colors/" in r]
    typos = [r for r in rels if f"files/{fid}/typographies/" in r]
    has_tokens = any(r.endswith("tokens.json") or "/tokens/" in r for r in rels)
    token_files = [r for r in rels if "token" in r.lower()]
    return {
        "fileId": fid, "totalFiles": len(rels), "pages": len(pages),
        "components": len(comps), "colors": len(colors),
        "typographies": len(typos), "hasTokens": has_tokens,
        "tokenFiles": token_files[:10],
        "fileDocKeys": None,
    }


def main():
    client = rt.Client()
    profile = client.login()
    project_id = profile["defaultProjectId"]
    print(f"[eco] logged in {profile['email']} project {project_id}")
    for tid in sys.argv[1:]:
        path = os.path.join(TEMPLATES_DIR, tid)
        zip_bytes = open(path, "rb").read()
        print(f"[eco] importing {tid} ({len(zip_bytes)} bytes) as new...")
        new_id = rt.import_binfile(client, project_id, zip_bytes, file_id=None, name=tid)
        print(f"[eco]   imported file id {new_id}; exporting...")
        zip_exp = rt.export_binfile(client, new_id)
        out_tree = os.path.join(OUT, tid, "tree")
        rt.unzip_to(zip_exp, out_tree)
        rt.normalize_tree(out_tree)
        c = census(out_tree)
        c["importedFileId"] = new_id
        os.makedirs(os.path.join(OUT, tid), exist_ok=True)
        json.dump(c, open(os.path.join(OUT, tid, "census.json"), "w"), indent=2)
        print(f"[eco]   census: {json.dumps(c)}")
        # keep the file for later stability tests; record its id
        with open(os.path.join(OUT, tid, "file_id.txt"), "w") as fh:
            fh.write(new_id)


if __name__ == "__main__":
    main()
