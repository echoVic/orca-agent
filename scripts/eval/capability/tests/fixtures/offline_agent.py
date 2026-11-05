import json
import os
from pathlib import Path
import sys
if "trust" in sys.argv: raise SystemExit(0)
def value(flag):
    return sys.argv[sys.argv.index(flag)+1] if flag in sys.argv else None
Path("../request-seen.json").write_text(json.dumps({"model": value("--model"), "max_turns": value("--max-turns"), "reasoning_effort": os.environ.get("ORCA_REASONING_EFFORT")}))
# TAMPER
print(json.dumps({"type":"usage.updated", "payload":{"input_tokens":101,"output_tokens":17,"estimated_cost_usd":0.002}}))
print(json.dumps({"type":"session.completed", "payload":{"status":"completed"}}))
