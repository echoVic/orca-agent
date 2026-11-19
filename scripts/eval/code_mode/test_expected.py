"""Tests for expected.py.

Run with: python3 -m unittest scripts/eval/code_mode/test_expected.py
"""

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import expected  # noqa: E402


def checkout(files: dict[str, str]) -> Path:
    root = Path(tempfile.mkdtemp(prefix="code-mode-expected-"))
    for name, text in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    return root


class UserTrustedLaunches(unittest.TestCase):
    def setUp(self):
        self.root = checkout({
            "crates/orca-runtime/src/hooks.rs": "broker.launch_user_trusted(a);",
            "crates/orca-runtime/src/workflow/host.rs":
                "x.launch_user_trusted(a); y.launch_user_trusted(b);",
            "crates/orca-runtime/src/quiet.rs": "fn nothing() {}",
        })

    def test_every_file_with_its_count_is_right(self):
        answer = "crates/orca-runtime/src/hooks.rs: 1\ncrates/orca-runtime/src/workflow/host.rs: 2"
        self.assertTrue(expected.is_right("user-trusted-launches", answer, self.root))

    def test_paths_under_the_directory_asked_about_are_accepted(self):
        answer = "| hooks.rs | 1 |\n| workflow/host.rs | 2 |"
        self.assertTrue(expected.is_right("user-trusted-launches", answer, self.root))

    def test_a_wrong_count_is_wrong(self):
        answer = "crates/orca-runtime/src/hooks.rs: 1\ncrates/orca-runtime/src/workflow/host.rs: 3"
        self.assertFalse(expected.is_right("user-trusted-launches", answer, self.root))

    def test_a_list_index_is_not_a_count(self):
        # The index 2 equals the right count of host.rs; the count stated is 5.
        answer = "1. hooks.rs \u2014 1\n2. workflow/host.rs \u2014 5"
        self.assertFalse(expected.is_right("user-trusted-launches", answer, self.root))
        answer = "1) hooks.rs: 7\n- 2. workflow/host.rs: 2"
        self.assertFalse(expected.is_right("user-trusted-launches", answer, self.root))

    def test_a_numbered_list_with_the_right_counts_is_right(self):
        answer = "1. hooks.rs \u2014 1\n2. workflow/host.rs \u2014 2"
        self.assertTrue(expected.is_right("user-trusted-launches", answer, self.root))

    def test_a_file_that_has_none_is_wrong(self):
        answer = "hooks.rs: 1\nworkflow/host.rs: 2\nquiet.rs: 1"
        self.assertFalse(expected.is_right("user-trusted-launches", answer, self.root))

    def test_a_sentence_that_mentions_other_files_names_none(self):
        answer = ("hooks.rs: 1\nworkflow/host.rs: 2"
                  "\n\nThe definitions live in `crates/orca-core/src/execution_broker.rs`, "
                  "and calls in `tasks.rs` were excluded. No files were modified.")
        self.assertTrue(expected.is_right("user-trusted-launches", answer, self.root))

    def test_an_extra_file_as_a_bullet_or_backticked_item_is_wrong(self):
        for extra in ("- quiet.rs: 1", "`quiet.rs` \u2014 1", "* **quiet.rs**: 1", "| quiet.rs | 1 |"):
            with self.subTest(extra=extra):
                answer = "hooks.rs: 1\nworkflow/host.rs: 2\n" + extra
                self.assertFalse(expected.is_right("user-trusted-launches", answer, self.root))


class WorkflowEvents(unittest.TestCase):
    def setUp(self):
        self.root = checkout({
            "crates/orca-core/src/event_schema.rs": """
                #[serde(rename = "workflow.started")] Started,
                #[serde(rename = "workflow.phase.completed")] PhaseCompleted,
                #[serde(rename = "tool.started")] ToolStarted,
                fn fixture() { Some("workflow.md".to_string()) }
            """,
        })
        self.listed = "1. workflow.started\n2. workflow.phase.completed\n"

    def right(self, answer):
        return expected.is_right("workflow-events", answer, self.root)

    def test_all_of_them_and_the_total_is_right(self):
        self.assertTrue(self.right("workflow.phase.completed\nworkflow.started\n\nTotal: 2."))

    def test_one_missing_is_wrong(self):
        self.assertFalse(self.right("workflow.started\nTotal: 1"))

    def test_a_wrong_total_is_wrong(self):
        self.assertFalse(self.right("workflow.phase.completed\nworkflow.started\nTotal: 3"))

    def test_a_file_name_that_is_not_an_event_is_not_expected(self):
        self.assertTrue(self.right("workflow.phase.completed\nworkflow.started\nTotal: 2"))
        self.assertFalse(self.right("workflow.md\nworkflow.phase.completed\nworkflow.started\nTotal: 3"))
        self.assertFalse(self.right("workflow.md\nworkflow.phase.completed\nworkflow.started\nTotal: 2"))

    def test_a_numbered_list_with_the_right_total_is_right(self):
        self.assertTrue(self.right(self.listed + "\nTotal: 2"))

    def test_the_numbers_of_a_list_are_not_the_total(self):
        self.assertFalse(self.right(self.listed + "\nTotal: 1"))
        self.assertFalse(self.right(self.listed))

    def test_two_different_totals_are_wrong(self):
        self.assertFalse(self.right(self.listed + "\nTotal: 2\nIn all, total: 3."))
        # A number on a line that is not a total line is not a second total.
        self.assertTrue(self.right(self.listed + "\nTotal: 2\nThat is 3 in all."))

    def test_a_bare_prefix_names_no_event(self):
        self.assertTrue(self.right(self.listed + "\nThey start with `workflow.`. Total: 2"))

    def test_a_missing_schema_file_is_an_error(self):
        with self.assertRaises(OSError):
            expected.is_right("workflow-events", "", checkout({}))

    PROSE = ("Found them: 2 `EventType` serde renames "
             "(`crates/orca-core/src/event_schema.rs:226-253`). "
             "The `\"workflow.md\"` at line 1873 is a test fixture path, not an event type, "
             "so it's excluded.\n\n")
    BLOCK = "```\nworkflow.phase.completed\nworkflow.started\n```\n\n"

    def test_a_list_in_a_code_block_with_prose_around_it_is_right(self):
        self.assertTrue(self.right(self.PROSE + self.BLOCK + "Total: **2**\n\nNo files were modified."))
        self.assertTrue(self.right(self.PROSE + self.BLOCK + "**Total: 2**\n\nNo files were modified."))

    def test_a_count_in_the_first_sentence_without_a_total_line_is_right(self):
        answer = ("2 event type strings begin with `workflow.` (lines 226\u2013253):\n\n"
                  + self.BLOCK + "No files were modified.")
        self.assertTrue(self.right(answer))

    def test_a_wrong_total_line_is_wrong_even_with_prose(self):
        self.assertFalse(self.right(self.PROSE + self.BLOCK + "Total: 3\n"))

    def test_a_file_name_listed_as_a_line_is_wrong_even_with_prose(self):
        block = "```\nworkflow.md\nworkflow.phase.completed\nworkflow.started\n```\n\n"
        self.assertFalse(self.right(self.PROSE + block + "Total: 2"))

    def test_an_event_only_in_a_sentence_is_not_listed(self):
        answer = ("There are 2 events; workflow.phase.completed is the other one.\n"
                  "```\nworkflow.started\n```\nTotal: 2")
        self.assertFalse(self.right(answer))

    def test_several_names_on_one_line_are_listed(self):
        self.assertTrue(self.right("`workflow.phase.completed`, `workflow.started`\nTotal: 2"))


class NetworkTools(unittest.TestCase):
    def setUp(self):
        self.root = checkout({
            "crates/orca-tools/src/registry.rs": """
                registry.register(BuiltinTool::new(builtin_spec(
                    "read_file", "Read.", json!({}), CapabilitySet::read_only_fs(),
                )));
                registry.register(BuiltinTool::new(builtin_spec(
                    "grep", "Search.", json!({}), CapabilitySet::read_only_fs(),
                )));
                registry.register(BuiltinTool::new(cooperative_builtin_spec(
                    "web_search", "Search.", json!({}),
                    CapabilitySet::network_search(),
                )));
                fn of(kind: ActionKind) { ActionKind::Network => CapabilitySet::network_search() }
            """,
        })

    def right(self, answer):
        return expected.is_right("network-tools", answer, self.root)

    def test_the_network_tools_alone_are_right(self):
        self.assertTrue(self.right("`web_search`"))

    def test_another_tool_named_with_them_is_wrong(self):
        self.assertFalse(self.right("web_search, read_file"))

    def test_a_plain_line_of_names_is_right(self):
        self.assertTrue(self.right("web_search"))
        self.assertTrue(self.right("- web_search"))
        self.assertTrue(self.right("1. `web_search`."))

    def test_an_ordinary_word_that_is_a_tool_name_names_no_tool(self):
        self.assertTrue(self.right("The tool is `web_search`. I found it with grep."))

    def test_a_tool_in_backticks_inside_prose_is_named(self):
        self.assertFalse(self.right("`web_search` and also `grep`."))

    def test_none_named_is_wrong(self):
        self.assertFalse(self.right("I do not know."))


class LargestTestFiles(unittest.TestCase):
    def setUp(self):
        self.root = checkout({
            f"tests/t{count}.rs": "#[test]\nfn x() {}\n" * count for count in (9, 7, 5, 4, 3, 1)
        })

    def test_the_five_largest_in_order_with_counts_is_right(self):
        answer = "\n".join(f"tests/t{count}.rs — {count}" for count in (9, 7, 5, 4, 3))
        self.assertTrue(expected.is_right("largest-test-files", answer, self.root))

    def test_out_of_order_is_wrong(self):
        answer = "\n".join(f"tests/t{count}.rs — {count}" for count in (7, 9, 5, 4, 3))
        self.assertFalse(expected.is_right("largest-test-files", answer, self.root))

    def test_a_count_cut_short_is_wrong(self):
        answer = "\n".join(f"tests/t{count}.rs — {count}" for count in (9, 7, 5, 4, 2))
        self.assertFalse(expected.is_right("largest-test-files", answer, self.root))


class CredentialScrubSites(unittest.TestCase):
    def setUp(self):
        self.root = checkout({
            "crates/orca-platform/src/process.rs":
                "pub fn remove_credential_env(c: &mut Command) {}\nfn t() { remove_credential_env(&mut c); }",
            "crates/orca-tools/src/bash.rs": "process::remove_credential_env(&mut command);",
            "crates/orca-tools/src/quiet.rs": "fn nothing() {}",
        })

    def test_every_calling_file_is_right_with_or_without_the_defining_one(self):
        self.assertTrue(expected.is_right(
            "credential-scrub-sites", "crates/orca-tools/src/bash.rs: the shell", self.root))
        self.assertTrue(expected.is_right(
            "credential-scrub-sites",
            "crates/orca-tools/src/bash.rs: the shell\ncrates/orca-platform/src/process.rs: a test",
            self.root))

    def test_a_missing_file_is_wrong(self):
        self.assertFalse(expected.is_right(
            "credential-scrub-sites", "crates/orca-platform/src/process.rs: a test", self.root))

    def test_a_file_that_does_not_call_it_is_wrong(self):
        self.assertFalse(expected.is_right(
            "credential-scrub-sites",
            "crates/orca-tools/src/bash.rs: the shell\ncrates/orca-tools/src/quiet.rs: something",
            self.root))

    def test_a_sentence_that_mentions_another_file_names_none(self):
        self.assertTrue(expected.is_right(
            "credential-scrub-sites",
            "crates/orca-tools/src/bash.rs: the shell\n\nI skipped `crates/orca-tools/src/quiet.rs`.",
            self.root))

    def test_another_file_as_an_item_line_is_wrong(self):
        self.assertFalse(expected.is_right(
            "credential-scrub-sites",
            "- `crates/orca-tools/src/bash.rs`: the shell\n- `crates/orca-tools/src/quiet.rs`: x",
            self.root))


class EmptyAnswer(unittest.TestCase):
    """An empty answer is wrong, not an error, on a checkout that has the files;
    a checkout that lacks them is an error."""

    def setUp(self):
        self.root = checkout({
            "crates/orca-runtime/src/hooks.rs": "x.launch_user_trusted(a);",
            "crates/orca-core/src/event_schema.rs": '#[serde(rename = "workflow.started")] Started,',
            "crates/orca-tools/src/registry.rs":
                'builtin_spec("web_search", "S.", json!({}), CapabilitySet::network_search(),)',
            "tests/a.rs": "#[test]\nfn x() {}\n",
            "crates/orca-tools/src/bash.rs": "remove_credential_env(&mut c);",
        })

    def test_every_task_marks_it_wrong(self):
        for task in expected.CHECKS:
            with self.subTest(task=task):
                self.assertFalse(expected.is_right(task, "", self.root))

    def test_a_checkout_without_the_files_raises(self):
        empty = checkout({})
        for task in expected.CHECKS:
            with self.subTest(task=task):
                with self.assertRaises(Exception):
                    expected.is_right(task, "", empty)


class UnknownTask(unittest.TestCase):
    def test_a_task_with_no_checker_is_an_error(self):
        with self.assertRaises(KeyError):
            expected.is_right("no-such-task", "anything", Path("."))


if __name__ == "__main__":
    unittest.main()
