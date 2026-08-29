use orca_platform::fs::{PathIdentity, PathPolicy};

#[test]
fn windows_identity_is_case_insensitive_and_component_aware() {
    let root = PathIdentity::windows(r"C:\Work\Orca").expect("root");
    let child = PathIdentity::windows(r"c:/work//orca/src/./main.rs").expect("child");
    let sibling = PathIdentity::windows(r"C:\Work\Orca-old\src").expect("sibling");

    assert!(child.is_within(&root));
    assert!(!sibling.is_within(&root));
    let mixed_case = PathIdentity::windows(r"c:/work/orca").unwrap();
    assert_eq!(root, mixed_case);
    assert_eq!(root.storage_key(), mixed_case.storage_key());
    assert_eq!(
        PathIdentity::windows(r"c:/work//orca/src/./main.rs")
            .unwrap()
            .display_path(),
        std::path::PathBuf::from(r"C:\work\orca\src\main.rs")
    );
}

#[test]
fn windows_unc_and_extended_roots_share_object_identity() {
    let unc = PathIdentity::windows(r"\\server\share\repo\file.rs").expect("UNC path");
    assert_eq!(unc.root_display(), r"\\server\share");

    let extended_unc =
        PathIdentity::windows(r"\\?\UNC\server\share\repo\file.rs").expect("extended UNC");
    assert_eq!(extended_unc.root_display(), r"\\?\UNC\server\share");
    assert_eq!(
        unc, extended_unc,
        "extended UNC is the same object namespace"
    );

    let extended_drive = PathIdentity::windows(r"\\?\C:\repo\file.rs").expect("long path");
    assert_eq!(extended_drive.root_display(), r"\\?\C:\");
}

#[test]
fn windows_identity_rejects_ambiguous_or_non_absolute_paths() {
    for path in [
        r"repo\file.rs",
        r"C:relative\file.rs",
        r"C:\repo\..\secret",
        r"C:\repo\CON",
        r"C:\repo\nul.txt",
        r"C:\repo\COM1.log",
        r"C:\repo\trailing.",
        "C:\\repo\\trailing ",
        "C:\\repo\\control\u{1f}",
        r"C:\repo\file.txt:secret",
        r"\\server",
        r"\\?\GLOBALROOT\Device\HarddiskVolume1",
    ] {
        assert!(PathIdentity::windows(path).is_err(), "path={path:?}");
    }
}

#[test]
fn windows_path_policy_controls_unc_ads_and_device_namespaces() {
    let strict = PathPolicy::windows_sandbox();
    assert!(strict.identity(r"C:\repo\src\main.rs").is_ok());
    assert!(strict.identity(r"\\server\share\repo").is_err());
    assert!(strict.identity(r"C:\repo\file.txt:secret").is_err());
    assert!(strict.identity(r"\\.\PhysicalDrive0").is_err());

    let unc = strict.with_unc_paths(true);
    assert!(unc.identity(r"\\server\share\repo").is_ok());

    let ads = strict.with_alternate_data_streams(true);
    assert!(ads.identity(r"C:\repo\file.txt:secret").is_ok());

    let device = strict.with_device_namespaces(true);
    assert!(device.identity(r"\\.\PhysicalDrive0").is_ok());
}

#[cfg(windows)]
#[test]
fn windows_no_follow_finalizes_a_regular_directory_from_its_handle() {
    let temp = tempfile::tempdir().expect("tempdir");
    let verified = PathPolicy::windows_sandbox()
        .open_no_follow(temp.path())
        .expect("regular directory");
    assert!(verified.identity().root_display().starts_with(r"\\?\"));
    assert_eq!(verified.source(), temp.path());
}

#[cfg(windows)]
#[test]
fn windows_no_follow_rejects_a_real_directory_junction() {
    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("target");
    let junction = temp.path().join("junction");
    std::fs::create_dir(&target).expect("junction target");

    let output = std::process::Command::new("cmd.exe")
        .args(["/D", "/S", "/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&target)
        .output()
        .expect("invoke mklink /J");
    assert!(
        output.status.success(),
        "mklink /J failed: status={:?}, stdout={}, stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let error = PathPolicy::windows_sandbox()
        .with_unc_paths(true)
        .open_no_follow(&junction)
        .expect_err("junction must be rejected before following its target");
    assert!(error.to_string().contains("reparse-point"));
}
