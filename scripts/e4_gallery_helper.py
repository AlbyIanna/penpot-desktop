#!/usr/bin/env python3
"""E4 package-gallery / update-channel / drift gate helper (scripts/e4-gallery.sh).

Subcommands, each printing human PASS/FAIL lines and exiting 0/1. Reuses the E2
lockfile shape (`lock.json`) and the vault-index package surface
(`/__api/packages/search`, `/__packages`) plus the E4b update/drift channel
(`/__api/packages/updates`, `/__api/packages/preserve-drift`) — nothing new is
plumbed; every route is already served by the running stack.

  wait_color_ondisk <base> <vault> <file_id> <color_name> <wait_s>
      Wait until the daemon-synced design tree for <file_id> (located via
      /__api/vault/boards) carries an exported color named <color_name> under
      files/<fid>/colors/. Used to confirm the E3-authored "Brand Teal" color
      landed on disk BEFORE the tree is copied into a package source.

  seed_synthetic <vault> <n> <nonce>
      Torture-scale seeding: load lock.json and append <n> synthetic package
      entries (ids e4syn-NNNN, each name carrying a UNIQUE token <nonce>NNNN so
      an FTS query for that token returns exactly one id). No source tree is
      copied — the indexer degrades a source-less package to a metadata-only
      searchable row, which is all the search-perf leg needs. Prints, on the
      last line, JSON {seeded, probes:[{q, id}, ...]} — the (query, expected-id)
      pairs the gate asserts on.

  unseed_synthetic <vault>
      Remove every e4syn-* entry from lock.json (restore it to the real
      packages only). Prints the count removed.

  search_wait <base> <min_count> <wait_s>
      Poll /__api/packages/search?q= (empty query = full listing) until count
      >= min_count (the indexer picked up the seeded packages). Exit 0/1.

  search_assert <base> <query> <expect_id> <max_ms>
      GET /__api/packages/search?q=<query>; assert the FIRST hit's id is
      <expect_id> AND the server-measured tookMs is < <max_ms>. The <100ms
      exit-criterion assertion targets tookMs.

  source_add_color <vault> <pkg_id> <new_name>
      MINOR edit: clone an existing color json in the package's .penpot source
      tree to a fresh uuid file with a new id + name (<new_name>) — the exported
      color surface grows by one. Prints the new relpath.

  source_remove_color <vault> <pkg_id> <name_substr>
      MAJOR edit: delete the color json whose name contains <name_substr> from
      the package source — an exported element is removed. Prints the removed
      relpath.

  updates_wait <base> <pkg_id> <expect_bump> <wait_s>
      Poll /__api/packages/updates until the row for <pkg_id> shows
      updateAvailable=true with bump==<expect_bump>. Asserts isMajor matches
      (bump=="major"), pinnedContractHash != liveContractHash, and the top-level
      updates/majors counts are consistent. Exit 0/1.

  materialized_rel <vault> <pkg_id>
      Print the vault-relative .penpot path the package materialized to (lock
      fileId -> manifest path). Used by the shell to raw-hash the consumer file
      before/after an edit (byte-unchanged proof).

  preserve_drift <base> <vault> <pkg_id>
      POST /__api/packages/preserve-drift {id}. Asserts a <stem>.conflict-<ts>
      copy appeared next to the installed file, that the copy is a valid .penpot
      dir, and that NEITHER the installed materialized file NOR the package
      source tree changed bytes (overwrite-neither-side). Exit 0/1.

  rows_snapshot <base>
      Print a canonical JSON snapshot of the indexed package rows (id, name,
      version, kind, fileId, deepLink), sorted by id — compared before/after an
      index-db delete to prove the rebuild is identical (invariant 1).

Stdlib only.
"""

import hashlib
import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

LOCK_NAME = "lock.json"
MANIFEST_NAME = ".penpot-sync.json"
PACKAGES_DIR = ".penpot-packages"
HEX64 = re.compile(r"^[0-9a-f]{64}$")
# The exact conflict-copy shape the E4b drift path writes (mirrors
# sync_daemon::paths::conflict_path_for): <stem>.conflict-<ts>.penpot
CONFLICT_RE = re.compile(r"\.conflict-.+\.penpot$")
# The exact deep-link shape the gate asserts on (vault_index::workspace_deep_link).
DEEPLINK_RE = re.compile(r"^/#/workspace\?team-id=[^&]+&file-id=[^&]+")


# --------------------------------------------------------------- HTTP helpers

def http_get(url, timeout=60):
    req = urllib.request.Request(url, headers={"Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return resp.status, resp.read()


def http_get_json(url, timeout=60):
    st, body = http_get(url, timeout)
    return st, (json.loads(body) if body else None)


def http_post_json(url, payload, timeout=120):
    body = json.dumps(payload).encode()
    req = urllib.request.Request(
        url, data=body,
        headers={"Content-Type": "application/json", "Accept": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


# ------------------------------------------------------------ on-disk helpers

def lock_path(vault):
    return os.path.join(vault, LOCK_NAME)


def read_lock(vault):
    with open(lock_path(vault), "rb") as fh:
        return json.loads(fh.read())


def write_lock(vault, lock):
    """Write lock.json 2-space-indented with a trailing newline (the same
    git-diffable normalization the Rust writer uses; byte-exactness does not
    matter here — the indexer only READS it)."""
    with open(lock_path(vault), "w") as fh:
        json.dump(lock, fh, indent=2, sort_keys=True)
        fh.write("\n")


def read_manifest(vault):
    p = os.path.join(vault, MANIFEST_NAME)
    if not os.path.exists(p):
        return {}
    with open(p, "rb") as fh:
        return json.loads(fh.read())


def pkg_source_tree(vault, pkg_id):
    """First sorted child dir ending .penpot under .penpot-packages/<id>
    (mirrors installer discover_penpot_tree)."""
    pkg_dir = os.path.join(vault, PACKAGES_DIR, pkg_id)
    if not os.path.isdir(pkg_dir):
        return None
    for child in sorted(os.listdir(pkg_dir)):
        p = os.path.join(pkg_dir, child)
        if child.endswith(".penpot") and not child.startswith(".") and os.path.isdir(p):
            return p
    return None


def tree_sig(root):
    """Raw content signature of an on-disk tree: sha256 over sorted
    (relpath, sha256(bytes)). Byte-level — catches any write to any file."""
    entries = []
    for dp, _dirs, files in os.walk(root):
        for fn in files:
            p = os.path.join(dp, fn)
            rel = os.path.relpath(p, root).replace(os.sep, "/")
            with open(p, "rb") as fh:
                entries.append((rel, hashlib.sha256(fh.read()).hexdigest()))
    h = hashlib.sha256()
    for rel, digest in sorted(entries):
        h.update(rel.encode()); h.update(b"\0")
        h.update(digest.encode()); h.update(b"\n")
    return h.hexdigest()


def color_files(tree):
    """Every files/<fid>/colors/<cid>.json path (absolute) in a source tree."""
    out = []
    files_dir = os.path.join(tree, "files")
    if not os.path.isdir(files_dir):
        return out
    for fid in sorted(os.listdir(files_dir)):
        cdir = os.path.join(files_dir, fid, "colors")
        if os.path.isdir(cdir):
            for cf in sorted(os.listdir(cdir)):
                if cf.endswith(".json"):
                    out.append(os.path.join(cdir, cf))
    return out


def resolve_rel_path(base, file_id, wait_s=0):
    deadline = time.time() + float(wait_s)
    while True:
        try:
            _, bd = http_get_json(base + "/__api/vault/boards")
            for c in (bd or {}).get("boards", []):
                if c.get("fileId") == file_id:
                    return c.get("relPath")
        except Exception:
            pass
        if time.time() >= deadline:
            return None
        time.sleep(2)


# ------------------------------------------------------------------ commands

def wait_color_ondisk(base, vault, file_id, color_name, wait_s):
    deadline = time.time() + float(wait_s)
    rel = None
    while time.time() < deadline:
        rel = rel or resolve_rel_path(base, file_id, 4)
        if rel:
            tree = os.path.join(vault, rel)
            for cf in color_files(tree):
                try:
                    with open(cf, "rb") as fh:
                        if json.loads(fh.read()).get("name") == color_name:
                            print(f"PASS: exported color {color_name!r} landed on disk "
                                  f"({os.path.relpath(cf, vault)})")
                            print(json.dumps({"relPath": rel}))
                            return 0
                except Exception:
                    pass
        time.sleep(2)
    print(f"FAIL: color {color_name!r} never appeared on disk within {wait_s}s (rel={rel})")
    return 1


def seed_synthetic(vault, n, nonce):
    n = int(n)
    lock = read_lock(vault)
    pkgs = lock.setdefault("packages", {})
    probes = []
    now = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    for i in range(n):
        pid = f"e4syn-{i:04d}"
        token = f"{nonce}{i:04d}"
        pkgs[pid] = {
            "version": "1.0.0",
            "kind": "design-data",
            # A fresh content hash so needs_reindex fires and the row is built.
            "contentHash": hashlib.sha256(f"{pid}:{token}".encode()).hexdigest(),
            "contractHash": hashlib.sha256(f"c:{pid}".encode()).hexdigest(),
            "sourceGitUrl": "",
            "fileId": str(uuid.uuid4()),
            "name": f"E4 Synthetic Pkg {token}",
            "installedAt": now,
            "libraryShared": False,
            "pluginProps": {},
            "links": [],
        }
        # Probe a deterministic sample across the range (first, middle, last).
        if i in (0, n // 2, n - 1):
            probes.append({"q": token, "id": pid})
    write_lock(vault, lock)
    print(f"PASS: seeded {n} synthetic package entries into lock.json "
          f"(ids e4syn-0000..e4syn-{n-1:04d})")
    print(json.dumps({"seeded": n, "probes": probes}))
    return 0


def unseed_synthetic(vault):
    lock = read_lock(vault)
    pkgs = lock.get("packages", {})
    removed = [k for k in list(pkgs) if k.startswith("e4syn-")]
    for k in removed:
        del pkgs[k]
    write_lock(vault, lock)
    print(f"PASS: removed {len(removed)} synthetic package entries from lock.json")
    return 0


def search_wait(base, min_count, wait_s):
    min_count = int(min_count)
    deadline = time.time() + float(wait_s)
    last = -1
    while time.time() < deadline:
        try:
            _, d = http_get_json(base + "/__api/packages/search?q=&limit=1000")
            last = (d or {}).get("count", 0)
            if last >= min_count:
                print(f"PASS: gallery indexed {last} packages (>= {min_count})")
                return 0
        except Exception:
            pass
        time.sleep(2)
    print(f"FAIL: gallery only reached {last} packages (< {min_count}) within {wait_s}s")
    return 1


def search_assert(base, query, expect_id, max_ms):
    max_ms = float(max_ms)
    try:
        _, d = http_get_json(base + "/__api/packages/search?q=" +
                             urllib.parse.quote(query) + "&limit=10")
    except Exception as e:
        print(f"FAIL: search q={query!r} errored: {e}")
        return 1
    pkgs = (d or {}).get("packages", [])
    took = (d or {}).get("tookMs", 1e9)
    ok = True
    got = pkgs[0]["id"] if pkgs else None
    if got == expect_id:
        print(f"PASS: search q={query!r} returned the correct id ({expect_id})")
    else:
        print(f"FAIL: search q={query!r} top id was {got!r}, expected {expect_id!r} "
              f"(count={len(pkgs)})")
        ok = False
    if took < max_ms:
        print(f"PASS: search q={query!r} served in {took:.2f}ms (< {max_ms:.0f}ms)")
    else:
        print(f"FAIL: search q={query!r} took {took:.2f}ms (>= {max_ms:.0f}ms)")
        ok = False
    return 0 if ok else 1


def source_add_color(vault, pkg_id, new_name):
    tree = pkg_source_tree(vault, pkg_id)
    if not tree:
        print(f"FAIL: no .penpot source tree for {pkg_id}")
        return 1
    cfs = color_files(tree)
    if not cfs:
        print(f"FAIL: package {pkg_id} source has no color to clone (need a baseline color)")
        return 1
    with open(cfs[0], "rb") as fh:
        base_color = json.loads(fh.read())
    new_id = str(uuid.uuid4())
    base_color["id"] = new_id
    base_color["name"] = new_name
    cdir = os.path.dirname(cfs[0])
    dest = os.path.join(cdir, new_id + ".json")
    with open(dest, "w") as fh:
        json.dump(base_color, fh, indent=2, sort_keys=True)
        fh.write("\n")
    print(f"PASS: added exported color {new_name!r} to {pkg_id} source (MINOR grow)")
    print(json.dumps({"relPath": os.path.relpath(dest, vault)}))
    return 0


def source_remove_color(vault, pkg_id, name_substr):
    tree = pkg_source_tree(vault, pkg_id)
    if not tree:
        print(f"FAIL: no .penpot source tree for {pkg_id}")
        return 1
    for cf in color_files(tree):
        try:
            with open(cf, "rb") as fh:
                name = json.loads(fh.read()).get("name", "")
        except Exception:
            continue
        if name_substr in name:
            os.remove(cf)
            print(f"PASS: removed exported color {name!r} from {pkg_id} source (MAJOR)")
            print(json.dumps({"relPath": os.path.relpath(cf, vault)}))
            return 0
    print(f"FAIL: no color matching {name_substr!r} in {pkg_id} source")
    return 1


def updates_wait(base, pkg_id, expect_bump, wait_s):
    deadline = time.time() + float(wait_s)
    last_row = None
    while time.time() < deadline:
        try:
            _, d = http_get_json(base + "/__api/packages/updates")
        except Exception:
            d = None
        rows = (d or {}).get("rows", [])
        row = next((r for r in rows if r.get("id") == pkg_id), None)
        last_row = row
        if row and row.get("updateAvailable") and row.get("bump") == expect_bump:
            ok = True

            def check(cond, msg):
                nonlocal ok
                print(("PASS: " if cond else "FAIL: ") + msg)
                ok = ok and cond

            check(row["bump"] == expect_bump,
                  f"{pkg_id} surfaced a {expect_bump} bump within the poll+debounce window")
            check(row.get("isMajor") == (expect_bump == "major"),
                  f"isMajor={row.get('isMajor')} consistent with bump={expect_bump}")
            check(bool(HEX64.match(row.get("pinnedContractHash", ""))) and
                  bool(HEX64.match(row.get("liveContractHash", ""))),
                  "pinned + live contract hashes are 64-hex digests")
            check(row.get("pinnedContractHash") != row.get("liveContractHash"),
                  "pinnedContractHash != liveContractHash (the source moved)")
            check(DEEPLINK_RE.match(row.get("deepLink", "")) is not None,
                  "row carries an exact /#/workspace deep link")
            if expect_bump == "major":
                check((d or {}).get("majors", 0) >= 1,
                      f"top-level majors count reflects the major bump ({d.get('majors')})")
            check((d or {}).get("updates", 0) >= 1,
                  f"top-level updates count reflects the surfaced update ({d.get('updates')})")
            return 0 if ok else 1
        time.sleep(1)
    print(f"FAIL: {pkg_id} did not surface a {expect_bump} bump within {wait_s}s "
          f"(last row: {last_row})")
    return 1


def materialized_rel(vault, pkg_id):
    lock = read_lock(vault)
    entry = lock.get("packages", {}).get(pkg_id)
    if not entry:
        print(f"FAIL: {pkg_id} not in lock.json", file=sys.stderr)
        return 1
    man = read_manifest(vault)
    ent = man.get("files", {}).get(entry["fileId"])
    if not ent:
        print(f"FAIL: {pkg_id} fileId {entry['fileId']} not in manifest", file=sys.stderr)
        return 1
    print(ent["path"])
    return 0


def preserve_drift(base, vault, pkg_id):
    ok = True

    def check(cond, msg):
        nonlocal ok
        print(("PASS: " if cond else "FAIL: ") + msg)
        ok = ok and cond

    # Locate both sides and snapshot their bytes BEFORE preserving drift.
    lock = read_lock(vault)
    entry = lock.get("packages", {}).get(pkg_id, {})
    man = read_manifest(vault)
    installed_rel = man.get("files", {}).get(entry.get("fileId", ""), {}).get("path")
    source_tree = pkg_source_tree(vault, pkg_id)
    if not installed_rel or not source_tree:
        print(f"FAIL: could not resolve installed_rel/source for {pkg_id} "
              f"(installed_rel={installed_rel}, source={source_tree})")
        return 1
    installed_dir = os.path.join(vault, installed_rel)
    installed_before = tree_sig(installed_dir)
    source_before = tree_sig(source_tree)

    st, body = http_post_json(base + "/__api/packages/preserve-drift", {"id": pkg_id})
    resp = json.loads(body) if body else {}
    check(st == 200 and resp.get("ok") is True,
          f"POST /preserve-drift {pkg_id} ok (HTTP {st})")
    copy_rel = resp.get("conflictCopyPath")
    check(bool(copy_rel) and CONFLICT_RE.search(copy_rel or "") is not None,
          f"response names a <stem>.conflict-<ts>.penpot copy ({copy_rel})")
    if copy_rel:
        copy_dir = os.path.join(vault, copy_rel)
        check(os.path.isdir(copy_dir) and os.path.exists(os.path.join(copy_dir, "manifest.json")),
              "the conflict copy is a real .penpot dir on disk (has manifest.json)")

    # Overwrite-neither-side: both original trees byte-identical after the copy.
    check(tree_sig(installed_dir) == installed_before,
          "installed materialized .penpot left BYTE-UNCHANGED (overwrote neither side)")
    check(tree_sig(source_tree) == source_before,
          "package source tree left BYTE-UNCHANGED (overwrote neither side)")
    return 0 if ok else 1


def rows_snapshot(base):
    _, d = http_get_json(base + "/__api/packages/search?q=&limit=1000")
    rows = []
    for p in (d or {}).get("packages", []):
        rows.append({k: p.get(k) for k in
                     ("id", "name", "version", "kind", "fileId", "deepLink")})
    rows.sort(key=lambda r: r["id"])
    print(json.dumps(rows, sort_keys=True))
    return 0


def main():
    if len(sys.argv) < 2:
        print("usage: e4_gallery_helper.py <subcommand> ...", file=sys.stderr)
        return 2
    cmd, a = sys.argv[1], sys.argv[2:]
    table = {
        "wait_color_ondisk": lambda: wait_color_ondisk(a[0], a[1], a[2], a[3], a[4]),
        "seed_synthetic": lambda: seed_synthetic(a[0], a[1], a[2]),
        "unseed_synthetic": lambda: unseed_synthetic(a[0]),
        "search_wait": lambda: search_wait(a[0], a[1], a[2]),
        "search_assert": lambda: search_assert(a[0], a[1], a[2], a[3]),
        "source_add_color": lambda: source_add_color(a[0], a[1], a[2]),
        "source_remove_color": lambda: source_remove_color(a[0], a[1], a[2]),
        "updates_wait": lambda: updates_wait(a[0], a[1], a[2], a[3]),
        "materialized_rel": lambda: materialized_rel(a[0], a[1]),
        "preserve_drift": lambda: preserve_drift(a[0], a[1], a[2]),
        "rows_snapshot": lambda: rows_snapshot(a[0]),
    }
    fn = table.get(cmd)
    if not fn:
        print(f"unknown subcommand {cmd!r}", file=sys.stderr)
        return 2
    return fn()


if __name__ == "__main__":
    sys.exit(main())
