import json, os, pathlib, subprocess, sys

HOME = os.environ["CAPABILITY_HOME"]
BIN = os.environ["CAPABILITY_ORCA_BIN"]
auth = pathlib.Path.home() / ".orca" / "auth.json"
key = json.loads(auth.read_text(encoding="utf-8"))["DEEPSEEK_API_KEY"]
env = {**os.environ, "ORCA_HOME": HOME, "DEEPSEEK_API_KEY": key}
result = subprocess.run([BIN, "doctor", "--online", "--json"], capture_output=True, text=True, timeout=90, env=env)
if result.returncode != 0:
    print("FAIL: doctor --online exited", result.returncode, result.stderr[-400:], result.stdout[-400:])
    sys.exit(1)
report = json.loads(result.stdout)
checks = report.get("checks") or []
online = [c for c in checks if c.get("id") == "credentials-online"]
if not online:
    print("FAIL: no credentials-online check")
    sys.exit(1)
check = online[0]
if check.get("status") not in ("pass", "warn"):
    print("FAIL: credentials-online status", check.get("status"))
    sys.exit(1)
if key[:8] in result.stdout or key in result.stdout:
    print("FAIL: doctor output contains the API key")
    sys.exit(1)
print("ok: doctor --online", check.get("status"))
