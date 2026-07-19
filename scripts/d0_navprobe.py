#!/usr/bin/env python3
"""D0 probe runner: read the navwatch JSONL and report what was observed."""
import json
import sys


def observations(log_path):
    """Every URL the webview reported, in order. Missing file => []."""
    out = []
    try:
        with open(log_path, "r", encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    out.append(json.loads(line)["url"])
                except (ValueError, KeyError):
                    continue
    except FileNotFoundError:
        return []
    return out


def saw_fragment(urls, fragment):
    """Did the webview report a navigation whose fragment matches?"""
    return any(fragment in u for u in urls)


def _selftest():
    import tempfile, os
    fd, p = tempfile.mkstemp()
    os.close(fd)
    with open(p, "w", encoding="utf-8") as fh:
        fh.write('{"source":"on_navigation","url":"http://x/#/dashboard"}\n')
        fh.write("not json\n")
        fh.write('{"source":"on_navigation","url":"http://x/__home"}\n')
    urls = observations(p)
    assert urls == ["http://x/#/dashboard", "http://x/__home"], urls
    assert saw_fragment(urls, "#/dashboard")
    assert not saw_fragment(urls, "#/settings")
    assert observations("/nonexistent/path") == []
    os.unlink(p)
    print("selftest OK")


if __name__ == "__main__":
    if len(sys.argv) >= 2 and sys.argv[1] == "selftest":
        _selftest()
    elif len(sys.argv) == 4 and sys.argv[1] == "observe":
        log_path, case = sys.argv[2], sys.argv[3]
        urls = observations(log_path)
        want = {"hash": "#/dashboard", "pushstate": "#/settings", "full": "stage=second"}[case]
        print(json.dumps({"case": case, "observed": saw_fragment(urls, want), "urls": urls}))
    else:
        print("usage: d0_navprobe.py selftest | observe <log> <hash|pushstate|full>", file=sys.stderr)
        sys.exit(2)
