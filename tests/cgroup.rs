use cedegrid::cgroup::{CgroupBackend, CgroupConfig, CpuMax};
use std::path::PathBuf;

fn authorized() -> CgroupConfig {
    CgroupConfig {
        enabled: true,
        delegated_root: Some(PathBuf::from("/sys/fs/cgroup/explicit-delegation")),
        authorized_controls: vec!["cgroup.procs".into(), "cpu.weight".into()],
        ..CgroupConfig::default()
    }
}

#[test]
fn defaults_keep_kernel_backend_optional_and_never_add_quota() {
    let config = CgroupConfig::default();
    config.validate().unwrap();
    assert!(!config.enabled);
    assert!(config.delegated_root.is_none());
    assert!(config.cpu_max.is_none());
    assert!(config.memory_max_mib.is_none());
    assert!(!config.allow_kill);
    assert_eq!(config.cpu_weight, Some(10));
}

#[test]
fn configuration_requires_explicit_membership_and_control_authorization() {
    authorized().validate().unwrap();
    let config = CgroupConfig {
        authorized_controls: vec![],
        ..authorized()
    };
    assert!(config.validate().is_err());
    let config = CgroupConfig {
        authorized_controls: vec!["cgroup.procs".into()],
        ..authorized()
    };
    assert!(config.validate().is_err());
    let config = CgroupConfig {
        cpu_weight: None,
        authorized_controls: vec!["cgroup.procs".into()],
        ..authorized()
    };
    config.validate().unwrap();
}

#[test]
fn global_and_traversal_paths_and_unknown_controls_are_rejected() {
    for path in [
        "/",
        "relative",
        "/authorized/../other",
        "/authorized/./other",
    ] {
        let config = CgroupConfig {
            delegated_root: Some(path.into()),
            ..authorized()
        };
        assert!(config.validate().is_err(), "{path}");
    }
    for control in [
        "../cpu.max",
        "cgroup.subtree_control",
        "cgroup.freeze",
        "global.sysctl",
    ] {
        let mut config = authorized();
        config.authorized_controls.push(control.into());
        assert!(config.validate().is_err(), "{control}");
    }
}

#[test]
fn limits_are_validated_without_writing_controls() {
    let mut config = authorized();
    config.cpu_weight = Some(0);
    assert!(config.validate().is_err());
    config.cpu_weight = Some(10_001);
    assert!(config.validate().is_err());
    config.cpu_weight = Some(10);
    config.cpu_max = Some(CpuMax {
        quota_us: 500,
        period_us: 100_000,
    });
    assert!(config.validate().is_err());
    config.cpu_max = Some(CpuMax {
        quota_us: 10_000,
        period_us: 100_000,
    });
    assert!(config.validate().is_err());
    config.authorized_controls.push("cpu.max".into());
    config.validate().unwrap();
    config.memory_max_mib = Some(u64::MAX);
    assert!(config.validate().is_err());
    config.memory_max_mib = Some(128);
    config.memory_high_mib = Some(256);
    assert!(config.validate().is_err());
}

#[test]
fn subtree_kill_requires_separate_explicit_permission() {
    let mut config = authorized();
    config.allow_kill = true;
    assert!(config.validate().is_err());
    config.authorized_controls.push("cgroup.kill".into());
    config.validate().unwrap();
}

#[test]
fn serde_rejects_unknown_fields_and_accepts_disabled_baseline() {
    let config: CgroupConfig = serde_json::from_str("{}").unwrap();
    config.validate().unwrap();
    assert!(
        serde_json::from_str::<CgroupConfig>(r#"{"enabled":false,"enable_all_controllers":true}"#)
            .is_err()
    );
}

#[test]
fn disabled_backend_cannot_execute() {
    assert!(CgroupBackend::new(CgroupConfig::default()).is_err());
}

#[cfg(not(target_os = "linux"))]
#[test]
fn non_linux_execution_is_explicitly_unavailable_not_claimed_verified() {
    let error = CgroupBackend::new(authorized()).unwrap_err().to_string();
    assert!(error.contains("unverified"));
}

#[cfg(target_os = "linux")]
#[test]
fn ordinary_filesystem_is_rejected_even_with_explicit_path() {
    let directory = tempfile::tempdir().unwrap();
    let config = CgroupConfig {
        delegated_root: Some(directory.path().to_owned()),
        ..authorized()
    };
    assert!(CgroupBackend::new(config).is_err());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

/// Never run against inferred credentials, host names, or the host's root
/// hierarchy. The ignored marker is intentional: absent authorization is NOT
/// reported as a passed Linux runtime validation.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "not tested: requires explicit authorized Linux cgroup delegation in CEDEGRID_TEST_CGROUP_ROOT"]
fn authorized_native_empty_leaf_preparation_and_cleanup() {
    use cedegrid::execution_model::{AllocationClass, LaunchBackend, LaunchRequest};
    let root = std::env::var_os("CEDEGRID_TEST_CGROUP_ROOT")
        .expect("explicit authorized delegated subtree is required");
    let config = CgroupConfig {
        delegated_root: Some(root.into()),
        ..authorized()
    };
    let mut backend = CgroupBackend::new(config).unwrap();
    let request = LaunchRequest {
        task_id: "authorized-cgroup-test".into(),
        assignment_id: "test-preparation-only".into(),
        argv: vec!["never-executed".into()],
        cwd: std::env::current_dir().unwrap(),
        env: Default::default(),
        resources: Default::default(),
        replay_safe: true,
        class: AllocationClass::Opportunistic,
        no_escape: true,
        single_process: true,
        managed_child_limit: 0,
        max_attempts: None,
        input_artifacts: vec![],
        required_controls: vec!["cpu.weight".into()],
        allow_fallback: false,
    };
    let gate = backend.prepare(&request).unwrap();
    assert!(gate.cgroup_path.as_ref().unwrap().exists());
    assert!(
        backend
            .control_evidence()
            .iter()
            .any(|e| e.control == "cpu.weight" && e.applied)
    );
    assert!(!backend.accounting().is_empty());
    backend.confirm_release().unwrap();
    assert!(!gate.cgroup_path.unwrap().exists());
}
