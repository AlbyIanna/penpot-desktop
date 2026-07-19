#!/usr/bin/env python3
"""D1: process-level egress observer.

Parses `lsof -nP -i` output and separates loopback peers (our own supervised
stack — the whole architecture) from anything leaving the machine (forbidden).
Pure text processing so it is unit-testable without opening a socket.
"""
import json
import re
import sys

LOOPBACK_HOSTS = {"127.0.0.1", "::1", "0:0:0:0:0:0:0:1", "localhost", "*", ""}

# lsof NAME column looks like: 127.0.0.1:6508->127.0.0.1:54321 (ESTABLISHED)
#                          or  [::1]:5581 (LISTEN)
PEER_RE = re.compile(r"->\[?([^\]\s]+?)\]?:(\d+)")

# A dotted-quad IPv4 literal in 127.0.0.0/8, e.g. "127.0.0.1" or "127.1.2.3".
# Each octet must be a real 0-255 value with no extra characters, so this
# does NOT match a hostname that merely starts with "127." (e.g.
# "127.0.0.1.evil.com" has a trailing label). lsof yields numeric hosts, so
# this case is not currently exploitable here, but the predicate is hardened
# to match scripts/d1_surfaces.cjs's isLoopback for consistency.
_IPV4_127_RE = re.compile(
    r"^127\.(25[0-5]|2[0-4]\d|1?\d{1,2})\.(25[0-5]|2[0-4]\d|1?\d{1,2})\."
    r"(25[0-5]|2[0-4]\d|1?\d{1,2})$"
)


def _host_is_loopback(host):
    return host in LOOPBACK_HOSTS or bool(_IPV4_127_RE.match(host))


def parse(text):
    """Every outbound peer seen, split into loopback and non-loopback."""
    conns, bad = [], []
    for line in text.splitlines():
        m = PEER_RE.search(line)
        if not m:
            continue
        host, port = m.group(1), m.group(2)
        entry = {"host": host, "port": int(port)}
        conns.append(entry)
        if not _host_is_loopback(host):
            bad.append(entry)
    return {"connections": conns, "nonLoopback": bad}


def _selftest():
    sample = "\n".join([
        "java 123 u IPv4 TCP 127.0.0.1:6508->127.0.0.1:54321 (ESTABLISHED)",
        "java 123 u IPv6 TCP [::1]:5581 (LISTEN)",
        "penpot 456 u IPv4 TCP 192.168.1.9:53344->142.250.1.1:443 (ESTABLISHED)",
    ])
    out = parse(sample)
    assert len(out["connections"]) == 2, out
    assert out["nonLoopback"] == [{"host": "142.250.1.1", "port": 443}], out
    assert parse("")["nonLoopback"] == []

    # Positive cases: genuine loopback literals/names must be classified
    # loopback (safe, excluded from nonLoopback).
    positive_hosts = ["127.0.0.1", "127.1.2.3", "127.255.255.255", "localhost",
                       "::1", "0:0:0:0:0:0:0:1", "*", ""]
    for host in positive_hosts:
        assert _host_is_loopback(host), f"expected loopback: {host!r}"

    # Negative cases: a hostname that merely *starts with* "127." (or is
    # otherwise not a genuine loopback literal) must NOT be classified
    # loopback. This is the security boundary of the whole milestone — if
    # this wrongly says "loopback", real egress silently vanishes from the
    # evidence.
    negative_hosts = [
        "127.0.0.1.evil.com",
        "127.evil.com",
        "1270.0.0.1",
        "12.7.0.0.1",
        "example.com",
        "0.0.0.0",
        "169.254.169.254",
        "127.0.0.256",  # octet out of range
    ]
    for host in negative_hosts:
        assert not _host_is_loopback(host), f"expected NON-loopback: {host!r}"

    # And prove it end-to-end through parse(): a trailing-label host that
    # merely starts with "127." must show up in nonLoopback, not disappear.
    spoof_sample = (
        "penpot 789 u IPv4 TCP 192.168.1.9:53345->127.0.0.1.evil.com:443 (ESTABLISHED)"
    )
    spoof_out = parse(spoof_sample)
    assert spoof_out["nonLoopback"] == [
        {"host": "127.0.0.1.evil.com", "port": 443}
    ], spoof_out

    print("selftest OK")


if __name__ == "__main__":
    if len(sys.argv) == 2 and sys.argv[1] == "selftest":
        _selftest()
    elif len(sys.argv) == 3 and sys.argv[1] == "parse":
        with open(sys.argv[2], "r", encoding="utf-8") as fh:
            print(json.dumps(parse(fh.read())))
    else:
        print("usage: d1_egress.py selftest | parse <lsof_output>", file=sys.stderr)
        sys.exit(2)
