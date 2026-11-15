#!/usr/bin/env python3
"""Scan a capability run directory for leaked provider credentials.

The evaluator passes the real API key through the environment, so every run must be checked
before its artifacts are kept or shared. This scans for the exact key from `~/.orca/auth.json`,
for `Authorization: Bearer ...` values, and for unquoted `sk-...` tokens that are not part of a
task/session id. Exits 1 when the exact key appears; other hits are reported as `review`.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys

ID_FRAGMENT = re.compile(r"(task|session|turn|call|item|run|tool)-")
# The Orca repository ships `sk-test-...` keys in its own provider tests; the capability run
# copies that workspace for the real-repo rungs, so those tokens are expected, not leaked.
BENIGN_SK = ("sk-test-", "sk-example", "sk-dummy")
SK_TOKEN = re.compile(r"(?<![A-Za-z0-9_-])sk-[A-Za-z0-9_-]{16,}")
AUTH_BEARER = re.compile(r"authorization\s*[:=]\s*['\"]?bearer\s+([A-Za-z0-9._-]{8,})", re.I)


def provider_key() -> str | None:
    path = pathlib.Path.home() / ".orca" / "auth.json"
    try:
        return json.loads(path.read_text(encoding="utf-8")).get("DEEPSEEK_API_KEY")
    except (OSError, ValueError):
        return None


def scan(roots: list[pathlib.Path], key: str | None) -> dict:
    exact, bearer, sk_tokens, scanned = [], [], [], 0
    for root in roots:
        for path in root.rglob("*"):
            if not path.is_file() or path.stat().st_size > 20_000_000:
                continue
            try:
                text = path.read_text(encoding="utf-8", errors="ignore")
            except OSError:
                continue
            scanned += 1
            if key and key in text:
                exact.append(str(path))
            for match in AUTH_BEARER.finditer(text):
                bearer.append({"path": str(path), "match": match.group(1)[:6] + "..."})
            for match in SK_TOKEN.finditer(text):
                before = text[max(0, match.start() - 6):match.start()]
                if ID_FRAGMENT.search(before) or match.group(0).startswith(BENIGN_SK):
                    continue
                sk_tokens.append({"path": str(path), "match": match.group(0)[:8] + "..."})
    return {
        "scanned_files": scanned,
        "exact_key_hits": exact,
        "authorization_bearer_hits": bearer,
        "sk_token_hits": sk_tokens,
        "ok": not exact,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("roots", nargs="+", type=pathlib.Path)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    report = scan(args.roots, provider_key())
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print(f"scanned {report['scanned_files']} files")
        print(f"exact key hits: {len(report['exact_key_hits'])}")
        print(f"authorization bearer hits: {len(report['authorization_bearer_hits'])} (review)")
        print(f"unrelated sk- token hits: {len(report['sk_token_hits'])} (review)")
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
