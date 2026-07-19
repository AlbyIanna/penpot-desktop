#!/usr/bin/env python3
"""D1: process-level egress observer.

Parses `lsof -nP -i` output and separates loopback peers (our own supervised
stack — the whole architecture) from anything leaving the machine (forbidden).
Pure text processing so it is unit-testable without opening a socket.
"""
import json
import re
import sys

LOOPBACK_HOSTS = {"127.0.0.1", "::1", "localhost", "*", ""}

# lsof NAME column looks like: 127.0.0.1:6508->127.0.0.1:54321 (ESTABLISHED)
#                          or  [::1]:5581 (LISTEN)
PEER_RE = re.compile(r"->\[?([^\]\s]+?)\]?:(\d+)")


def _host_is_loopback(host):
    return host in LOOPBACK_HOSTS or host.startswith("127.")


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
