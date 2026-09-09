use cedegrid::state::StorageProfile;
use cedegrid::storage_qualification::{child_main, run};
use std::path::Path;

fn home_test_parent() -> tempfile::TempDir {
    let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
    let home = Path::new(&std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    assert!(
        cwd.starts_with(home),
        "qualification tests must run inside home"
    );
    tempfile::Builder::new()
        .prefix(".qualification-test-")
        .tempdir_in(cwd)
        .unwrap()
}

#[test]
fn all_linked_profiles_preserve_exact_commits_across_owned_process_interruptions() {
    let parent = home_test_parent();
    for profile in [
        StorageProfile::WalFull,
        StorageProfile::DeleteExtra,
        StorageProfile::BurstReplayDeleteExtra,
    ] {
        let directory = parent.path().join(profile.name());
        let report = run(
            &directory,
            profile,
            Path::new(env!("CARGO_BIN_EXE_cedegrid")),
        )
        .unwrap();
        assert!(report.process_crash_compatibility_passed, "{report:#?}");
        assert!(report.cleanup_confirmed);
        assert_eq!(report.children.len(), 5);
        assert!(report.children.iter().all(|child| child.reaped));
        assert_eq!(report.checks.len(), 5);
        assert!(report.checks.iter().all(|check| check.passed));
        assert!(report.elapsed_seconds < 30.0);
        assert!(report.retained_bytes < 16 * 1024 * 1024);
        assert!(
            !report.namespace_durability_qualified,
            "process crash is not proof of namespace durability"
        );
        assert!(report.production_admission_unchanged);
        assert_eq!(report.assurance, profile.assurance());
        assert_eq!(report.assurance_detail, profile.assurance().detail());
        if profile.is_replayable() {
            assert!(report.assurance_detail.contains("no namespace barrier"));
        }
        if !report.strict_preflight.supported {
            assert!(report.strict_store_refusal.is_some());
            assert!(!directory.join("strict-store-must-not-exist").exists());
        }
        let retained: cedegrid::storage_qualification::QualificationReport =
            serde_json::from_reader(std::fs::File::open(directory.join("report.json")).unwrap())
                .unwrap();
        assert_eq!(
            retained.process_crash_compatibility_passed,
            report.process_crash_compatibility_passed
        );
    }
}

#[test]
fn refuses_reuse_without_modifying_existing_files() {
    let parent = home_test_parent();
    let directory = parent.path().join("existing");
    std::fs::create_dir(&directory).unwrap();
    let sentinel = directory.join("original");
    std::fs::write(&sentinel, b"unchanged").unwrap();
    assert!(
        run(
            &directory,
            StorageProfile::WalFull,
            Path::new(env!("CARGO_BIN_EXE_cedegrid"))
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged");
    assert_eq!(std::fs::read_dir(directory).unwrap().count(), 1);
}

#[test]
fn spawn_failure_is_failed_evidence_and_never_qualification_success() {
    let parent = home_test_parent();
    let directory = parent.path().join("missing-child");
    let report = run(
        &directory,
        StorageProfile::DeleteExtra,
        &parent.path().join("no-such-program"),
    )
    .unwrap();
    assert!(!report.process_crash_compatibility_passed);
    assert!(!report.namespace_durability_qualified);
    assert!(report.cleanup_confirmed);
    assert!(report.children.is_empty());
    assert!(report.checks.iter().any(|check| !check.passed));
    assert!(directory.join("report.json").exists());
}

#[test]
fn child_refuses_wrong_capability_before_opening_database() {
    let parent = home_test_parent();
    let directory = parent.path().join("wrong-token");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(
        directory.join("manifest.json"),
        br#"{"token":"owned-token","profile":"wal_full"}"#,
    )
    .unwrap();
    assert!(
        child_main(
            &directory,
            StorageProfile::WalFull,
            "dirty",
            "incorrect-token"
        )
        .is_err()
    );
    assert!(!directory.join("qualification.sqlite3").exists());
}

#[cfg(unix)]
#[test]
fn refuses_parent_symlink_without_writing_through_alias() {
    let parent = home_test_parent();
    let real = parent.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = parent.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    assert!(
        run(
            &alias.join("new"),
            StorageProfile::WalFull,
            Path::new(env!("CARGO_BIN_EXE_cedegrid"))
        )
        .is_err()
    );
    assert!(std::fs::read_dir(real).unwrap().next().is_none());
}
