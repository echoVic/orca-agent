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
        let source = fs::read_to_string(&page)
            .unwrap_or_else(|error| panic!("{} must be readable: {error}", page.display()));
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
