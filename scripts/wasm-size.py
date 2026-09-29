#!/usr/bin/env python3
"""Measure a built Soroban contract Wasm and emit benchmark metrics as JSON.

Covers the "WASM size benchmarks" requirement from Issue #406. Size matters for
two distinct reasons, so both are reported:

* **raw / gzip bytes** - the deployment footprint. Gzip is what actually crosses
  the network when a contract is uploaded, and a raw-size regression that gzip
  hides (or vice versa) is worth seeing separately.
* **`contractspecv0`** - the embedded contract interface. Its length and digest
  are what change when the *public API* changes, independent of how the code is
  compiled. Tracking it separately means a size regression can be attributed to
  either an interface change or an implementation change.

Usage:
    wasm-size.py <path/to/contract.wasm> [--out results.json]
"""

import argparse
import gzip
import hashlib
import json
import os
import re
import sys

# Custom sections a Soroban contract embeds. Anything matching contract* is
# picked up dynamically, but these are the ones we report individually because
# they are stable across builds.
KNOWN_SECTIONS = ("contractenvmetav0", "contractmetav0", "contractspecv0")


def read_uleb(buf: bytes, i: int):
    """Read an unsigned LEB128 value, returning (value, next_index)."""
    result = 0
    shift = 0
    while True:
        if i >= len(buf):
            raise ValueError("truncated LEB128")
        byte = buf[i]
        i += 1
        result |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return result, i
        shift += 7


def custom_sections(buf: bytes):
    """Yield (name, payload) for every custom section (section id 0)."""
    if buf[:4] != b"\x00asm":
        raise ValueError("not a wasm module: missing \\0asm magic")
    i = 8  # magic + version
    while i < len(buf):
        section_id = buf[i]
        i += 1
        size, i = read_uleb(buf, i)
        payload = buf[i:i + size]
        i += size
        if section_id != 0:
            continue
        name_len, j = read_uleb(payload, 0)
        name = payload[j:j + name_len].decode("utf-8", "replace")
        yield name, payload[j + name_len:]


def measure(path: str) -> dict:
    raw = open(path, "rb").read()
    sections = {}
    for name, payload in custom_sections(raw):
        if "contract" not in name.lower():
            continue
        sections[name] = {
            "size_bytes": len(payload),
            "sha256": hashlib.sha256(payload).hexdigest(),
            "identifiers": len(
                set(re.findall(rb"[a-z_][a-z0-9_]{3,}", payload))
            ),
        }

    spec = sections.get("contractspecv0")
    out = {
        "wasm_raw_bytes": len(raw),
        "wasm_gzip_bytes": len(gzip.compress(raw, compresslevel=9)),
        "contract_sections": sections,
    }
    if spec:
        out["contractspecv0_size_bytes"] = spec["size_bytes"]
        out["contractspecv0_sha256"] = spec["sha256"]
        out["contractspecv0_identifiers"] = spec["identifiers"]
    for name in KNOWN_SECTIONS:
        if name in sections:
            out[f"{name}_size_bytes"] = sections[name]["size_bytes"]
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("wasm", help="path to the built .wasm")
    ap.add_argument("--out", help="write JSON here instead of stdout")
    args = ap.parse_args()

    if not os.path.isfile(args.wasm):
        print(f"error: no such file: {args.wasm}", file=sys.stderr)
        print("       build it first: cargo build --target wasm32-unknown-unknown --release",
              file=sys.stderr)
        return 2

    try:
        result = measure(args.wasm)
    except ValueError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2

    text = json.dumps(result, indent=2, sort_keys=True)
    if args.out:
        os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
        with open(args.out, "w") as fh:
            fh.write(text + "\n")
        print(f"wasm size: {result['wasm_raw_bytes']} raw / "
              f"{result['wasm_gzip_bytes']} gzip bytes -> {args.out}")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
