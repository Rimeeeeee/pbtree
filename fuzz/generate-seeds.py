#!/usr/bin/env python3
"""Regenerate the curated edge-case seeds from operations and canonical artifacts."""

import argparse
from pathlib import Path

root = Path(__file__).resolve().parent / "seeds"


def record(action, key, value=0):
    assert 1 <= len(key) <= 8
    return bytes([action, len(key) - 1]) + key.ljust(8, b"\0") + value.to_bytes(32, "big")


def operations(items):
    return b"".join(record(*item) for item in items)


tree_cases = {
    "zero-leaf-replace-delete": [(0, b"\0", 0), (1, b"\0", (1 << 256) - 1), (1, b"\0", 0), (2, b"\0"), (2, b"\0")],
    "prefix-conflicts-both-orders": [(0, b"\0", 1), (0, b"\0\1", 2), (2, b"\0"), (0, b"\0\1", 3), (0, b"\0", 4)],
    "eight-byte-boundaries": [(0, b"\0" * 8, 1), (0, b"\0" * 7 + b"\1", 2), (0, b"\0" * 7 + b"\x80", 3), (0, b"\xff" * 8, 4)],
    "common-prefix-collapse": [(0, b"\0" * 7 + bytes([last]), last) for last in [0, 1, 2, 128]] + [(2, b"\0" * 7 + bytes([last])) for last in [1, 0, 128, 2]],
    "wide-root-branches": [(0, bytes([key]), key) for key in [0, 1, 127, 128, 254, 255]],
    "proper-prefix-lookups": [(0, b"\xf0\0", 1), (0, b"\xf0\x80", 2), (3, b"\xf0"), (3, b"\xf0\1"), (2, b"\xf0")],
    "selected-existing-key-updates": [(0, b"\1", 1), (0, b"\2", 2), (8, b"\0", 0), (9, b"\1", 255), (10, b"\1")],
    "read-only-empty": [(3, b"\0"), (2, b"\xff"), (3, b"\0" * 8)],
    "delete-all-reinsert": [(0, bytes([key]), key) for key in range(16)] + [(2, bytes([key])) for key in reversed(range(16))] + [(0, bytes([key]), 255 - key) for key in reversed(range(16))],
}
seeds = {root / "tree_ops" / name: operations(items) for name, items in tree_cases.items()}
seeds[root / "tree_ops" / "truncated-final-record"] = operations([(0, b"\1", 7)]) + b"\1\7\xff"

canonical = (root / "artifact_decode" / "canonical-1").read_bytes()
length = int.from_bytes(canonical[1:5], "little")
snapshot, preimages = canonical[5:5 + length], canonical[5 + length:]


def artifact(snapshot_bytes, preimage_bytes, selector=1):
    return bytes([selector]) + len(snapshot_bytes).to_bytes(4, "little") + snapshot_bytes + preimage_bytes


artifact_cases = {
    "reject-truncated-header": b"\1\0",
    "reject-oversized-snapshot-length": b"\1\xff\xff\xff\xff\0",
    "reject-truncated-snapshot": artifact(snapshot[:-1], preimages),
    "reject-truncated-preimages": artifact(snapshot, preimages[:-1]),
    "reject-snapshot-trailing-byte": artifact(snapshot + b"\0", preimages),
    "reject-preimages-trailing-byte": artifact(snapshot, preimages + b"\0"),
    "reject-unknown-tag": artifact(b"\xff" + snapshot[1:], preimages),
    "reject-wrong-trusted-root": artifact(snapshot, preimages, selector=0),
    "reject-preimage-mismatch": artifact(snapshot, preimages[:-1] + bytes([preimages[-1] ^ 1])),
    "reject-snapshot-mismatch": artifact(snapshot[:-1] + bytes([snapshot[-1] ^ 1]), preimages),
    "reject-empty-snapshot": artifact(b"", preimages),
    "reject-empty-preimages": artifact(snapshot, b""),
}
seeds.update({root / "artifact_decode" / name: data for name, data in artifact_cases.items()})

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--check", action="store_true", help="verify checked-in seeds instead of writing them")
args = parser.parse_args()
for path, data in seeds.items():
    if args.check:
        if path.read_bytes() != data:
            raise ValueError(f"Seed differs: {path}")
    else:
        path.write_bytes(data)
print(f"{'Checked' if args.check else 'Generated'} {len(seeds)} edge-case seeds")
