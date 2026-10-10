use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use orca_file_search::{
    MatchKind, SearchMode, SearchPhase, SearchSession, SearchSessionOptions, SearchSnapshot,
    SessionGeneration,
};
use tempfile::tempdir;

fn wait_for_snapshot(
    session: &SearchSession,
    notifications: &mpsc::Receiver<SessionGeneration>,
    predicate: impl Fn(&SearchSnapshot) -> bool,
) -> SearchSnapshot {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        notifications
            .recv_timeout(remaining)
            .expect("search snapshot notification");
        if let Some(snapshot) = session.take_latest_snapshot()
            && predicate(&snapshot)
        {
            return snapshot;
        }
    }
}

#[test]
fn fuzzy_session_streams_complete_workspace_snapshot() {
    let root = tempdir().unwrap();
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::write(root.path().join(".gitignore"), "dist/\n").unwrap();
    fs::create_dir_all(root.path().join("src/runtime/config")).unwrap();
    fs::write(root.path().join("src/runtime/config/mod.rs"), "mod config;").unwrap();
    fs::write(root.path().join(".hidden.rs"), "hidden").unwrap();
    fs::create_dir(root.path().join("dist")).unwrap();
    fs::write(root.path().join("dist/generated.rs"), "generated").unwrap();

    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start(
        root.path(),
        SearchSessionOptions::new(SessionGeneration(1), move |generation| {
            let _ = notify_tx.send(generation);
        }),
    )
    .unwrap();
    session.update(SearchMode::fuzzy("rcm"));

    let snapshot = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        matches!(snapshot.phase, SearchPhase::Complete)
    });

    assert!(snapshot.matches.iter().any(|candidate| {
        candidate.path == "src/runtime/config/mod.rs" && candidate.kind == MatchKind::File
    }));
    assert!(snapshot.progress.walk_complete);
    assert!(snapshot.progress.scanned_paths >= 5);
    assert!(
        snapshot
            .matches
            .iter()
            .all(|candidate| !candidate.path.starts_with("dist/"))
    );
    session.join();
}

#[test]
fn browse_mode_lists_direct_children_while_catalog_builds() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("src/nested")).unwrap();
    fs::write(root.path().join("src/lib.rs"), "lib").unwrap();
    fs::write(root.path().join("src/nested/deep.rs"), "deep").unwrap();

    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start(
        root.path(),
        SearchSessionOptions::new(SessionGeneration(2), move |generation| {
            let _ = notify_tx.send(generation);
        }),
    )
    .unwrap();
    session.update(SearchMode::browse("src/"));

    // The catalog is still being built: a snapshot can come before it has
    // found both children, so wait for the one that lists them.
    let snapshot = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        let lists = |path: &str| {
            snapshot
                .matches
                .iter()
                .any(|candidate| candidate.path == path)
        };
        snapshot.mode == SearchMode::browse("src/") && lists("src/nested/") && lists("src/lib.rs")
    });

    assert_eq!(snapshot.matches[0].path, "src/nested/");
    assert!(
        snapshot
            .matches
            .iter()
            .all(|candidate| candidate.path != "src/nested/deep.rs")
    );
    session.join();
}

#[test]
fn multi_root_session_streams_matches_with_stable_root_identity() {
    let first = tempdir().unwrap();
    let second = tempdir().unwrap();
    fs::create_dir_all(first.path().join("src")).unwrap();
    fs::create_dir_all(second.path().join("src")).unwrap();
    fs::write(first.path().join("src/main.rs"), "first").unwrap();
    fs::write(second.path().join("src/main.rs"), "second").unwrap();
    fs::write(second.path().join("src/worker.rs"), "worker").unwrap();

    let roots = vec![first.path().to_path_buf(), second.path().to_path_buf()];
    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start_roots(
        &roots,
        SearchSessionOptions::new(SessionGeneration(20), move |generation| {
            let _ = notify_tx.send(generation);
        })
        .with_result_limit(32),
    )
    .unwrap();
    session.update(SearchMode::fuzzy("main"));

    let snapshot = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        matches!(snapshot.phase, SearchPhase::Complete)
    });
    let mut main_roots = snapshot
        .matches
        .iter()
        .filter(|candidate| candidate.path == "src/main.rs")
        .map(|candidate| candidate.root.clone())
        .collect::<Vec<_>>();
    main_roots.sort();

    let mut expected = roots
        .iter()
        .map(|root| root.canonicalize().unwrap())
        .collect::<Vec<PathBuf>>();
    expected.sort();
    assert_eq!(main_roots, expected);
    session.join();
}

#[test]
fn overlapping_roots_preserve_each_traversal_identity() {
    let workspace = tempdir().unwrap();
    let nested = workspace.path().join("pkg");
    fs::create_dir_all(nested.join("src")).unwrap();
    fs::write(nested.join("src/main.rs"), "nested").unwrap();

    let roots = vec![workspace.path().to_path_buf(), nested.clone()];
    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start_roots(
        &roots,
        SearchSessionOptions::new(SessionGeneration(23), move |generation| {
            let _ = notify_tx.send(generation);
        })
        .with_result_limit(32),
    )
    .unwrap();
    session.update(SearchMode::fuzzy("main"));

    let snapshot = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        matches!(snapshot.phase, SearchPhase::Complete)
    });
    let mut identities = snapshot
        .matches
        .iter()
        .filter(|candidate| candidate.path.ends_with("src/main.rs"))
        .map(|candidate| (candidate.root.clone(), candidate.path.clone()))
        .collect::<Vec<_>>();
    identities.sort();

    let mut expected = vec![
        (
            workspace.path().canonicalize().unwrap(),
            "pkg/src/main.rs".to_string(),
        ),
        (nested.canonicalize().unwrap(), "src/main.rs".to_string()),
    ];
    expected.sort();
    assert_eq!(identities, expected);
    session.join();
}

#[test]
fn multi_root_browse_lists_direct_children_from_each_root() {
    let first = tempdir().unwrap();
    let second = tempdir().unwrap();
    fs::create_dir_all(first.path().join("src/alpha")).unwrap();
    fs::create_dir_all(second.path().join("src/beta")).unwrap();
    fs::write(first.path().join("src/first.rs"), "first").unwrap();
    fs::write(second.path().join("src/second.rs"), "second").unwrap();

    let roots = vec![first.path().to_path_buf(), second.path().to_path_buf()];
    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start_roots(
        &roots,
        SearchSessionOptions::new(SessionGeneration(21), move |generation| {
            let _ = notify_tx.send(generation);
        })
        .with_result_limit(32),
    )
    .unwrap();
    session.update(SearchMode::browse("src/"));

    let snapshot = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        snapshot.mode == SearchMode::browse("src/")
            && matches!(snapshot.phase, SearchPhase::Complete)
    });

    assert!(snapshot.matches.iter().any(|candidate| {
        candidate.root == first.path().canonicalize().unwrap() && candidate.path == "src/first.rs"
    }));
    assert!(snapshot.matches.iter().any(|candidate| {
        candidate.root == second.path().canonicalize().unwrap() && candidate.path == "src/second.rs"
    }));
    assert!(
        snapshot
            .matches
            .iter()
            .all(|candidate| !candidate.path.ends_with("/first.rs/"))
    );
    session.join();
}

#[test]
fn search_options_apply_excludes_and_can_disable_gitignore() {
    let root = tempdir().unwrap();
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::write(root.path().join(".gitignore"), "ignored.rs\n").unwrap();
    fs::create_dir(root.path().join("generated")).unwrap();
    fs::write(root.path().join("ignored.rs"), "ignored").unwrap();
    fs::write(root.path().join("generated/skip.rs"), "skip").unwrap();
    fs::write(root.path().join("keep.rs"), "keep").unwrap();

    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start(
        root.path(),
        SearchSessionOptions::new(SessionGeneration(22), move |generation| {
            let _ = notify_tx.send(generation);
        })
        .with_result_limit(32)
        .with_excludes(["generated/**"])
        .with_respect_gitignore(false),
    )
    .unwrap();
    session.update(SearchMode::fuzzy("rs"));

    let snapshot = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        matches!(snapshot.phase, SearchPhase::Complete)
    });

    assert!(
        snapshot
            .matches
            .iter()
            .any(|candidate| candidate.path == "ignored.rs")
    );
    assert!(
        snapshot
            .matches
            .iter()
            .any(|candidate| candidate.path == "keep.rs")
    );
    assert!(
        snapshot
            .matches
            .iter()
            .all(|candidate| candidate.path != "generated/skip.rs")
    );
    session.join();
}

#[test]
fn cancellation_joins_synthetic_workers_within_deadline() {
    let root = tempdir().unwrap();
    let paths = (0..100_000)
        .map(|index| format!("src/package-{index:06}/file-{index:06}.rs"))
        .collect();
    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start_with_paths(
        root.path(),
        SearchSessionOptions::new(SessionGeneration(3), move |_| {
            let _ = notify_tx.send(Instant::now());
        }),
        paths,
    )
    .unwrap();
    session.update(SearchMode::fuzzy("file"));
    notify_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("initial search notification");

    let started = Instant::now();
    session.join();

    assert!(
        started.elapsed() <= Duration::from_millis(500),
        "workers should join within 500 ms, took {:?}",
        started.elapsed()
    );
    assert!(
        notify_rx
            .try_iter()
            .all(|published_at| published_at <= started + Duration::from_millis(50)),
        "cancelled workers must stop publication within 50 ms"
    );
}

#[test]
fn advancing_generation_retags_reused_catalog_snapshots() {
    let root = tempdir().unwrap();
    let paths = vec!["src/main.rs".to_string(), "README.md".to_string()];
    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start_with_paths(
        root.path(),
        SearchSessionOptions::new(SessionGeneration(7), move |generation| {
            let _ = notify_tx.send(generation);
        }),
        paths,
    )
    .unwrap();
    session.update(SearchMode::fuzzy("main"));
    let _ = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        snapshot.generation == SessionGeneration(7)
            && matches!(snapshot.phase, SearchPhase::Complete)
    });

    session.set_generation(SessionGeneration(8));
    session.update(SearchMode::fuzzy("read"));
    let snapshot = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        snapshot.generation == SessionGeneration(8)
            && snapshot.mode == SearchMode::fuzzy("read")
            && matches!(snapshot.phase, SearchPhase::Complete)
    });

    assert_eq!(snapshot.generation, SessionGeneration(8));
    assert_eq!(session.generation(), SessionGeneration(8));
    session.join();
}

#[test]
fn fuzzy_ranking_is_deterministic_and_exposes_unicode_indices() {
    let root = tempdir().unwrap();
    let paths = vec![
        "b/main.rs".to_string(),
        "a/main.rs".to_string(),
        "src/main".to_string(),
        "src/main/".to_string(),
        "src/你好.rs".to_string(),
        "src/Main.rs".to_string(),
    ];
    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start_with_paths(
        root.path(),
        SearchSessionOptions::new(SessionGeneration(5), move |generation| {
            let _ = notify_tx.send(generation);
        }),
        paths,
    )
    .unwrap();

    session.update(SearchMode::fuzzy("main"));
    let first = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        matches!(snapshot.phase, SearchPhase::Complete)
    });
    let a = first
        .matches
        .iter()
        .position(|candidate| candidate.path == "a/main.rs")
        .unwrap();
    let b = first
        .matches
        .iter()
        .position(|candidate| candidate.path == "b/main.rs")
        .unwrap();
    let file = first
        .matches
        .iter()
        .position(|candidate| candidate.path == "src/main")
        .unwrap();
    let directory = first
        .matches
        .iter()
        .position(|candidate| candidate.path == "src/main/")
        .unwrap();
    assert_eq!(first.matches[a].score, first.matches[b].score);
    assert!(a < b);
    assert_eq!(first.matches[file].score, first.matches[directory].score);
    assert!(file < directory);

    session.update(SearchMode::fuzzy("你"));
    let unicode = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        snapshot.mode == SearchMode::fuzzy("你") && matches!(snapshot.phase, SearchPhase::Complete)
    });
    let chinese = unicode
        .matches
        .iter()
        .find(|candidate| candidate.path == "src/你好.rs")
        .unwrap();
    assert_eq!(chinese.indices, vec![4]);

    session.update(SearchMode::fuzzy("Main"));
    let smart_case = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        snapshot.mode == SearchMode::fuzzy("Main")
            && matches!(snapshot.phase, SearchPhase::Complete)
    });
    assert!(
        smart_case
            .matches
            .iter()
            .any(|candidate| candidate.path == "src/Main.rs")
    );
    assert!(
        smart_case
            .matches
            .iter()
            .all(|candidate| candidate.path != "src/main")
    );
    session.join();
}

#[cfg(unix)]
#[test]
fn discovery_excludes_external_symlink_candidate() {
    use std::os::unix::fs::symlink;

    let root = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("outside.rs"), "outside").unwrap();
    symlink(
        outside.path().join("outside.rs"),
        root.path().join("outside-link.rs"),
    )
    .unwrap();
    fs::write(root.path().join("inside.rs"), "inside").unwrap();

    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start(
        root.path(),
        SearchSessionOptions::new(SessionGeneration(4), move |generation| {
            let _ = notify_tx.send(generation);
        }),
    )
    .unwrap();
    session.update(SearchMode::fuzzy("rs"));

    let snapshot = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        matches!(snapshot.phase, SearchPhase::Complete)
    });

    assert!(
        snapshot
            .matches
            .iter()
            .any(|candidate| candidate.path == "inside.rs")
    );
    assert!(
        snapshot
            .matches
            .iter()
            .all(|candidate| candidate.path != "outside-link.rs")
    );
    session.join();
}

#[cfg(unix)]
#[test]
fn browsing_an_internal_directory_symlink_preserves_its_logical_prefix() {
    use std::os::unix::fs::symlink;

    let root = tempdir().unwrap();
    fs::create_dir(root.path().join("real")).unwrap();
    fs::write(root.path().join("real/inside.rs"), "inside").unwrap();
    symlink(root.path().join("real"), root.path().join("alias")).unwrap();

    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start(
        root.path(),
        SearchSessionOptions::new(SessionGeneration(6), move |generation| {
            let _ = notify_tx.send(generation);
        }),
    )
    .unwrap();
    session.update(SearchMode::browse("alias/"));

    let snapshot = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        snapshot.mode == SearchMode::browse("alias/") && !snapshot.matches.is_empty()
    });

    assert!(
        snapshot
            .matches
            .iter()
            .any(|candidate| candidate.path == "alias/inside.rs")
    );
    assert!(
        snapshot
            .matches
            .iter()
            .all(|candidate| candidate.path != "real/inside.rs")
    );
    session.join();
}
