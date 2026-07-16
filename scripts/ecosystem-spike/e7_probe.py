#!/usr/bin/env python3
"""E7 plugin-activation spike RPC probe (scripts/e7-plugins-spike.sh).

Backend-RPC-side witnesses (the browser leg drives the native UI; this file
proves what actually landed in the DB). Reuses scripts/roundtrip.py as the RPC
client library (rt.Client) exactly like the E2-E6 helpers.

Subcommands (each prints one JSON line on stdout as its last line; exit 0/1):

  seed_file
      Create (or reuse) a spike design file in the default project. Prints
      {teamId, projectId, fileId, pageId, deepLink} — the browser leg opens
      deepLink to reach the workspace.

  profile_props
      Print the CURRENT profile props as JSON: {props: {...}, hasPlugins: bool,
      pluginsProps: <the "plugins" subtree or null>}. This is the E7 pointer
      capture (consent-pre: hasPlugins must be false; post-install: true).

  apply_props <props_json_file>
      Re-apply a previously captured plugin-registry pointer via the PUBLIC
      update-profile-props RPC — the delete-DB re-registration recipe. The
      file holds the captured "plugins" props VALUE. Prints {applied: true,
      verified: bool} after reading the props back.

  count_shapes <file_id> <name> [timeout_s]
      Poll get-file until >=1 object named <name> exists (or timeout). Prints
      {count, fileId}. Exit 0 iff count >= wanted (env E7_WANT_COUNT, def 1).

Env: PENPOT_BACKEND, PENPOT_TOKEN (rt.Client conventions).
Stdlib only + scripts/roundtrip.py.
"""

import json
import os
import sys
import time
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))
import roundtrip as rt  # noqa: E402

FILE_NAME = "e7-plugin-spike-canvas"


def out(obj, ok=True):
    print(json.dumps(obj, sort_keys=True))
    sys.exit(0 if ok else 1)


def client():
    c = rt.Client()
    c.login()
    return c


def default_project(c):
    profile = c.rpc("get-profile", {})
    team_id = profile.get("defaultTeamId")
    projects = c.rpc("get-projects", {"teamId": team_id})
    proj = next((p for p in projects if p.get("isDefault")), projects[0])
    return profile, team_id, proj["id"]


def cmd_seed_file():
    c = client()
    _, team_id, project_id = default_project(c)
    files = c.rpc("get-project-files", {"projectId": project_id})
    fid = next((f["id"] for f in files if f["name"] == FILE_NAME), None)
    if fid is None:
        created = c.rpc("create-file", {"name": FILE_NAME, "projectId": project_id})
        fid = created["id"]
        page_id = created["data"]["pages"][0]
    else:
        data = c.rpc("get-file", {"id": fid})
        page_id = data["data"]["pages"][0]
    deep = f"/#/workspace?team-id={team_id}&file-id={fid}&page-id={page_id}"
    out({"teamId": team_id, "projectId": project_id, "fileId": fid,
         "pageId": page_id, "deepLink": deep})


def cmd_profile_props():
    c = client()
    profile = c.rpc("get-profile", {})
    props = profile.get("props") or {}
    plugins = props.get("plugins")
    out({"props": props, "hasPlugins": plugins is not None,
         "pluginsProps": plugins})


def cmd_apply_props(path):
    with open(path, encoding="utf-8") as f:
        plugins_value = json.load(f)
    c = client()
    # The same PUBLIC RPC the SPA itself calls on install (same class as
    # import-binfile): write the plugin-registry pointer under props.plugins.
    c.rpc("update-profile-props", {"props": {"plugins": plugins_value}})
    profile = c.rpc("get-profile", {})
    stored = (profile.get("props") or {}).get("plugins")
    verified = json.dumps(stored, sort_keys=True) == json.dumps(
        plugins_value, sort_keys=True)
    out({"applied": True, "verified": verified, "stored": stored}, ok=verified)


def count_named(data, name):
    n = 0
    for page in (data.get("data", {}).get("pagesIndex") or {}).values():
        for obj in (page.get("objects") or {}).values():
            if obj.get("name") == name:
                n += 1
    return n


def cmd_count_shapes(file_id, name, timeout_s):
    want = int(os.environ.get("E7_WANT_COUNT", "1"))
    c = client()
    deadline = time.time() + timeout_s
    count = 0
    while time.time() < deadline:
        data = c.rpc("get-file", {"id": file_id})
        count = count_named(data, name)
        if count >= want:
            break
        time.sleep(2)
    out({"count": count, "fileId": file_id, "want": want}, ok=count >= want)


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd == "seed_file":
        cmd_seed_file()
    elif cmd == "profile_props":
        cmd_profile_props()
    elif cmd == "apply_props":
        cmd_apply_props(sys.argv[2])
    elif cmd == "count_shapes":
        timeout = float(sys.argv[4]) if len(sys.argv) > 4 else 60.0
        cmd_count_shapes(sys.argv[2], sys.argv[3], timeout)
    else:
        print("usage: e7_probe.py seed_file|profile_props|apply_props|count_shapes ...",
              file=sys.stderr)
        sys.exit(64)


if __name__ == "__main__":
    main()
