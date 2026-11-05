"""Run real Python functions and scripts using offline provider records."""
import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
HERE = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HERE))
import run
from lib import backend, metrics, report, scenario

class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
    def args(self, **kw):
        values = dict(provider="mock", model="vendor/requested", reasoning_effort="low",
            endpoint="https://alice:password@api.example.test:8443/secret-key/v1?token=hidden#secret",
            max_turns=7, max_tool_calls=11, max_cost_usd=0.75, max_wall_time_secs=30,
            profile="smoke", k=3, out=str(self.root / "runs"), max_usd=5,
            against=None, record=False, split="tuning", suite_id="offline-v1")
        values.update(kw)
        return argparse.Namespace(**values)
    def load_item(self, **kw):
        root = self.root / "case"
        (root / "fixture").mkdir(parents=True, exist_ok=True)
        (root / "fixture" / "verify.py").write_text("raise SystemExit(0)\n")
        data = dict(schema_version=1, id="test", axis="S", rung=1, difficulty={"level": 1},
            backend="local", fixture="fixture", verify=["python3 verify.py"],
            prompt="do task", prompt_is_text=True, provider="mock")
        data.update(kw)
        (root / "scenario.json").write_text(json.dumps(data))
        return scenario.load(root / "scenario.json")
    def test_actual_request_and_safe_endpoint_unknown_response(self):
        binary = self.root / "orca"; binary.write_bytes(b"offline binary")
        result = run.protocol(self.args(), binary)
        self.assertEqual(result.get("requested_model"), "vendor/requested")
        self.assertEqual(result.get("reasoning_effort"), "low")
        self.assertEqual(result.get("endpoint_identity"), "https://api.example.test:8443")
        self.assertEqual(result.get("response_model"), "unknown")
        self.assertEqual(result.get("provider_fingerprint"), "unknown")
        self.assertEqual(result["binary_sha256"], hashlib.sha256(b"offline binary").hexdigest())
        self.assertEqual(result.get("sampling"), {"temperature": "unknown", "top_p": "unknown", "seed": "unknown"})
        self.assertEqual(result.get("budget", {}).get("max_turns"), 7)
        self.assertNotIn("secret-key", json.dumps(result))
    def test_unknown_config_and_missing_binary(self):
        result = run.protocol(self.args(model=None, reasoning_effort=None, endpoint=None), self.root / "missing")
        self.assertEqual(result.get("requested_model"), "unknown")
        self.assertEqual(result.get("reasoning_effort"), "unknown")
        self.assertEqual(result.get("binary_sha256"), "unknown")
    def test_execution_and_fixed_split_loaded(self):
        item = self.load_item(execution={"model": "vendor/scenario", "reasoning_effort": "high"},
            evaluation={"split": "holdout", "suite_id": "fixed-v1"})
        self.assertEqual(getattr(item, "execution", {}), {"model": "vendor/scenario", "reasoning_effort": "high"})
        self.assertEqual(getattr(item, "evaluation", {}), {"split": "holdout", "suite_id": "fixed-v1"})
    def test_invalid_split_rejected(self):
        with self.assertRaises(scenario.ScenarioError): self.load_item(evaluation={"split": "training"})
    def test_first_successful_discovery_from_real_event_shape(self):
        records = [{"type": "usage.updated", "payload": {"input_tokens": 100, "output_tokens": 12}}]
        for cid, status in [("a", "failed"), ("b", "completed"), ("c", "completed")]:
            records.extend([
                {"type": "tool.call.requested", "payload": {"id": cid, "name": "read_file", "target": "docs/POLICY.md"}},
                {"type": "tool.call.completed", "payload": {"id": cid, "status": status, "output": "Use half-to-even"}}])
        records.append({"type": "session.completed", "payload": {"status": "completed"}})
        path = self.root / "trace.jsonl"; path.write_text("\n".join(json.dumps(r) for r in records))
        result = backend.parse_events(path, constraints=[{"id": "rounding", "source": "docs/POLICY.md", "contains": "half-to-even"}])
        first = result.get("discoveries", [{}])[0]
        self.assertEqual(first.get("tool_call_id"), "b")
        self.assertEqual(first.get("tool_calls_to_discovery"), 2)
        self.assertEqual(first.get("tokens_in_to_discovery"), 100)
        self.assertEqual(first.get("source"), "docs/POLICY.md")
    def test_missing_trace_is_unknown(self):
        result = backend.parse_events(self.root / "absent")
        self.assertEqual(result.get("discoveries"), [])
        self.assertIsNone(result["tokens_in"])
    def test_provider_records_and_real_schema(self):
        from lib import evidence
        result = evidence.provider_metadata(HERE / "tests/fixtures/provider.jsonl")
        self.assertEqual(result["requested_models"], ["vendor/requested"])
        self.assertEqual(result["response_models"], ["vendor/served"])
        self.assertEqual(result["provider_fingerprints"], ["fp-offline"])
        self.assertEqual(result["sampling"]["seed"], 17)
        self.assertRegex(result["tool_schema_sha256"], r"^[0-9a-f]{64}$")
        self.assertNotIn("credential", json.dumps(result))
    def test_hashes_follow_fixture_and_verifier_bytes(self):
        from lib import evidence
        item = self.load_item(); first = evidence.scenario_identity(item, item.fixture)
        self.assertRegex(first["fixture_sha256"], r"^[0-9a-f]{64}$")
        self.assertRegex(first["verifier_sha256"], r"^[0-9a-f]{64}$")
        (item.fixture / "verify.py").write_text("raise SystemExit(1)\n")
        after = evidence.scenario_identity(item, item.fixture)
        self.assertNotEqual(after["fixture_sha256"], first["fixture_sha256"])
        self.assertNotEqual(after["verifier_sha256"], first["verifier_sha256"])
    def test_cost_includes_failed_attempts(self):
        result = metrics.cost_per_pass([
            {"outcome": "pass", "tokens_in": 100, "tokens_out": 20, "cost_usd": 0.1, "wall_seconds": 2},
            {"outcome": "fail", "tokens_in": 200, "tokens_out": 30, "cost_usd": 0.4, "wall_seconds": 5}])
        self.assertEqual(result.get("cost_usd_per_pass"), 0.5)
        self.assertEqual(result.get("tokens_in_per_pass"), 300)
        self.assertEqual(result["tokens_out_per_pass"], 50)
        self.assertEqual(result["wall_seconds_per_pass"], 7)
    def test_missing_failed_usage_is_unknown(self):
        result = metrics.cost_per_pass([{"outcome": "pass", "cost_usd": 0.1, "tokens_out": 20}, {"outcome": "fail"}])
        self.assertEqual(result.get("cost_usd_per_pass"), "unknown")
        self.assertEqual(result.get("tokens_out_per_pass"), "unknown")
        self.assertEqual(result.get("observed_cost_usd"), 0.1)
        self.assertEqual(result.get("cost_observed_attempts"), 1)
    def test_repeats_keep_holdout_tuning_and_config_separate(self):
        def sample(split, model, outcomes):
            return {"run_id": f"{split}-{model}", "protocol": {"requested_model": model},
                "evaluation": {"split": split, "suite_id": "fixed-v1", "manifest_sha256": "same"},
                "scenarios": [{"id": "s", "axis": "S", "rung": 1,
                    "attempts": [{"outcome": o, "cost_usd": 0.2} for o in outcomes]}]}
        result = report.aggregate_runs([sample("holdout", "a", ["pass", "fail"]),
            sample("tuning", "a", ["pass"]), sample("holdout", "b", ["fail"])])
        self.assertEqual(len(result["groups"]), 3)
        entry = next(g for g in result["groups"] if g["split"] == "holdout" and g["requested_model"] == "a")
        self.assertEqual(entry["pass_rate"], 0.5)
        self.assertEqual(entry["outcome_variance"], 0.5)
        self.assertEqual(entry["cost"]["cost_usd_per_pass"], 0.4)
        self.assertEqual(entry["wilson"], [0.0945, 0.9055])
    def test_real_mock_ports_repeat_two_pool_with_provenance(self):
        import socket
        from lib import evidence
        binary = self.root / "offline-orca"
        source = (HERE / "tests/fixtures/offline_agent.py").read_text()
        request = """import urllib.request
request = {"model": "vendor/requested", "messages": [{"role": "user", "content": "clean"}], "stream": True}
with urllib.request.urlopen(urllib.request.Request(os.environ["ORCA_BASE_URL"] + "/chat/completions",
    data=json.dumps(request).encode(), headers={"Content-Type": "application/json"}), timeout=5) as response:
    response.read()
"""
        source = source.replace("def value(flag):", request + "def value(flag):")
        binary.write_text("#!/usr/bin/env python3\n" + source); binary.chmod(0o755)
        item = self.load_item(evaluation={"split": "holdout", "suite_id": "offline-v1"})
        # Reserve two distinct OS-assigned ports; only allocation is patched,
        # both provider subprocesses and HTTP requests are real and offline.
        with closing(socket.socket()) as first, closing(socket.socket()) as second:
            first.bind(("127.0.0.1", 0)); second.bind(("127.0.0.1", 0))
            ports = [first.getsockname()[1], second.getsockname()[1]]
        with patch.object(backend, "_free_port", side_effect=ports):
            result = run.run_scenarios(self.args(k=2, split="holdout"), [item], binary)
        attempts = result["scenarios"][0]["attempts"]
        origins = {a["protocol"]["endpoint_identity"] for a in attempts}
        self.assertEqual(origins, {f"http://127.0.0.1:{port}" for port in ports})
        self.assertEqual(len(result["aggregation"]["groups"]), 1)
        group = result["aggregation"]["groups"][0]
        self.assertEqual((group["attempts"], group["passes"]), (2, 2))
        self.assertEqual(set(group["endpoint_identities"]), origins)
        fingerprints = {a["protocol"]["provider_fixture_sha256"] for a in attempts}
        self.assertEqual(len(fingerprints), 1)
        self.assertRegex(next(iter(fingerprints)), r"^[0-9a-f]{64}$")
        for index, a in enumerate(attempts, 1):
            saved = json.loads((Path(result["out"]) / "per-scenario/test" / f"attempt-{index}" / "attempt.json").read_text())
            self.assertEqual(saved["protocol"], a["protocol"])
            self.assertEqual(a["protocol"]["endpoint_comparison_identity"], "controlled-mock:" + next(iter(fingerprints)))
        self.assertNotEqual(evidence.file_sha(backend.MOCK_PROVIDER), "unknown")

    def test_comparison_checks_attempt_scenario_identity_under_same_manifest(self):
        from copy import deepcopy
        from lib import evidence
        item = self.load_item()
        identity = evidence.scenario_identity(item)
        baseline = {"run_id": "one", "evaluation": {"manifest_sha256": "fixed"},
            "scenarios": [{"id": "test", "attempts": [{"outcome": "pass", "protocol": identity}]}]}
        for key in ("scenario_sha256", "prompt_sha256", "prepare_sha256", "verifier_command_sha256", "source_fixture_sha256"):
            with self.subTest(key=key):
                changed = deepcopy(baseline); changed["run_id"] = "two"
                changed["scenarios"][0]["attempts"][0]["protocol"][key] = evidence.digest([key, "changed"])
                self.assertEqual(len(report.aggregate_runs([baseline, changed])["groups"]), 2)

    def test_real_provider_endpoints_never_use_mock_comparison_identity(self):
        from lib import evidence
        fingerprint = evidence.digest("controlled fixture")
        attempts = [{"outcome": "pass", "protocol": {"provider": "deepseek", "backend": "local",
            "endpoint_identity": endpoint, "provider_fixture_sha256": fingerprint,
            "endpoint_comparison_identity": "controlled-mock:" + fingerprint}}
            for endpoint in ("https://one.example.test", "https://two.example.test")]
        result = report.aggregate_runs([{"run_id": "one", "evaluation": {"manifest_sha256": "fixed"},
            "scenarios": [{"id": "test", "attempts": attempts}]}])
        self.assertEqual(len(result["groups"]), 2)
        self.assertEqual({g["configuration"]["endpoint_comparison_identity"] for g in result["groups"]},
            {"https://one.example.test", "https://two.example.test"})

    def test_mock_fixture_changes_and_unknown_fingerprint_do_not_pool(self):
        from lib import evidence
        def attempt(port, fingerprint):
            return {"outcome": "pass", "protocol": {"provider": "mock", "backend": "local",
                "endpoint_identity": f"http://127.0.0.1:{port}", "provider_fixture_sha256": fingerprint,
                "endpoint_comparison_identity": "controlled-mock:" + fingerprint}}
        attempts = [attempt(55101, evidence.digest("v1")), attempt(55112, evidence.digest("v2")),
                    attempt(55122, "unknown"), attempt(55127, "unknown")]
        result = report.aggregate_runs([{"run_id": "one", "evaluation": {"manifest_sha256": "fixed"},
            "scenarios": [{"id": "test", "attempts": attempts}]}])
        self.assertEqual(len(result["groups"]), 4)

    def test_dry_run_without_binary_auth_or_provider(self):
        out = self.root / "dry"
        proc = subprocess.run([sys.executable, "-B", str(HERE / "run.py"), "--dry-run", "--scenarios",
            "C-multi-constraint-full,C-multi-constraint-compacted,G-1-external-state",
            "--binary", str(self.root / "missing"), "--out", str(out)], capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        plan = json.loads(next(out.glob("*/plan.json")).read_text())
        self.assertEqual(plan["execution"], "dry-run")
        self.assertEqual(len(plan["scenarios"]), 3)
        self.assertEqual(plan["protocol"]["binary_sha256"], "unknown")
    def test_backend_applies_config_and_protects_verifier(self):
        binary = self.root / "offline-orca"
        binary.write_text("#!/usr/bin/env python3\n" + (HERE / "tests/fixtures/offline_agent.py").read_text()); binary.chmod(0o755)
        item = self.load_item(execution={"model": "vendor/scenario", "reasoning_effort": "high", "max_turns": 7})
        with patch.object(backend, "MockProvider") as endpoint:
            endpoint.return_value.port = 12345
            attempt = backend.run_local(item, self.root / "attempt", binary=binary)
        self.assertEqual(attempt.outcome, "pass")
        saved = json.loads((self.root / "attempt/request-seen.json").read_text())
        self.assertEqual(saved["model"], "vendor/scenario")
        self.assertEqual(saved["reasoning_effort"], "high")
        self.assertEqual(saved["max_turns"], "7")
        self.assertEqual(attempt.as_dict().get("protocol", {}).get("requested_model"), "vendor/scenario")
        binary.write_text(binary.read_text().replace("# TAMPER", "Path('verify.py').write_text('raise SystemExit(9)\\n')"))
        with patch.object(backend, "MockProvider") as endpoint:
            endpoint.return_value.port = 12345
            tampered = backend.run_local(item, self.root / "tamper", binary=binary)
        self.assertEqual(tampered.failure_class, "harness")
    def test_external_state_verifier_rejects_completion_claim(self):
        fixture = HERE / "scenarios/G-1-external-state/fixture"
        self.assertTrue(fixture.is_dir(), "external-state fixture missing")
        workspace = self.root / "external"; shutil.copytree(fixture, workspace)
        env = dict(os.environ, CAPABILITY_EXTERNAL_DB=str(self.root / "app.db"))
        def command(script):
            return subprocess.run([sys.executable, str(workspace / script)], cwd=workspace, env=env, capture_output=True, text=True)
        self.assertEqual(command("prepare.py").returncode, 0)
        (workspace / "done.txt").write_text("completed")
        self.assertNotEqual(command("verify.py").returncode, 0)
        self.assertEqual(command("app.py").returncode, 0)
        self.assertEqual(command("verify.py").returncode, 0)
        with closing(sqlite3.connect(env["CAPABILITY_EXTERNAL_DB"])) as db:
            db.execute("UPDATE tickets SET owner='wrong' WHERE id=41")
            db.commit()
        self.assertNotEqual(command("verify.py").returncode, 0)
    def test_compression_checks_each_constraint(self):
        fixture = HERE / "scenarios/C-multi-constraint-full/fixture"
        self.assertTrue(fixture.is_dir(), "multi-constraint fixture missing")
        workspace = self.root / "compression"; shutil.copytree(fixture, workspace)
        answer = {"path": "exports/weekly.csv", "limit": 137, "forbidden": ["network", "delete archives"],
            "unresolved": "owner approval pending", "support": "trial reduced latency by 8 ms",
            "opposition": "trial lost 2 records", "decision": "hold"}
        path = workspace / "answer.json"
        def verify():
            return subprocess.run([sys.executable, str(HERE / "scenarios/C-multi-constraint-full/verifier/verify.py")], cwd=workspace, capture_output=True, text=True).returncode
        path.write_text(json.dumps(answer)); self.assertEqual(verify(), 0)
        for key in answer:
            with self.subTest(key=key):
                bad = dict(answer); bad.pop(key); path.write_text(json.dumps(bad)); self.assertNotEqual(verify(), 0)
    def test_writes_and_unpaired_results_are_not_constraint_discovery(self):
        path = self.root / "misleading.jsonl"
        path.write_text("\n".join(json.dumps(r) for r in [
            {"type":"tool.call.requested", "payload":{"id":"w", "name":"write_file", "target":"docs/POLICY.md"}},
            {"type":"tool.call.completed", "payload":{"id":"w", "status":"completed", "output":"half-to-even"}},
            {"type":"tool.call.completed", "payload":{"id":"unpaired", "status":"completed", "output":"docs/POLICY.md half-to-even"}},
        ]))
        result = backend.parse_events(path, [{"id":"c", "source":"docs/POLICY.md", "contains":"half-to-even"}])
        self.assertEqual(result["discoveries"], [])

    def test_read_source_must_match_path_not_substring(self):
        path = self.root / "decoy.jsonl"
        path.write_text("\n".join(json.dumps(r) for r in [
            {"type":"tool.call.requested", "payload":{"id":"r", "name":"read_file", "target":"docs/POLICY.md.decoy"}},
            {"type":"tool.call.completed", "payload":{"id":"r", "status":"completed", "output":"half-to-even"}},
        ]))
        result = backend.parse_events(path, [{"id":"c", "source":"docs/POLICY.md", "contains":"half-to-even"}])
        self.assertEqual(result["discoveries"], [])

    def test_final_result_independent_of_discovery_and_failure_usage_preserved(self):
        binary = self.root / "offline-orca"
        source = "#!/usr/bin/env python3\n" + (HERE / "tests/fixtures/offline_agent.py").read_text()
        binary.write_text(source); binary.chmod(0o755)
        item = self.load_item(evidence=[{"id":"c", "source":"docs/POLICY.md", "contains":"half-to-even"}])
        with patch.object(backend, "MockProvider") as endpoint:
            endpoint.return_value.port = 12345
            good = backend.run_local(item, self.root / "good", binary=binary)
        self.assertEqual(good.outcome, "pass")
        self.assertEqual(good.discoveries, [])
        (item.fixture / "verify.py").write_text("raise SystemExit(1)\n")
        trace = [
            {"type":"tool.call.requested", "payload":{"id":"r", "name":"read_file", "target":"docs/POLICY.md"}},
            {"type":"tool.call.completed", "payload":{"id":"r", "status":"completed", "output":"half-to-even"}},
        ]
        binary.write_text(source + "\n" + "\n".join("print(" + repr(json.dumps(r)) + ")" for r in trace))
        with patch.object(backend, "MockProvider") as endpoint:
            endpoint.return_value.port = 12345
            bad = backend.run_local(item, self.root / "bad", binary=binary)
        self.assertEqual(bad.outcome, "fail")
        self.assertEqual(bad.final_verification, "failed")
        self.assertEqual(len(bad.discoveries), 1)
        self.assertTrue(bad.false_completion)
        self.assertEqual(bad.tokens_in, 101)
        self.assertEqual(bad.cost_usd, 0.002)

    def test_terminal_cost_in_nested_usage_is_observed(self):
        path = self.root / "cost.jsonl"
        path.write_text(json.dumps({"type":"session.completed", "payload":{"status":"failed", "usage":
            {"input_tokens":42, "output_tokens":5, "estimated_cost_usd":0.03}}}))
        observed = backend.parse_events(path)
        self.assertEqual(observed["cost_usd"], 0.03)
        self.assertEqual(observed["terminal"], "failed")

    def test_invalid_execution_types_rejected_before_binary_runs(self):
        for config in ({"model":42}, {"max_turns":-1}, {"reasoning_effort":"medium"}):
            with self.subTest(config=config), self.assertRaises(scenario.ScenarioError):
                self.load_item(execution=config)

    def test_holdout_cannot_be_relabelled_tuning(self):
        from lib import evidence
        item = self.load_item(evaluation={"split":"holdout", "suite_id":"fixed-v1"})
        with self.assertRaises(ValueError): evidence.suite_metadata([item], self.args(split="tuning"))

    def test_missing_provider_metadata_and_router_are_unknown(self):
        from lib import evidence
        path = self.root / "request.jsonl"
        path.write_text('not json\n' + json.dumps({"model":"requested", "authorization":"secret"}))
        observed = evidence.attach_provider(evidence.execution_protocol({}), path)
        for key in ("response_model", "provider_fingerprint", "engine_fingerprint", "tool_schema_sha256"):
            self.assertEqual(observed[key], "unknown")
        self.assertEqual(observed["sampling"]["seed"], "unknown")
        self.assertNotIn("authorization", json.dumps(observed))

    def test_actual_observed_requests_and_mixed_splits_separate(self):
        base = {"protocol":{}, "evaluation":{"split":"mixed", "suite_id":"mixed", "manifest_sha256":"fixed",
            "scenarios":[{"id":"s", "split":"holdout", "suite_id":"h-v1"}]}}
        runs = [{**base, "run_id":str(i), "scenarios":[{"id":"s", "attempts":[{"outcome":"pass",
            "protocol":{"observed_requested_model":model}}]}]} for i, model in enumerate(["a","b"])]
        groups = report.aggregate_runs(runs)["groups"]
        self.assertEqual(len(groups), 2)
        self.assertTrue(all(g["split"] == "holdout" and g["suite_id"] == "h-v1" for g in groups))

    def test_verifier_snapshot_taken_before_agent_mutates_source(self):
        binary = self.root / "offline-orca"
        item = self.load_item()
        # Agent can address source outside its workspace. The snapshot must stay independent.
        extra = "Path(" + repr(str(item.fixture / "verify.py")) + ").write_text('raise SystemExit(8)\\n')"
        binary.write_text("#!/usr/bin/env python3\n" + (HERE / "tests/fixtures/offline_agent.py").read_text().replace("# TAMPER", extra))
        binary.chmod(0o755)
        with patch.object(backend, "MockProvider") as endpoint:
            endpoint.return_value.port = 12345
            attempt = backend.run_local(item, self.root / "snapshot", binary=binary)
        result = json.loads((self.root / "snapshot/attempt.json").read_text())
        self.assertEqual(result["verifier"][0]["exit"], 0)
        self.assertEqual(attempt.failure_class, "harness")

    def test_goal_overrides_rejected_instead_of_reported_unapplied(self):
        item = self.load_item(command="goal", execution={"model":"unapplied"})
        with self.assertRaises(backend.BackendError):
            backend.run_local(item, self.root / "goal", binary=self.root / "missing")

    def test_failed_tool_exit_and_malformed_usage_not_discoveries(self):
        path = self.root / "failed.jsonl"
        path.write_text("\n".join(json.dumps(r) for r in [[],
            {"type":"usage.updated", "payload":{"usage":"bad"}},
            {"type":"tool.call.requested", "payload":{"id":"r", "name":"read_file", "target":"docs/POLICY.md"}},
            {"type":"tool.call.completed", "payload":{"id":"r", "status":"completed", "exit_code":1, "output":"half-to-even"}},
        ]) + "\n{truncated")
        self.assertEqual(backend.parse_events(path, [{"id":"c", "source":"docs/POLICY.md", "contains":"half-to-even"}])["discoveries"], [])

    def test_prepare_both_context_variants_and_independent_verification(self):
        for variant in ("full", "compacted"):
            item = scenario.load(HERE / f"scenarios/C-multi-constraint-{variant}/scenario.json")
            workspace = self.root / variant; shutil.copytree(item.fixture, workspace)
            prepare = subprocess.run([sys.executable, "-B", str(item.prepare)], cwd=workspace,
                env={**os.environ, **item.prepare_env}, capture_output=True, text=True)
            self.assertEqual(prepare.returncode, 0, prepare.stderr)
            context = (workspace / "docs/CONTEXT.md").read_text()
            for marker in ("exports/weekly.csv", "137", "network", "delete archives", "owner approval pending",
                           "reduced latency by 8 ms", "lost 2 records", "hold"):
                self.assertIn(marker, context)
            (workspace / "answer.json").write_text(json.dumps({"path":"exports/weekly.csv", "limit":137,
                "forbidden":["network", "delete archives"], "unresolved":"owner approval pending",
                "support":"trial reduced latency by 8 ms", "opposition":"trial lost 2 records", "decision":"hold"}))
            verify = subprocess.run([sys.executable, "-B", str(item.verifier_root / "verify.py")], cwd=workspace, capture_output=True)
            self.assertEqual(verify.returncode, 0)
        self.assertGreater(len((self.root / "full/docs/CONTEXT.md").read_text()),
                           100 * len((self.root / "compacted/docs/CONTEXT.md").read_text()))

    def test_repeated_runner_and_aggregation_cli_offline(self):
        item = self.load_item()
        binary = self.root / "offline-orca"
        binary.write_text("#!/usr/bin/env python3\n" + (HERE / "tests/fixtures/offline_agent.py").read_text()); binary.chmod(0o755)
        with patch.object(backend, "MockProvider") as endpoint:
            endpoint.return_value.port = 12345
            first = run.run_scenarios(self.args(), [item], binary)
            (item.fixture / "verify.py").write_text("raise SystemExit(1)\n")
            second = run.run_scenarios(self.args(), [item], binary)
        self.assertEqual(first["cost"]["cost_usd_per_pass"], 0.002)
        self.assertEqual(second["cost"]["observed_cost_usd"], 0.006)
        self.assertEqual(second["cost"]["cost_usd_per_pass"], "unknown")
        out = self.root / "aggregate"
        proc = subprocess.run([sys.executable, "-B", str(HERE / "run.py"), "--aggregate", str(self.root / "runs"),
            "--out", str(out)], capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        data = json.loads((out / "aggregate.json").read_text())
        self.assertEqual(len(data["groups"]), 2, "changed verifier manifests must not pool")
        self.assertEqual(sum(g["attempts"] for g in data["groups"]), 6)

    def test_docker_config_does_not_overwrite_auth_key_offline(self):
        # Capture Docker argv only; no Docker process, provider connection or Cargo execution.
        item = self.load_item(provider="deepseek", image="offline", verify=["true"], execution={"model":"served"})
        musl = self.root / "target/x86_64-unknown-linux-musl/release/orca"
        musl.parent.mkdir(parents=True); musl.write_bytes(b"fixture binary")
        auth = self.root / ".orca/auth.json"; auth.parent.mkdir(); auth.write_text(json.dumps({"DEEPSEEK_API_KEY":"offline-only"}))
        calls = []
        def capture(argv, **kwargs):
            calls.append(argv)
            if kwargs.get("stdout") is not None:
                kwargs["stdout"].write((HERE / "tests/fixtures/observations.jsonl").read_text())
            return subprocess.CompletedProcess(argv, 0, stdout="", stderr="")
        with patch.object(backend, "REPO", self.root), patch.object(backend.Path, "home", return_value=self.root), \
             patch.object(backend.subprocess, "run", side_effect=capture):
            result = backend.run_docker(item, self.root / "docker")
        self.assertIn("DEEPSEEK_API_KEY=offline-only", calls[0])
        self.assertIn("ORCA_MODEL=served", calls[0])
        self.assertEqual(result.protocol["requested_model"], "served")
        self.assertEqual(len(result.as_dict().get("provider_observations", [])), 3)
        self.assertEqual(len(result.as_dict().get("compactions", [])), 3)
        saved = json.loads((self.root / "docker/attempt.json").read_text())
        self.assertEqual(saved["provider_observations"], result.provider_observations)
        self.assertEqual(saved["compactions"], result.compactions)
        self.assertEqual(saved["protocol"], result.protocol)
        self.assertEqual(saved["final_verification"], result.final_verification)


    def test_completed_compactions_retain_measurement_and_unknowns(self):
        events = backend.parse_events(HERE / "tests/fixtures/observations.jsonl")
        completed = [row for row in events["compactions"] if row["type"] == "context.compacted"]
        self.assertEqual(len(completed), 2)
        self.assertEqual(completed[0]["event_line"], 2)
        self.assertEqual(completed[0]["strategy"], "deep")
        observation = completed[0]["observation"]
        self.assertEqual(observation["before_tokens_est"], 12000)
        self.assertEqual(observation["after_tokens_est"], 3000)
        self.assertEqual(observation["elapsed_ms"], 50)
        self.assertEqual(observation["estimated_cost_usd"], 0.004)
        self.assertEqual(observation["summary_requests"][0]["usage"]["cache_tokens"], 500)
        self.assertEqual(observation["prefix_reuse_estimate"]["unchanged_prefix_tokens_est"], 512)
        self.assertEqual(completed[1]["observation"]["elapsed_ms"], "unknown")
        self.assertEqual(completed[1]["observation"]["summary_usage"], "unknown")

    def test_provider_completion_names_and_missing_latency(self):
        events = backend.parse_events(HERE / "tests/fixtures/observations.jsonl")
        rows = events.get("provider_observations", [])
        self.assertEqual(len(rows), 3)
        self.assertEqual(rows[0]["type"], "provider.response.completed")
        self.assertEqual(rows[0]["observation"], {"elapsed_ms": 200, "first_delta_ms": 40, "first_delta_kind": "reasoning"})
        self.assertEqual(rows[1]["type"], "model.response.completed")
        self.assertEqual(rows[1]["observation"], {"elapsed_ms": 400, "first_delta_ms": "unknown", "first_delta_kind": "unknown"})
        self.assertEqual(rows[2]["observation"], {"elapsed_ms": "unknown", "first_delta_ms": "unknown", "first_delta_kind": "unknown"})
        self.assertEqual(events["cost_usd"], 0.01, "per-response/summary observations must not double count cumulative cost")
        missing = backend.parse_events(self.root / "absent.jsonl")
        self.assertEqual(missing.get("provider_observations"), [])

    def test_observation_zero_is_known_invalid_latency_is_unknown(self):
        path = self.root / "invalid-observations.jsonl"
        path.write_text("\n".join(json.dumps(r) for r in [
            {"type": "provider.response.completed", "payload": {"observation": {"elapsed_ms": 0, "first_delta_ms": 0, "first_delta_kind": "message"}}},
            {"type": "provider.response.completed", "payload": {"observation": {"elapsed_ms": -1, "first_delta_ms": True}}},
            {"type": "provider.response.completed", "payload": {"observation": ["bad"]}},
            {"type": "context.compacted", "payload": {"observation": {"summary_usage": None, "summary_usage_complete": False, "summary_requests": [{"usage": None}], "estimated_cost_usd": 0}}},
        ]))
        events = backend.parse_events(path)
        providers = events.get("provider_observations", [])
        self.assertEqual(len(providers), 3)
        self.assertEqual(providers[0]["observation"]["first_delta_ms"], 0)
        self.assertEqual(providers[1]["observation"]["elapsed_ms"], "unknown")
        self.assertEqual(providers[1]["observation"]["first_delta_ms"], "unknown")
        self.assertEqual(providers[2]["observation"]["elapsed_ms"], "unknown")
        observation = events["compactions"][0]["observation"]
        self.assertEqual(observation["summary_usage"], "unknown")
        self.assertEqual(observation["summary_requests"][0]["usage"], "unknown")
        self.assertIs(observation["summary_usage_complete"], False)
        self.assertEqual(observation["estimated_cost_usd"], 0)

    def test_runner_preserves_observations_on_pass_and_failed_verifier(self):
        binary = self.root / "offline-observation-orca"
        source = "#!/usr/bin/env python3\n" + (HERE / "tests/fixtures/offline_agent.py").read_text()
        trace = (HERE / "tests/fixtures/observations.jsonl").read_text().splitlines()
        binary.write_text(source + "\n" + "\n".join("print(" + repr(line) + ")" for line in trace))
        binary.chmod(0o755)
        item = self.load_item()
        for should_pass in (True, False):
            (item.fixture / "verify.py").write_text("raise SystemExit(" + ("0" if should_pass else "1") + ")\n")
            out = self.root / ("pass" if should_pass else "fail")
            with patch.object(backend, "MockProvider") as endpoint:
                endpoint.return_value.port = 12345
                data = run.run_scenarios(self.args(k=1, out=str(out)), [item], binary)
            row = data["scenarios"][0]["attempts"][0]
            self.assertEqual(row.get("outcome"), "pass" if should_pass else "fail")
            self.assertEqual(len(row.get("compactions", [])), 3)
            self.assertEqual(len(row.get("provider_observations", [])), 3)
            saved = json.loads(next(out.glob("**/attempt.json")).read_text())
            self.assertEqual(saved["compactions"], row["compactions"])
            self.assertEqual(saved["provider_observations"], row["provider_observations"])
            disk = json.loads(next(out.glob("*/run.json")).read_text())
            self.assertEqual(disk["scenarios"][0]["attempts"][0]["provider_observations"], row["provider_observations"])
            self.assertEqual(row["cost_usd"], 0.01)

    def test_observation_statistics_include_failures_and_exclude_started_events(self):
        rows = [
            {"outcome": "pass", "compactions": [{"type": "context.compaction.started"}, {"type": "context.compacted", "observation": {"before_tokens_est": 12000, "after_tokens_est": 3000, "elapsed_ms": 50, "estimated_cost_usd": 0.004}}], "provider_observations": [{"observation": {"elapsed_ms": 200, "first_delta_ms": 40, "first_delta_kind": "reasoning"}}]},
            {"outcome": "fail", "compactions": [{"type": "context.compacted", "observation": {"before_tokens_est": 8000, "after_tokens_est": 1000, "elapsed_ms": 150, "estimated_cost_usd": 0.006}}], "provider_observations": [{"observation": {"elapsed_ms": 400, "first_delta_ms": 80, "first_delta_kind": "message"}}]},
        ]
        data = {"run_id": "fixture", "protocol": {}, "evaluation": {"manifest_sha256": "fixed"}, "scenarios": [{"id": "s", "attempts": rows}]}
        group = report.aggregate_runs([data])["groups"][0]
        observed = group.get("observations", {})
        self.assertEqual(observed.get("compaction", {}).get("completed_events"), 2)
        self.assertEqual(observed["compaction"]["before_tokens_est"]["mean"], 10000)
        self.assertEqual(observed["compaction"]["after_tokens_est"]["mean"], 2000)
        self.assertEqual(observed["compaction"]["elapsed_ms"]["mean"], 100)
        self.assertAlmostEqual(observed["compaction"]["estimated_cost_usd"]["total"], 0.01)
        self.assertEqual(observed["provider"]["elapsed_ms"]["mean"], 300)
        self.assertEqual(observed["provider"]["first_delta_ms"]["mean"], 60)
        self.assertEqual(observed["provider"]["first_delta_kind_counts"], {"reasoning": 1, "message": 1})
        rows[1]["provider_observations"][0]["observation"]["first_delta_ms"] = "unknown"
        partial = report.aggregate_runs([data])["groups"][0]["observations"]["provider"]["first_delta_ms"]
        self.assertEqual(partial["mean"], "unknown")
        self.assertEqual(partial["observed_mean"], 40)
        self.assertEqual(partial["observed_events"], 1)
        old = report.aggregate_runs([{**data, "scenarios": [{"id": "s", "attempts": [{"outcome": "pass"}]}]}])["groups"][0]["observations"]
        self.assertEqual(old["provider"]["elapsed_ms"]["mean"], "unknown")
        self.assertEqual(old["compaction"]["before_tokens_est"]["mean"], "unknown")

    def test_report_displays_observation_values_and_measurement_basis(self):
        events = backend.parse_events(HERE / "tests/fixtures/observations.jsonl")
        attempts = [{"outcome": "pass", "compactions": events["compactions"], "provider_observations": events.get("provider_observations", [])}]
        meta = {"id": "s", "axis": "C", "rung": 1, "difficulty": {}, "difficulty_value": 1}
        data = {"run_id": "fixture", "profile": "smoke", "started_at": "offline", "k": 1, "out": "offline", "protocol": {}, "summary": report.summarize([(meta, attempts)]), "scenarios": [{"id": "s", "attempts": attempts}]}
        data["aggregation"] = report.aggregate_runs([data])
        text = report.render_markdown(data)
        self.assertIn("before_tokens_est", text)
        self.assertIn("first_delta_kind", text)
        self.assertIn("12000", text)
        self.assertIn("observed_mean", text)
        self.assertIn("local token estimates", text)
        self.assertIn("first nonempty streamed delta", text)
        self.assertIn("not service KV cache proof", text)
        self.assertIn("not added to cumulative attempt cost", text)
        self.assertIn("unknown", text)

if __name__ == "__main__": unittest.main()
