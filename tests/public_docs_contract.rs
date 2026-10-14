use std::fs;
use std::path::Path;

const FORBIDDEN_PUBLIC_CLI_CLAIMS: &[&str] = &[
    "@orcla/cli",
    "orca.ai",
    "orca sessions",
    "orca goal",
    "orca context",
    "orca config ",
    "orca skill",
    "--max-cost ",
    "--output ",
    "--approval",
    "--reasoning-effort",
    "--no-jsonl",
    "--verifier-timeout",
    "--no-verifier",
    "--total-max-cost",
    "--total-max-turns",
];

#[test]
fn public_docs_only_describe_the_current_cli_contract() {
    let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("site/src/docs/md");
    let mut pages = Vec::new();
    collect_mdx_pages(&docs, &mut pages);

    assert!(!pages.is_empty(), "public documentation tree is missing");
    for page in pages {
        let source = fs::read_to_string(&page).expect("public documentation is readable");
        for forbidden in FORBIDDEN_PUBLIC_CLI_CLAIMS {
            assert!(
                !source.contains(forbidden),
                "{} still claims obsolete public CLI surface: {forbidden}",
                page.display()
            );
        }
    }
}

#[test]
fn computer_use_examples_grant_only_the_named_app_and_disable_foreground_or_the_tool() {
    use orca_core::approval_rules::CompiledPermissionRules;
    use orca_core::approval_types::Decision;
    use orca_core::config::file::FileConfig;

    let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("site/src/docs/md");
    for locale in ["en", "zh"] {
        let page = docs.join(locale).join("computer-use.mdx");
        // A Windows checkout may have CRLF line endings.
        let source = fs::read_to_string(&page)
            .unwrap_or_else(|error| panic!("{} must be readable: {error}", page.display()))
            .replace("\r\n", "\n");
        let examples = source
            .split("~~~toml\n")
            .skip(1)
            .map(|block| block.split_once("~~~").expect("closed TOML fence").0)
            .map(|example| {
                toml::from_str::<FileConfig>(example).expect("usable user configuration")
            })
            .collect::<Vec<_>>();
        assert_eq!(examples.len(), 4, "{locale}: four actionable examples");

        for (example, app) in examples[..2]
            .iter()
            .zip(["com.apple.Safari", "com.apple.Passwords"])
        {
            let rules = CompiledPermissionRules::from_rules(example.permissions.clone());
            assert_eq!(
                rules.matching_decision("computer", Some(app)),
                Some(Decision::Allow)
            );
            assert_eq!(
                rules.matching_decision("computer", Some("com.apple.TextEdit")),
                None,
                "{locale}: an app grant must not authorize other apps"
            );
            assert_eq!(
                example.permissions.rules[0].pattern.as_deref(),
                Some(app),
                "{locale}: default-block overrides require an exact bundle id"
            );
        }
        assert!(!examples[2].computer_use.foreground_fallback);
        let rules = CompiledPermissionRules::from_rules(examples[3].permissions.clone());
        for target in [None, Some("com.apple.Safari"), Some("com.apple.TextEdit")] {
            assert_eq!(
                rules.matching_decision("computer", target),
                Some(Decision::Deny),
                "{locale}: disabling computer use must cover every target"
            );
        }
        assert_eq!(rules.matching_decision("read", None), None);
    }
}

#[test]
fn computer_use_docs_cover_secure_text_keys_and_the_whole_tool_rule_exception() {
    for locale in ["en", "zh"] {
        let source = public_page(locale, "computer-use");
        require_fragments(
            &source,
            &[
                "`[secure]`",
                "`type`",
                "`key`",
                "Shift",
                "Option",
                "`Cmd+V`",
                "`#secure`",
                "**Allow typing this once**",
                "**Deny**",
            ],
            locale,
        );
        let scope = match locale {
            "en" => [
                "each time",
                "Only a whole-tool allow rule",
                "without a `pattern`",
            ],
            _ => ["每次", "只有整个工具的 allow 规则", "不写 `pattern`"],
        };
        require_fragments(&source, &scope, locale);
    }
}

#[test]
fn computer_use_docs_cover_grants_for_every_app_and_full_auto() {
    for locale in ["en", "zh"] {
        let source = public_page(locale, "computer-use");
        require_fragments(
            &source,
            &[
                "`3` **Allow all apps this session**",
                "`5` **Always allow all apps (saved)**",
                "`pattern = \"*\"`",
                "`full-auto`",
            ],
            locale,
        );
        let scope = match locale {
            "en" => [
                "app calls run there without app approval",
                "Password entry still asks each time",
                "the app's first result in each session",
            ],
            _ => [
                "应用调用不再请求应用授权",
                "仍然每次询问",
                "第一次操作该应用的 结果里",
            ],
        };
        require_fragments(&source, &scope, locale);
    }
}

#[test]
fn computer_use_and_approval_docs_require_app_specific_grants_for_terminals_and_ides() {
    for locale in ["en", "zh"] {
        for page in ["computer-use", "approval-modes"] {
            let source = public_page(locale, page);
            let fragments: &[&str] = match locale {
                "en" => &[
                    "Outside `full-auto`, terminals and IDEs need approval for that app itself",
                    "`1`, `2`, or `7`",
                    "an allow rule whose `pattern` is the exact bundle id",
                    "**Allow all apps this session**, `pattern = \"*\"` or another wildcard, and a whole-tool allow rule do not cover them",
                    "Their approval panel does not offer the two all-app options",
                ],
                _ => &[
                    "在 `full-auto` 之外，终端和 IDE 需要针对该应用本身的授权",
                    "`1`、`2` 或 `7`",
                    "`pattern` 为精确 bundle id 的 allow 规则",
                    "**Allow all apps this session**、 `pattern = \"*\"` 或其他通配符，以及整个工具的 allow 规则都不覆盖它们",
                    "它们的授权面板不提供两个“所有应用”选项",
                ],
            };
            require_fragments(&source, fragments, &format!("{locale}/{page}"));
        }
    }
}

#[test]
fn computer_use_docs_explain_live_password_detection_without_sending_input() {
    for locale in ["en", "zh"] {
        let source = public_page(locale, "computer-use");
        let retry = match locale {
            "en" => [
                "point target or a stale observation",
                "sends no text",
                "target the secure field by its element id",
            ],
            _ => ["坐标目标或过期观察", "不会发送文字", "用元素 id 指向密码框"],
        };
        require_fragments(&source, &retry, locale);
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/releases/v0.5.11.md");
    let source = fs::read_to_string(path).expect("release notes are readable");
    require_fragments(
        &source,
        &[
            "sends no text",
            "observe again",
            "target the secure field by its element id",
        ],
        "v0.5.11",
    );
}

#[test]
fn computer_use_docs_bound_system_wide_escape_to_the_computer_turn() {
    for locale in ["en", "zh"] {
        let source = public_page(locale, "computer-use");
        let timing = match locale {
            "en" => [
                "first `computer` call",
                "until the turn ends",
                "system-wide",
                "`Esc` interrupts the running turn",
            ],
            _ => [
                "第一次 `computer` 调用",
                "直到本轮结束",
                "系统范围",
                "`Esc` 中断正在运行的轮次",
            ],
        };
        require_fragments(&source, &timing, locale);
        for unbounded in [
            "Press **Esc** at any time",
            "任何时候按 **Esc** 都可以中断当前轮次",
        ] {
            assert!(!source.contains(unbounded), "{locale}: unbounded Esc claim");
        }
    }
}

#[test]
fn computer_use_docs_explain_screenshot_click_and_q_to_close() {
    for locale in ["en", "zh"] {
        let source = public_page(locale, "computer-use");
        let controls = match locale {
            "en" => [
                "Click the `[N image(s) attached]` line",
                "Press `q` to close",
            ],
            _ => ["点击 `[N image(s) attached]` 这一行", "按 `q` 关闭"],
        };
        require_fragments(&source, &controls, locale);
    }
}

#[test]
fn computer_use_docs_describe_background_launch_and_partial_trees() {
    for locale in ["en", "zh"] {
        let source = public_page(locale, "computer-use");
        require_fragments(&source, &["`launch_app`", "`truncated`", "`query`"], locale);
        let behavior = match locale {
            "en" => [
                "without requesting activation",
                "time limit",
                "point targets",
            ],
            _ => ["不请求激活", "时间上限", "坐标目标"],
        };
        require_fragments(&source, &behavior, locale);
    }
}

#[test]
fn computer_use_docs_explain_conditional_input_monitoring_permission() {
    for locale in ["en", "zh"] {
        let source = public_page(locale, "computer-use");
        let permission = match locale {
            "en" => [
                "**Accessibility**",
                "**Screen Recording**",
                "**Input Monitoring**",
                "if the driver reports",
            ],
            _ => [
                "**辅助功能**",
                "**屏幕录制**",
                "**输入监控**",
                "如果驱动提示",
            ],
        };
        require_fragments(&source, &permission, locale);
    }
}

#[test]
fn computer_use_and_approval_docs_explain_plan_and_full_auto_app_approval() {
    for locale in ["en", "zh"] {
        for page in ["computer-use", "approval-modes"] {
            let source = public_page(locale, page);
            require_fragments(
                &source,
                &["`list_apps`", "`observe`", "`full-auto`"],
                &format!("{locale}/{page}"),
            );
            let scope = match locale {
                "en" => ["only `list_apps` and `observe`", "without app approval"],
                _ => ["只允许 `list_apps` 和 `observe`", "不再请求应用授权"],
            };
            require_fragments(&source, &scope, &format!("{locale}/{page}"));
        }
    }
}

#[test]
fn approval_docs_describe_computer_app_keys_and_call_only_approvals() {
    for locale in ["en", "zh"] {
        let source = public_page(locale, "approval-modes");
        require_fragments(
            &source,
            &[
                "`2` Allow this app this session",
                "`3` Allow all apps this session",
                "`7` Always allow this app (saved)",
                "`5` Always allow all apps (saved)",
                "**Allow typing this once**",
                "`#secure`",
            ],
            locale,
        );
        let scope = match locale {
            "en" => [
                "only `1` and `4`",
                "no resolved app target",
                "whole-tool allow rule",
            ],
            _ => [
                "只有 `1` 和 `4`",
                "没有解析出应用目标",
                "整个工具的 allow 规则",
            ],
        };
        require_fragments(&source, &scope, locale);
    }
}

#[test]
fn release_notes_retain_the_computer_use_approval_and_preview_boundaries() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/releases/v0.5.11.md");
    let source = fs::read_to_string(path).expect("release notes are readable");
    require_fragments(
        &source,
        &[
            "`Cmd+V`",
            "`#secure`",
            "whole-tool allow rule",
            "first `computer` call",
            "until the turn ends",
            "`[N image(s) attached]`",
            "`q`",
            "only `list_apps` and `observe`",
            "`full-auto`",
            "Input Monitoring",
            "`launch_app`",
            "`truncated`",
            "`query`",
        ],
        "v0.5.11",
    );
}

fn public_page(locale: &str, page: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("site/src/docs/md")
        .join(locale)
        .join(format!("{page}.mdx"));
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} must be readable: {error}", path.display()))
}

fn require_fragments(source: &str, fragments: &[&str], page: &str) {
    let text = source.split_whitespace().collect::<Vec<_>>().join(" ");
    for fragment in fragments {
        assert!(
            text.contains(fragment),
            "{page}: missing public behavior contract {fragment:?}"
        );
    }
}

fn collect_mdx_pages(root: &Path, pages: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(root).expect("public documentation directory is readable") {
        let entry = entry.expect("public documentation entry is readable");
        let path = entry.path();
        if path.is_dir() {
            collect_mdx_pages(&path, pages);
        } else if path.extension().is_some_and(|extension| extension == "mdx") {
            pages.push(path);
        }
    }
}
