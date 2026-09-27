"""Unit tests for the repo-fix harness's parsing and agent invocation.

Run with: python3 -m unittest scripts/eval/test_repo_task.py
"""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import repo_task  # noqa: E402

NEXTEST_OUTPUT = """
        PASS [   0.353s] (1/3) blade-deepseek::acp_daemon model_read_file_uses_local_disk
        FAIL [   0.233s] (2/3) blade-deepseek::acp_daemon endpoint_is_private_singleton
        PASS [   0.101s] (3/3) orca-runtime server::tests::unit_level_test
"""


class NextestParsingTest(unittest.TestCase):
    def test_a_failing_nextest_test_is_named_by_its_test_not_its_counter(self):
        self.assertEqual(repo_task.failing_tests(NEXTEST_OUTPUT), {"endpoint_is_private_singleton"})

    def test_passing_nextest_tests_count_as_passing(self):
        self.assertEqual(
            repo_task.passing_tests(NEXTEST_OUTPUT), {"model_read_file_uses_local_disk"}
        )

    def test_libtest_output_still_parses(self):
        output = "test alpha ... ok\ntest beta ... FAILED\n"
        self.assertEqual(repo_task.passing_tests(output), {"alpha"})
        self.assertEqual(repo_task.failing_tests(output), {"beta"})


class AgentCommandTest(unittest.TestCase):
    def test_the_prompt_never_reaches_a_shell(self):
        # A commit message with shell syntax, a newline, and CJK text.
        prompt = "fix `rm -rf ~` and $(id)\nsecond line 中文"
        command, stdin = repo_task.agent_invocation(prompt, Path("/tmp/tree"), None, "/w")
        self.assertNotIn(prompt, " ".join(command))
        self.assertNotIn("$(id)", " ".join(command))
        self.assertEqual(stdin, prompt)


if __name__ == "__main__":
    unittest.main()
