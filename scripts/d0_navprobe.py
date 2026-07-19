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


def probe_ran(urls):
    """Proof-of-life: did the app get far enough to record ANY observation?

    Every successful launch records an initial-load line before anything else
    (verified live: {"source":"on_navigation","url":"tauri://localhost"}).
    An empty/missing log means the probe never ran (crash, timeout, wrong
    log path) — that is an infra failure, not a legitimate "not observed"
    measurement, and callers must not conflate the two.
    """
    return len(urls) > 0


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
    assert probe_ran(urls) is True
    assert probe_ran(observations("/nonexistent/path")) is False

    # A boot that crashed/timed out before any navigation was recorded: the
    # log file exists but is empty. This must read as "did not run", not as
    # a False measurement.
    fd2, empty_p = tempfile.mkstemp()
    os.close(fd2)
    assert observations(empty_p) == []
    assert probe_ran(observations(empty_p)) is False
    os.unlink(empty_p)

    # Proof-of-life example grounded in the verified real baseline line.
    baseline_urls = observations_from_lines(
        ['{"source":"on_navigation","url":"tauri://localhost"}']
    )
    assert probe_ran(baseline_urls) is True

    os.unlink(p)
    print("selftest OK")


def observations_from_lines(lines):
    """Test helper: same parsing as observations() but over in-memory lines."""
    out = []
    for line in lines:
        line = line.strip()
        if not line:
            continue
        try:
            out.append(json.loads(line)["url"])
        except (ValueError, KeyError):
            continue
    return out


if __name__ == "__main__":
    if len(sys.argv) >= 2 and sys.argv[1] == "selftest":
        _selftest()
    elif len(sys.argv) == 4 and sys.argv[1] == "observe":
        log_path, case = sys.argv[2], sys.argv[3]
        urls = observations(log_path)
        want = {"hash": "#/dashboard", "pushstate": "#/settings", "full": "stage=second"}[case]
        print(json.dumps({
            "case": case,
            "ran": probe_ran(urls),
            "observed": saw_fragment(urls, want),
            "urls": urls,
        }))
    else:
        print("usage: d0_navprobe.py selftest | observe <log> <hash|pushstate|full>", file=sys.stderr)
        sys.exit(2)
