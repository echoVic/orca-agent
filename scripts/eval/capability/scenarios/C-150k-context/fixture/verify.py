#!/usr/bin/env python3
"""C-axis verifier: answer.txt must carry the value named by the buried constraint."""
import os
import sys

expected_sku = os.environ["CAPABILITY_EXPECTED_SKU"]
path = "answer.txt"
try:
    text = open(path, encoding="utf-8").read()
except OSError:
    print("answer.txt is missing")
    sys.exit(1)
if expected_sku not in text:
    print(f"answer.txt does not contain {expected_sku!r}: {text[:200]!r}")
    sys.exit(1)
if "ZX-0000" in text:
    print("answer.txt contains the decoy value")
    sys.exit(1)
print(f"ok: {expected_sku} found")
