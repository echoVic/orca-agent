use std::fs;
use std::path::Path;

const FORBIDDEN_PUBLIC_CLI_CLAIMS: &[&str] = &[
    "@orcla/cli",
    "orca.ai",
    "orca sessions",
    "orca goal",
    "orca context",
    "orca config ",
    "orca mcp",
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
