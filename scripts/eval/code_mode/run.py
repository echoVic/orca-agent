#!/usr/bin/env python3
"""Compare `run_script` off and on for read-only repository questions.

Each task in tasks.json is run once per repetition with `[tools.script]`
off and once with it on, through `orca exec` against the real provider.
For each run this records the model rounds, the tools called, tokens, cost
and the final answer. Each answer is marked right or wrong against the checkout (expected.py), and
summary.json says whether the totals meet the rule for turning `run_script`
on by default.

The runs spend provider credit. Credentials and settings are copied from
--home into two throwaway Orca homes, so the user's own files are not
changed.

    python3 scripts/eval/code_mode/run.py --binary target/release/orca \
        --cwd /path/to/a/checkout --out /tmp/code-mode-eval
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent

sys.path.insert(0, str(HERE))

import expected  # noqa: E402

# One call of a script as the runtime writes it: number, batch mark, tool,
# arguments, how it ended, how long it took. The number is right-aligned in
# three columns, so a summary line starts with at most two spaces; a preview
# line starts with ten.
SCRIPT_CALL = re.compile(r"^ {0,2}\d+ . \S+ .* · (ok|failed) · \d+ms")


def make_home(source: Path, scripts_on: bool) -> Path:
    home = Path(tempfile.mkdtemp(prefix="orca-code-mode-eval-"))
    home.chmod(0o700)
    for name in ("auth.json", "config.toml"):
        if (source / name).exists():
            shutil.copy2(source / name, home / name)
    config = home / "config.toml"
    text = without_script_table(config.read_text(encoding="utf-8")) if config.exists() else ""
    if text and not text.endswith("\n"):
        text += "\n"
    text += f"\n[tools.script]\nenabled = {'true' if scripts_on else 'false'}\n"
    config.write_text(text, encoding="utf-8")
    return home


def without_script_table(text: str) -> str:
    """`text` without its `[tools.script]` table: the header and every line up
    to the next line that starts a table."""
    kept, skipping = [], False
    for line in text.splitlines(keepends=True):
        if re.match(r"\s*\[\s*tools\s*\.\s*script\s*\]\s*(#.*)?$", line):
            skipping = True
        elif line.lstrip().startswith("["):
            skipping = False
        if not skipping:
            kept.append(line)
    return "".join(kept)


def run_once(binary: str, home: Path, cwd: str, mode: str, prompt: str, timeout: float) -> dict:
    env = dict(os.environ, ORCA_HOME=str(home))
    started = time.time()
    try:
        done = subprocess.run(
            [binary, "exec", "--mode", mode, "--output-format", "jsonl",
             "--cwd", cwd, "--no-history", prompt],
            # Closed: exec reads a stdin that is not a terminal to its end as
            # more of the prompt, and an open pipe has no end.
            stdin=subprocess.DEVNULL,
            capture_output=True, text=True, timeout=timeout, env=env,
        )
        stdout, code = done.stdout, done.returncode
    except subprocess.TimeoutExpired as expired:
        stdout = expired.stdout or ""
        stdout = stdout.decode(errors="replace") if isinstance(stdout, bytes) else stdout
        code = None
    return {"wall_seconds": round(time.time() - started, 1), "exit": code, "jsonl": stdout}


def summarize(jsonl: str) -> dict:
    rounds, tools, usage, answer, script_calls = 0, [], {}, [], 0
    # One event per "\n": Orca's JSON keeps U+2028 and U+0085 as they are,
    # and splitlines() would end a line at either.
    for line in jsonl.split("\n"):
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind, payload = event.get("type"), event.get("payload") or {}
        if kind == "model.response.completed":
            rounds += 1
        elif kind == "tool.call.requested":
            tools.append(payload.get("name"))
            answer = []
        elif kind == "tool.output.delta":
            chunk = str(payload.get("chunk", ""))
            # Lines end at "\n" only: splitlines() also ends one at a U+2028
            # inside a call's arguments.
            script_calls += sum(1 for part in chunk.split("\n") if SCRIPT_CALL.match(part))
        elif kind == "usage.updated":
            usage = payload
        elif kind == "assistant.message.delta":
            answer.append(str(payload.get("delta") or payload.get("chunk") or payload.get("text") or ""))
    return {
        "rounds": rounds,
        "tool_calls": len(tools),
        "tools": tools,
        "script_tool_calls": script_calls,
        "input_tokens": usage.get("input_tokens"),
        "output_tokens": usage.get("output_tokens"),
        "cache_tokens": usage.get("cache_tokens"),
        "cost_usd": usage.get("estimated_cost_usd"),
        "answer": "".join(answer).strip(),
    }


def compare(rows: list[dict]) -> dict:
    """Totals for each variant, and whether they meet the rule for default-on:
    at least as many right answers, fewer model rounds, lower cost.

    A run is incomplete when it did not exit 0, has no cost, or made no model
    round (a timeout looks like that); its zeros would make a variant look
    cheap. Nothing is decided unless both variants have the same non-zero
    number of runs and none is incomplete."""
    totals = {}
    for variant in ("off", "on"):
        mine = [row for row in rows if row["variant"] == variant]
        totals[variant] = {
            "runs": len(mine),
            "right": sum(1 for row in mine if row["right"]),
            "rounds": sum(row["rounds"] for row in mine),
            "cost_usd": round(sum(row["cost_usd"] or 0 for row in mine), 6),
            "incomplete": sum(1 for row in mine
                              if row.get("exit") != 0 or row.get("cost_usd") is None
                              or not row.get("rounds")),
        }
    off, on = totals["off"], totals["on"]
    decidable = (off["runs"] > 0 and off["runs"] == on["runs"]
                 and off["incomplete"] == 0 and on["incomplete"] == 0)
    totals["decidable"] = decidable
    totals["default_on"] = (decidable and on["right"] >= off["right"]
                            and on["rounds"] < off["rounds"] and on["cost_usd"] < off["cost_usd"])
    return totals


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--cwd", required=True, help="checkout the questions are asked about")
    parser.add_argument("--out", required=True)
    parser.add_argument("--home", default=str(Path.home() / ".orca"))
    parser.add_argument("--mode", default="auto-edit",
                        help="approval mode; `plan` leaves the model without a shell")
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--timeout", type=float, default=600)
    parser.add_argument("--only", action="append", help="task id; may be given more than once")
    args = parser.parse_args()

    tasks = json.loads((HERE / "tasks.json").read_text(encoding="utf-8"))
    if args.only:
        tasks = [task for task in tasks if task["id"] in args.only]
    # Prove each checker can read this checkout before any credit is spent.
    for task in tasks:
        expected.is_right(task["id"], "", Path(args.cwd))
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    homes = {"off": make_home(Path(args.home), False), "on": make_home(Path(args.home), True)}
    rows = []
    try:
        for task in tasks:
            for repetition in range(args.repeat):
                for variant, home in homes.items():
                    prompt = task["prompt"]
                    if args.mode == "plan":
                        # A shell call in a headless plan run ends the run.
                        prompt += " There is no shell here: do not call bash, use the other tools."
                    result = run_once(args.binary, home, args.cwd, args.mode, prompt, args.timeout)
                    name = f"{task['id']}.{variant}.{repetition}"
                    (out / f"{name}.jsonl").write_text(result.pop("jsonl"), encoding="utf-8")
                    row = {"task": task["id"], "variant": variant, "repetition": repetition,
                           **result, **summarize((out / f"{name}.jsonl").read_text(encoding="utf-8"))}
                    row["right"] = expected.is_right(task["id"], row["answer"], Path(args.cwd))
                    rows.append(row)
                    print(json.dumps({key: value for key, value in row.items() if key not in ("answer", "tools")}),
                          flush=True)
    finally:
        for home in homes.values():
            shutil.rmtree(home, ignore_errors=True)
    (out / "results.json").write_text(json.dumps(rows, indent=2, ensure_ascii=False), encoding="utf-8")
    summary = compare(rows)
    (out / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
    print(json.dumps(summary), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
