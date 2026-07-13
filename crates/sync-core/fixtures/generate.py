#!/usr/bin/env python3
"""Fixture generator for sync-core's Python-parity tests.

Regenerates:
  - cases/random_numbers.json  (deterministic pseudo-random corpus, seed fixed;
    floats written in NON-canonical `%.17g` / `%.17e` forms to exercise the
    parser, plus repr forms; ints across the full i64/u64 range)
  - expected/<name>.json for every cases/<name>.json, using the EXACT
    normalizer from scripts/roundtrip.py:

        json.dumps(obj, sort_keys=True, indent=2, ensure_ascii=False) + "\n"

Run from anywhere: `python3 crates/sync-core/fixtures/generate.py`.
The outputs are checked in; the Rust test `tests/python_parity.rs` asserts
byte-for-byte equality between the Rust normalizer and expected/.

Intentionally NOT covered here (documented Rust-is-spec divergences, see
crates/sync-core/src/normalize.rs docs): integer literals outside i64/u64,
the integer literal `-0`, and NaN/Infinity/overflowing tokens.
"""

import json
import math
import os
import random
import struct

HERE = os.path.dirname(os.path.abspath(__file__))
CASES = os.path.join(HERE, "cases")
EXPECTED = os.path.join(HERE, "expected")

SEED = 20260713


def gen_random_numbers():
    rng = random.Random(SEED)
    floats = []
    # Uniform over the entire double bit space (subnormals, extremes, ...).
    while len(floats) < 4000:
        v = struct.unpack("<d", struct.pack("<Q", rng.getrandbits(64)))[0]
        if math.isnan(v) or math.isinf(v):
            continue
        floats.append(v)
    # Values shaped like real design coordinates/opacities.
    for _ in range(2000):
        floats.append(rng.uniform(-1e6, 1e6))
    for _ in range(1000):
        floats.append(rng.random())
    for _ in range(500):
        floats.append(round(rng.uniform(0, 4000), rng.randint(0, 6)))
    # Powers of ten straddling the fixed/scientific thresholds.
    for e in range(-30, 31):
        floats.append(float(10.0 ** e))
        floats.append(-(10.0 ** e))

    ints = [rng.randint(-2**63, 2**63 - 1) for _ in range(500)]
    ints += [rng.randint(0, 2**64 - 1) for _ in range(500)]
    ints += [0, 1, -1, 2**53, 2**53 + 1, -2**53, 2**63 - 1, -2**63, 2**64 - 1]

    # Write the INPUT with mixed, non-canonical (but round-trip-exact) float
    # spellings so the test also proves parse parity, not just format parity.
    reprs = []
    for i, v in enumerate(floats):
        if i % 3 == 0:
            reprs.append("%.17g" % v)
        elif i % 3 == 1:
            reprs.append("%.17e" % v)
        else:
            reprs.append(repr(v))
    raw = (
        '{"floats": ['
        + ",".join(reprs)
        + '], "ints": ['
        + ",".join(str(i) for i in ints)
        + "]}"
    )
    # Sanity: every spelling above must parse back to the exact same double.
    parsed = json.loads(raw)
    assert parsed["floats"] == floats, "non-canonical spellings must round-trip"
    assert parsed["ints"] == ints
    with open(os.path.join(CASES, "random_numbers.json"), "w") as fh:
        fh.write(raw)


def main():
    os.makedirs(CASES, exist_ok=True)
    os.makedirs(EXPECTED, exist_ok=True)
    gen_random_numbers()
    for name in sorted(os.listdir(CASES)):
        if not name.endswith(".json"):
            continue
        with open(os.path.join(CASES, name), "rb") as fh:
            obj = json.loads(fh.read().decode("utf-8"))
        out = json.dumps(obj, sort_keys=True, indent=2, ensure_ascii=False) + "\n"
        with open(os.path.join(EXPECTED, name), "wb") as fh:
            fh.write(out.encode("utf-8"))
        print(f"generated expected/{name}")


if __name__ == "__main__":
    main()
