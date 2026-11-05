"""PR160 correctness regressions; actual scripts, no paid providers or Docker daemon."""
import argparse
import json
import os
from pathlib import Path
import shlex
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
from lib import backend, evidence, metrics, report, scenario, goal_episode

class ReviewTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def item(self, **changes):
        case = self.root / "case"
        fixture = case / "fixture"
        fixture.mkdir(parents=True, exist_ok=True)
        (fixture / "verify.py").write_text("raise SystemExit(0)\n")
        raw = dict(schema_version=1, id="review", axis="S", rung=1, difficulty={"level":1},
            backend="local", provider="mock", fixture="fixture", verify=["python3 -B verify.py"],
            prompt="task", prompt_is_text=True)
        raw.update(changes)
        (case / "scenario.json").write_text(json.dumps(raw))
        return scenario.load(case / "scenario.json")

    def binary(self, action):
        path = self.root / "offline-orca"
        path.write_text("#!/usr/bin/env python3\nimport sys, os, json\nfrom pathlib import Path\n"
            "if 'trust' in sys.argv: raise SystemExit(0)\n" + action + "\n"
            "print(json.dumps({'type':'session.completed','payload':{'status':'completed'}}))\n")
        path.chmod(0o755)
        return path

    def local(self, item, action, name="attempt"):
        with patch.object(backend, "MockProvider") as mock:
            mock.return_value.port = 12345
            return backend.run_local(item, self.root/name, self.binary(action))

    def test_answer_shortcut_cannot_read_oracle_from_agent_workspace(self):
        for variant in ("full", "compacted"):
            with self.subTest(variant=variant):
                item=scenario.load(HERE/f"scenarios/C-multi-constraint-{variant}/scenario.json")
                action = """import ast
for script in Path('.').rglob('*.py'):
    for node in ast.walk(ast.parse(script.read_text())):
        if isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id == 'EXPECTED' for t in node.targets):
            Path('answer.json').write_text(json.dumps(ast.literal_eval(node.value)))
"""
                result=self.local(item,action,variant)
                self.assertEqual(result.final_verification,"failed")
                self.assertNotEqual(result.outcome,"pass")
                self.assertFalse((self.root/variant/"workspace/verify.py").exists())

    def test_editable_diff_output_is_live_and_readonly_expected_is_snapshotted(self):
        item=self.item(verify=["diff result.txt expected.txt"], readonly_inputs=["expected.txt"])
        (item.fixture/"expected.txt").write_text("correct\n")
        for before, after, expected in (("wrong\n","correct\n","passed"),("correct\n","wrong\n","failed")):
            with self.subTest(before=before):
                (item.fixture/"result.txt").write_text(before)
                result=self.local(item,f"Path('result.txt').write_text({after!r})",expected)
                self.assertEqual(result.final_verification,expected)
                command=json.loads((self.root/expected/"attempt.json").read_text())["verifier"][0]["command"]
                self.assertIn("result.txt",shlex.split(command))
                self.assertNotIn("expected.txt",shlex.split(command))
                self.assertEqual((self.root/expected/"verifier/expected.txt").read_text(),"correct\n")

    def test_absolute_system_program_keeps_live_output_and_snapshotted_input(self):
        item = self.item(verify=["/usr/bin/diff result.txt expected.txt"], readonly_inputs=["expected.txt"])
        (item.fixture / "expected.txt").write_text("correct\n")
        result = self.local(item, "Path('result.txt').write_text('correct\\n')")
        self.assertEqual(result.final_verification, "passed")
        command = json.loads((self.root / "attempt/attempt.json").read_text())["verifier"][0]["command"]
        self.assertEqual(shlex.split(command)[0], "/usr/bin/diff")
        self.assertEqual((self.root / "attempt/verifier/expected.txt").read_text(), "correct\n")

    def test_absolute_interpreter_keeps_relative_script_snapshotted(self):
        item = self.item(verify=["/bin/sh verify.sh"])
        (item.fixture / "verify.sh").write_text('test "$(cat result.txt)" = correct\n')
        result = self.local(item, "Path('result.txt').write_text('correct')")
        self.assertEqual(result.final_verification, "passed")
        command = json.loads((self.root / "attempt/attempt.json").read_text())["verifier"][0]["command"]
        self.assertEqual(shlex.split(command)[0], "/bin/sh")
        self.assertEqual(Path(shlex.split(command)[1]), self.root / "attempt/verifier/verify.sh")

    def test_absolute_interpreter_script_is_left_as_external_command(self):
        script = self.root / "system-verifier.sh"
        script.write_text('test "$(cat result.txt)" = correct\n')
        command = "/bin/sh " + shlex.quote(str(script))
        item = self.item(verify=[command])
        self.assertEqual(evidence.verifier_bindings(item, item.fixture), [])
        result = self.local(item, "Path('result.txt').write_text('correct')")
        self.assertEqual(result.final_verification, "passed")
        saved = json.loads((self.root / "attempt/attempt.json").read_text())
        self.assertEqual(saved["verifier"][0]["command"], command)

    def test_executable_verifier_mode_is_preserved(self):
        item=self.item(verify=["scripts/verify.sh"])
        script=item.fixture/"scripts/verify.sh";script.parent.mkdir()
        script.write_text('#!/bin/sh\ntest "$(cat result.txt)" = correct\n');script.chmod(0o755)
        result=self.local(item,"Path('result.txt').write_text('correct')")
        self.assertEqual(result.final_verification,"passed")
        self.assertTrue(os.access(self.root/"attempt/verifier/scripts/verify.sh",os.X_OK))

    def test_added_protected_test_is_tampering(self):
        item=self.item()
        result=self.local(item,"Path('tests').mkdir(); Path('tests/conftest.py').write_text('exit(0)')")
        self.assertEqual(result.failure_class,"harness")
        self.assertEqual(result.final_verification,"failed")

    def test_verifier_restoration_cannot_hide_preverification_tampering(self):
        item=self.item()
        original="raise SystemExit(7)\n"
        (item.fixture/"verify.py").write_text(original)
        attack=f"from pathlib import Path\nPath('verify.py').write_text({original!r})\nraise SystemExit(0)\n"
        result=self.local(item,f"Path('verify.py').write_text({attack!r})")
        self.assertEqual(result.failure_class,"harness")
        saved=json.loads((self.root/"attempt/attempt.json").read_text())
        self.assertEqual(saved["verifier"][0]["exit"],7)
        self.assertEqual(saved["final_verification"],"failed")

    def test_readonly_input_mode_change_is_tampering(self):
        item=self.item(verify=["diff result.txt expected.txt"],readonly_inputs=["expected.txt"])
        (item.fixture/"expected.txt").write_text("correct")
        result=self.local(item,"Path('expected.txt').chmod(0o777); Path('result.txt').write_text('correct')")
        self.assertEqual(result.failure_class,"harness")

    def trace(self, target, output, workspace=None):
        path=self.root/"agent.stdout.jsonl"
        records=[{"type":"tool.call.requested","payload":{"id":"r","name":"read_file" if target.endswith('POLICY.md') else "grep","target":target}},
            {"type":"tool.call.completed","payload":{"id":"r","status":"completed","output":output}}]
        path.write_text('\n'.join(map(json.dumps,records)))
        if workspace is None:
            return backend.parse_events(path,[{"id":"policy","source":"docs/POLICY.md","contains":"marker"}])["discoveries"]
        return backend.parse_events(path,[{"id":"policy","source":"docs/POLICY.md","contains":"marker"}],workspace=workspace)["discoveries"]

    def test_absolute_target_discovery_stays_inside_workspace(self):
        workspace=self.root/"workspace";workspace.mkdir()
        self.assertEqual(len(self.trace(str(workspace/"docs/POLICY.md"),"marker",workspace)),1)
        self.assertEqual(self.trace("/outside/docs/POLICY.md","marker",workspace),[])
        self.assertEqual(self.trace(str(workspace/"docs/../wrong/POLICY.md"),"marker",workspace),[])

    def test_search_source_and_marker_must_share_output_line(self):
        self.assertEqual(self.trace("docs","docs/POLICY.md: no constraint\ndocs/OTHER.md: marker"),[])
        self.assertEqual(len(self.trace("docs","docs/POLICY.md: marker\ndocs/OTHER.md: other")),1)

    def test_duplicate_evidence_ids_rejected(self):
        with self.assertRaisesRegex(scenario.ScenarioError,"duplicate evidence"):
            self.item(evidence=[{"id":"same","source":"one","contains":"a"},{"id":"same","source":"two","contains":"b"}])

    def test_duplicate_scenario_ids_rejected(self):
        item=self.item()
        shutil.copytree(item.path.parent,self.root/"second")
        with self.assertRaisesRegex(scenario.ScenarioError,"duplicate scenario"):
            scenario.discover(self.root)

    def diff_run(self, run_id, outcome, **changes):
        protocol = {
            "source_commit": "commit", "binary_sha256": "b" * 64,
            "requested_model": "vendor/model", "reasoning_effort": "low",
            "backend": "local", "provider": "deepseek",
            "endpoint_identity": "https://api.example.test",
            "budget": {"max_turns": 7, "max_tool_calls": 11, "max_cost_usd": 0.75, "max_wall_time_secs": 30},
            "sampling": {"temperature": "unknown", "top_p": "unknown", "seed": "unknown"},
            "scenario_sha256": "s" * 64, "prompt_sha256": "p" * 64,
            "fixture_sha256": "f" * 64, "verifier_sha256": "v" * 64,
        }
        protocol.update(changes)
        attempt = {"outcome": outcome, "protocol": protocol}
        meta = {"id": "s", "axis": "S", "rung": 1, "difficulty": {"level": 1}, "difficulty_value": 1.0}
        return {"run_id": run_id, "protocol": {},
            "evaluation": {"manifest_sha256": "fixed", "split": "holdout", "suite_id": "fixed-v1"},
            "scenarios": [{"id": "s", "axis": "S", "rung": 1, "attempts": [attempt]}],
            "summary": report.summarize([(meta, [attempt])])}

    def test_diff_identity_drift_never_claims_fixed_or_regressed(self):
        changes = {
            "requested_model": "vendor/other", "endpoint_identity": "https://other.example.test",
            "reasoning_effort": "high", "budget": {"max_turns": 99},
            "binary_sha256": "different", "verifier_sha256": "different",
            "fixture_sha256": "different", "scenario_sha256": "different", "prompt_sha256": "different",
            "prepare_sha256": "different", "sampling": {"temperature": 0.8}, "provider": "other",
        }
        for key, value in changes.items():
            for old_outcome, new_outcome in (("fail", "pass"), ("pass", "fail")):
                with self.subTest(key=key, before=old_outcome):
                    baseline = self.diff_run("old", old_outcome)
                    current = self.diff_run("new", new_outcome, **{key: value})
                    self.assertEqual(report.diff(current, baseline)["transitions"],
                        [{"id": "s", "change": "incomparable"}])

    def test_diff_same_known_group_keeps_real_outcome_transition(self):
        for old_outcome, new_outcome, expected in (("fail", "pass", "fixed"), ("pass", "fail", "regressed")):
            with self.subTest(before=old_outcome):
                self.assertEqual(report.diff(self.diff_run("new", new_outcome), self.diff_run("old", old_outcome))["transitions"],
                    [{"id": "s", "change": expected}])

    def test_diff_controlled_mock_ports_use_same_group_but_real_origins_do_not(self):
        baseline = self.diff_run("old", "fail", provider="mock", provider_fixture_sha256="a" * 64,
            endpoint_identity="http://127.0.0.1:55101", endpoint_comparison_identity="controlled-mock:" + "a" * 64)
        current = self.diff_run("new", "pass", provider="mock", provider_fixture_sha256="a" * 64,
            endpoint_identity="http://127.0.0.1:55112", endpoint_comparison_identity="controlled-mock:" + "a" * 64)
        self.assertEqual(report.diff(current, baseline)["transitions"], [{"id": "s", "change": "fixed"}])
        current["scenarios"][0]["attempts"][0]["protocol"]["provider_fixture_sha256"] = "c" * 64
        self.assertEqual(report.diff(current, baseline)["transitions"], [{"id": "s", "change": "incomparable"}])

    def test_diff_unknown_identity_and_empty_attempts_are_incomparable(self):
        for missing in ("manifest", "requested_model", "endpoint_identity", "budget", "verifier_sha256", "attempts"):
            with self.subTest(missing=missing):
                baseline, current = self.diff_run("old", "fail"), self.diff_run("new", "pass")
                for value in (baseline, current):
                    if missing == "manifest": value["evaluation"]["manifest_sha256"] = "unknown"
                    elif missing == "attempts": value["scenarios"][0]["attempts"] = []
                    else: value["scenarios"][0]["attempts"][0]["protocol"].pop(missing)
                self.assertEqual(report.diff(current, baseline)["transitions"], [{"id": "s", "change": "incomparable"}])

    def test_diff_split_manifest_and_mixed_attempt_groups_are_incomparable(self):
        for change in ("split", "suite_id", "manifest_sha256", "mixed"):
            with self.subTest(change=change):
                baseline, current = self.diff_run("old", "fail"), self.diff_run("new", "pass")
                if change == "mixed":
                    for value in (baseline, current):
                        alternate = json.loads(json.dumps(value["scenarios"][0]["attempts"][0]))
                        alternate["protocol"]["budget"]["max_turns"] = 99
                        value["scenarios"][0]["attempts"].append(alternate)
                else: current["evaluation"][change] = "different"
                self.assertEqual(report.diff(current, baseline)["transitions"], [{"id": "s", "change": "incomparable"}])

    def test_against_last_runner_artifact_does_not_claim_model_change_fixed(self):
        item = self.item(verify=["diff result.txt expected.txt"], readonly_inputs=["expected.txt"],
            evaluation={"split": "holdout", "suite_id": "fixed-v1"})
        (item.fixture / "expected.txt").write_text("correct")
        binary = self.binary("Path('result.txt').write_text('correct' if 'vendor/new' in sys.argv else 'wrong')")
        args = argparse.Namespace(provider="mock", model="vendor/old", reasoning_effort="low", endpoint=None,
            max_turns=7, max_tool_calls=11, max_cost_usd=0.75, max_wall_time_secs=30,
            profile="smoke", k=1, out=str(self.root / "runs"), max_usd=5,
            against=None, record=False, split="holdout", suite_id="fixed-v1")
        with patch.object(backend, "MockProvider") as provider:
            provider.return_value.port = 12345
            baseline = run.run_scenarios(args, [item], binary)
            args.model, args.against = "vendor/new", "last"
            current = run.run_scenarios(args, [item], binary)
        self.assertEqual(baseline["scenarios"][0]["attempts"][0]["outcome"], "fail")
        self.assertEqual(current["scenarios"][0]["attempts"][0]["outcome"], "pass")
        saved = json.loads((Path(current["out"]) / "run.json").read_text())
        self.assertEqual(saved["diff"]["transitions"], [{"id": item.id, "change": "incomparable"}])
        text = (Path(current["out"]) / "report.md").read_text()
        self.assertIn("incomparable", text)
        self.assertNotIn("- fixed:", text)

    def test_bool_cost_and_usage_are_unknown(self):
        for key in ("cost_usd","tokens_in","tokens_out","wall_seconds"):
            with self.subTest(key=key):
                result=metrics.cost_per_pass([{"outcome":"pass",key:True}])
                self.assertEqual(result[key+"_per_pass"],"unknown")
                self.assertEqual(result[key+"_observed_attempts"],0)

    def sample(self,run_id="same"):
        return {"run_id":run_id,"evaluation":{"manifest_sha256":"fixed"},"scenarios":[{"id":"s","attempts":[{"outcome":"pass","cost_usd":0.2}]}]}

    def test_duplicate_runs_do_not_narrow_confidence(self):
        sample=self.sample()
        one=report.aggregate_runs([sample]);two=report.aggregate_runs([sample,json.loads(json.dumps(sample))])
        self.assertEqual(two,one)

    def test_conflicting_run_id_is_rejected(self):
        one=self.sample();two=self.sample();two["scenarios"][0]["attempts"][0]["outcome"]="fail"
        with self.assertRaisesRegex(ValueError,"conflicting run"):
            report.aggregate_runs([one,two])

    def test_duplicate_aggregate_roots_cli(self):
        directory=self.root/"runs/one";directory.mkdir(parents=True)
        (directory/"run.json").write_text(json.dumps(self.sample()))
        proc=subprocess.run([sys.executable,"-B",str(HERE/"run.py"),"--aggregate",str(directory.parent),
            "--aggregate",str(directory.parent/"../runs"),"--out",str(self.root/"aggregate")],capture_output=True,text=True)
        self.assertEqual(proc.returncode,0,proc.stderr)
        groups=json.loads((self.root/"aggregate/aggregate.json").read_text())["groups"]
        self.assertEqual(groups[0]["attempts"],1)

    def test_sqlite_encoded_path_reads_actual_state_readonly(self):
        fixture=HERE/"scenarios/G-1-external-state/fixture"
        path=self.root/"state ?# db.sqlite"
        env={**os.environ,"CAPABILITY_EXTERNAL_DB":str(path)}
        for script in ("prepare.py","app.py","verify.py"):
            proc=subprocess.run([sys.executable,"-B",str(fixture/script)],cwd=self.root,env=env,capture_output=True,text=True)
            self.assertEqual(proc.returncode,0,proc.stderr)
        with sqlite3.connect(path) as db:
            self.assertEqual(db.execute("SELECT state FROM tickets").fetchone(),("closed",))

    def test_goal_final_verification_and_protocol_artifact(self):
        for code,expected in ((0,"passed"),(3,"failed")):
            item=self.item(command="goal")
            (item.fixture/"verify.py").write_text(f"raise SystemExit({code})\n")
            with patch.object(goal_episode,"run",return_value={"error":None}):
                result=backend.run_local(item,self.root/expected,self.binary("pass"))
            self.assertEqual(result.final_verification,expected)
            saved=json.loads((self.root/expected/"attempt.json").read_text())
            self.assertEqual(saved["final_verification"],result.final_verification)
            self.assertEqual(saved["protocol"],result.protocol)

    def test_shell_adjacent_operator_binds_trusted_program(self):
        item=self.item(verify=["python3 -B verify.py&&true"])
        (item.fixture/"verify.py").write_text("raise SystemExit(9)\n")
        result=self.local(item,"Path('verify.py').write_text('raise SystemExit(0)\\n')")
        saved=json.loads((self.root/"attempt/attempt.json").read_text())
        self.assertEqual(saved["verifier"][0]["exit"],9)
        self.assertEqual(result.final_verification,"failed")

    def test_prepare_generated_readonly_input_is_supported(self):
        item=self.item(verify=["diff result.txt expected.txt"],readonly_inputs=["expected.txt"])
        prepare=item.path.parent/"prepare.py";prepare.write_text("from pathlib import Path\nPath('expected.txt').write_text('correct')\n")
        item.prepare=prepare
        result=self.local(item,"Path('result.txt').write_text('correct')")
        self.assertEqual(result.final_verification,"passed")

    def test_unknown_run_path_alias_loaded_once_without_collapsing_distinct_runs(self):
        directory=self.root/"runs/one";directory.mkdir(parents=True)
        sample=self.sample("unknown");(directory/"run.json").write_text(json.dumps(sample))
        (directory.parent/"alias").symlink_to(directory,target_is_directory=True)
        proc=subprocess.run([sys.executable,"-B",str(HERE/"run.py"),"--aggregate",str(directory.parent),
            "--out",str(self.root/"aggregate")],capture_output=True,text=True)
        self.assertEqual(proc.returncode,0,proc.stderr)
        groups=json.loads((self.root/"aggregate/aggregate.json").read_text())["groups"]
        self.assertEqual(sum(g["attempts"] for g in groups),1)
        self.assertEqual(sum(g["attempts"] for g in report.aggregate_runs([sample,json.loads(json.dumps(sample))])["groups"]),2)

    def test_duplicate_scenario_ids_in_imported_run_rejected(self):
        sample=self.sample();sample["scenarios"]*=2
        with self.assertRaisesRegex(ValueError,"duplicate scenario"):
            report.aggregate_runs([sample])

    def test_python_skin_readonly_tests_load_editable_application(self):
        item=scenario.load(HERE/"scenarios/H-1-python-skin/scenario.json")
        item.provider="mock"
        action="p=Path('summary.py'); p.write_text(p.read_text().replace('raise NotImplementedError(\"implement summarize\")', 'return Summary(sum(values), min(values) if values else 0, max(values) if values else 0, len(values))'))"
        result=self.local(item,action)
        self.assertEqual(result.final_verification,"passed")
        saved=json.loads((self.root/"attempt/attempt.json").read_text())
        self.assertIn(str(self.root/"attempt/verifier/tests"),saved["verifier"][0]["command"])
        self.assertNotIn("summary.py",saved["verifier"][0]["command"])

    def test_legacy_hidden_artifact_is_only_mounted_for_verification(self):
        item=self.item(provider="deepseek",image="offline",verify=["diff result.txt /artifacts/hidden.txt"])
        prepare=item.path.parent/"prepare.py"
        prepare.write_text("from pathlib import Path\nPath('../hidden.txt').write_text('correct')\n")
        item.prepare=prepare
        musl=self.root/"target/x86_64-unknown-linux-musl/release/orca";musl.parent.mkdir(parents=True);musl.write_bytes(b"offline")
        auth=self.root/".orca/auth.json";auth.parent.mkdir();auth.write_text(json.dumps({"DEEPSEEK_API_KEY":"offline-only"}))
        calls=[];real_run=subprocess.run
        def emulate(argv,**kwargs):
            if argv[0] != "docker":return real_run(argv,**kwargs)
            calls.append(argv)
            mounts={}
            for i,arg in enumerate(argv):
                if arg=="-v":
                    host,target,*mode=argv[i+1].split(":");mounts[target]=host
            if kwargs.get("stdout") is not None:
                Path(mounts["/workspace"]).joinpath("result.txt").write_text("correct")
                return subprocess.CompletedProcess(argv,0)
            command=argv[-1]
            for target,host in sorted(mounts.items(),key=lambda p:-len(p[0])):command=command.replace(target,host)
            return real_run(["bash","-lc",command],cwd=mounts["/workspace"],capture_output=True,text=True)
        with patch.object(backend,"REPO",self.root),patch.object(backend.Path,"home",return_value=self.root),patch.object(backend.subprocess,"run",side_effect=emulate):
            result=backend.run_docker(item,self.root/"docker")
        self.assertEqual(result.final_verification,"passed")
        self.assertNotIn("/artifacts",calls[0][-1])
        self.assertIn("/verifier/artifacts/hidden.txt",calls[1][-1])
        self.assertFalse(any(v.startswith(str(self.root/"docker")+":") for v in calls[0]))

    def test_readonly_python_dependency_and_input_are_snapshots_with_live_output(self):
        item=self.item(verify=["python3 -B verify.py expected.json result.json"],readonly_inputs=["rules.py","expected.json"])
        (item.fixture/"rules.py").write_text("def check(a,b): return a == b\n")
        (item.fixture/"expected.json").write_text('{"correct": 1}')
        (item.fixture/"verify.py").write_text("import json, sys\nfrom pathlib import Path\nfrom rules import check\nraise SystemExit(0 if check(json.loads(Path(sys.argv[1]).read_text()), json.loads(Path(sys.argv[2]).read_text())) else 6)\n")
        for output,code in (("correct",0),("wrong",6)):
            with self.subTest(output=output):
                result=self.local(item,f"Path('result.json').write_text('{{\"{output}\": 1}}')",output)
                saved=json.loads((self.root/output/"attempt.json").read_text())
                self.assertEqual(saved["verifier"][0]["exit"],code)
                self.assertEqual(result.final_verification,"passed" if code==0 else "failed")

    def test_docker_verifier_executes_independent_readonly_copy_with_live_output(self):
        item=self.item(provider="deepseek",image="offline",verify=["python3 -B verify.py","diff result.txt expected.txt"],readonly_inputs=["expected.txt"])
        (item.fixture/"verify.py").write_text("from pathlib import Path\nraise SystemExit(0 if Path('result.txt').read_text() == 'correct' else 9)\n")
        (item.fixture/"expected.txt").write_text("correct")
        (item.fixture/"result.txt").write_text("wrong")
        musl=self.root/"target/x86_64-unknown-linux-musl/release/orca";musl.parent.mkdir(parents=True);musl.write_bytes(b"offline")
        auth=self.root/".orca/auth.json";auth.parent.mkdir();auth.write_text(json.dumps({"DEEPSEEK_API_KEY":"offline-only"}))
        calls=[];real_run=subprocess.run
        def emulate(argv,**kwargs):
            calls.append(argv)
            mounts={}
            for index,arg in enumerate(argv):
                if arg=="-v":
                    host,target,*mode=argv[index+1].split(":")
                    mounts[target]=(host,mode)
            workspace=Path(mounts["/workspace"][0])
            if kwargs.get("stdout") is not None:
                workspace.joinpath("result.txt").write_text("wrong")
                original=workspace.joinpath("verify.py").read_text()
                workspace.joinpath("verify.py").write_text(f"from pathlib import Path\nPath('verify.py').write_text({original!r})\nraise SystemExit(0)\n")
                return subprocess.CompletedProcess(argv,0)
            command=argv[-1]
            for target,(host,mode) in sorted(mounts.items(),key=lambda p:-len(p[0])):
                command=command.replace(target,host)
            return real_run(["bash","-lc",command],cwd=workspace,capture_output=True,text=True)
        with patch.object(backend,"REPO",self.root),patch.object(backend.Path,"home",return_value=self.root),patch.object(backend.subprocess,"run",side_effect=emulate):
            result=backend.run_docker(item,self.root/"docker")
        # Same bytes can be restored by a replaced verifier; the trusted copy and
        # the pre-verification guard must still reject this attempt.
        self.assertEqual(result.failure_class,"harness")
        verify_argv=calls[1]
        self.assertIn(":ro",next(v for v in verify_argv if "/verifier:" in v))
        self.assertNotIn("/verifier",calls[0][-1])
        self.assertFalse(any(v.startswith(str(self.root/"docker")+":") for v in calls[0]))
        saved=json.loads((self.root/"docker/attempt.json").read_text())
        self.assertEqual(saved["verifier"][0]["exit"],9)
        self.assertEqual(saved["protocol"],result.protocol)
        self.assertEqual(saved["final_verification"],result.final_verification)
        self.assertIn("result.txt",shlex.split(verify_argv[-1]))
        self.assertIn("/verifier/expected.txt",verify_argv[-1])

if __name__=="__main__":unittest.main()
