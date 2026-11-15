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
    for control in ["../cpu.max", "cgroup.subtree_control", "global.sysctl"] {
        let mut config = authorized();
        config.authorized_controls.push(control.into());
        assert!(config.validate().is_err(), "{control}");
    }
    // Pause writes cgroup.freeze at runtime, so an operator may authorize it.
    let mut config = authorized();
    config.authorized_controls.push("cgroup.freeze".into());
    config.validate().unwrap();
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
        contention: None,
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

/// T138 native gate: a RAM cap step writes `memory.high` on the test-owned leaf
/// this backend created, reads it back, and reversal restores `max`. Requires an
/// authorized delegated subtree with the memory controller enabled.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "not tested: requires explicit authorized Linux cgroup delegation in CEDEGRID_TEST_CGROUP_ROOT"]
fn authorized_native_ram_cap_reads_back_memory_high() {
    use cedegrid::contention::{ResourceKind, YieldStep};
    use cedegrid::execution_model::{
        ActuatorSettings, AllocationClass, Family, LaunchBackend, LaunchRequest, ProcessIdentity,
    };
    use std::os::unix::process::CommandExt;
    struct Child(u32);
    impl Family for Child {
        fn identities(&self) -> Vec<ProcessIdentity> {
            vec![ProcessIdentity {
                pid: self.0,
                boot_id: "native".into(),
                start_time: 1,
                assignment_id: "test-ram-cap".into(),
                generation: 1,
            }]
        }
        fn signal(&mut self, _signal: i32) -> anyhow::Result<()> {
            anyhow::bail!("the RAM cap never signals")
        }
        fn cpu_time(&mut self) -> Option<std::time::Duration> {
            None
        }
    }
    let root = std::env::var_os("CEDEGRID_TEST_CGROUP_ROOT")
        .expect("explicit authorized delegated subtree is required");
    let config = CgroupConfig {
        delegated_root: Some(root.into()),
        cpu_weight: None,
        authorized_controls: vec!["cgroup.procs".into(), "memory.high".into()],
        ..authorized()
    };
    let mut backend = CgroupBackend::new(config).unwrap();
    let request = LaunchRequest {
        task_id: "authorized-ram-cap-test".into(),
        assignment_id: "test-ram-cap".into(),
        argv: vec!["sleep".into()],
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
        required_controls: vec![],
        allow_fallback: false,
        contention: None,
    };
    let leaf = backend.prepare(&request).unwrap().cgroup_path.unwrap();
    let procs = leaf.join("cgroup.procs");
    // The test-owned child joins the new leaf before it runs anything.
    // SAFETY: the pre-exec closure only writes one file before exec.
    let mut child = unsafe {
        std::process::Command::new("sleep")
            .arg("30")
            .pre_exec(move || std::fs::write(&procs, "0"))
            .spawn()
            .unwrap()
    };
    let mut family = Child(child.id());
    let mut actuator = backend.actuator(&ActuatorSettings {
        class: AllocationClass::Opportunistic,
        requested: Default::default(),
        period: std::time::Duration::from_millis(500),
        ram_noise_tolerance_mib: 256,
        ram_calm_window: std::time::Duration::from_millis(1500),
    });
    let read = || std::fs::read_to_string(leaf.join("memory.high")).unwrap();
    let applied = actuator.apply(&mut family, ResourceKind::Ram, YieldStep::Cap, Some(1024));
    let capped = read();
    let reversed = actuator.apply(&mut family, ResourceKind::Ram, YieldStep::None, None);
    let restored = read();
    child.kill().unwrap();
    child.wait().unwrap();
    backend.confirm_release().unwrap();
    assert_eq!(applied.unwrap(), YieldStep::Cap);
    let mib = actuator.applied_target(ResourceKind::Ram);
    assert!(mib.is_none(), "reversal clears the cap");
    assert!(
        capped.trim().parse::<u64>().unwrap() >= 1024 * 1024 * 1024,
        "{capped}"
    );
    assert_eq!(reversed.unwrap(), YieldStep::None);
    assert_eq!(restored.trim(), "max");
}

/// T054 native gate (quickstart section 4): on a test-owned leaf in an authorized
/// delegated subtree, runtime `cpu.weight`, `cpu.max` and `cgroup.freeze` steps read
/// back and reverse, and a step is refused while the leaf holds a process outside
/// the verified set. It skips without `CEDEGRID_TEST_CGROUP_ROOT` and off Linux.
#[test]
#[ignore = "not tested: requires explicit authorized Linux cgroup delegation in CEDEGRID_TEST_CGROUP_ROOT"]
fn runtime_adjust_requires_verified_membership() {
    let Some(root) = std::env::var_os("CEDEGRID_TEST_CGROUP_ROOT") else {
        eprintln!("skipped: CEDEGRID_TEST_CGROUP_ROOT names no authorized delegated subtree");
        return;
    };
    #[cfg(target_os = "linux")]
    native_runtime_adjust(root.into());
    #[cfg(not(target_os = "linux"))]
    {
        let _ = root;
        eprintln!("skipped: cgroup v2 actuation needs Linux");
    }
}

#[cfg(target_os = "linux")]
fn native_runtime_adjust(root: PathBuf) {
    use cedegrid::contention::{ResourceKind, YieldStep};
    use cedegrid::execution_model::{
        ActuatorSettings, AllocationClass, Family, LaunchBackend, LaunchRequest, ProcessIdentity,
    };
    use std::os::unix::process::CommandExt;
    struct Verified(u32);
    impl Family for Verified {
        fn identities(&self) -> Vec<ProcessIdentity> {
            vec![ProcessIdentity {
                pid: self.0,
                boot_id: "native".into(),
                start_time: 1,
                assignment_id: "test-runtime-adjust".into(),
                generation: 1,
            }]
        }
        fn signal(&mut self, _signal: i32) -> anyhow::Result<()> {
            anyhow::bail!("cgroup steps never signal")
        }
        fn cpu_time(&mut self) -> Option<std::time::Duration> {
            None
        }
    }
    let config = CgroupConfig {
        delegated_root: Some(root),
        authorized_controls: vec![
            "cgroup.procs".into(),
            "cpu.weight".into(),
            "cpu.max".into(),
            "cgroup.freeze".into(),
        ],
        ..authorized()
    };
    let mut backend = CgroupBackend::new(config).unwrap();
    let request = LaunchRequest {
        task_id: "authorized-runtime-adjust-test".into(),
        assignment_id: "test-runtime-adjust".into(),
        argv: vec!["sleep".into()],
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
        required_controls: vec![],
        allow_fallback: false,
        contention: None,
    };
    let leaf = backend.prepare(&request).unwrap().cgroup_path.unwrap();
    // Each test-owned child joins the leaf before it runs anything.
    let join = |leaf: &std::path::Path| {
        let procs = leaf.join("cgroup.procs");
        // SAFETY: the pre-exec closure only writes one file before exec.
        unsafe {
            std::process::Command::new("sleep")
                .arg("30")
                .pre_exec(move || std::fs::write(&procs, "0"))
                .spawn()
                .unwrap()
        }
    };
    let read = |control: &str| {
        std::fs::read_to_string(leaf.join(control))
            .unwrap()
            .trim()
            .to_owned()
    };
    let mut member = join(&leaf);
    let mut family = Verified(member.id());
    let mut actuator = backend.actuator(&ActuatorSettings {
        class: AllocationClass::Opportunistic,
        requested: cedegrid::model::Resources {
            cpu_millicores: 1000,
            ..Default::default()
        },
        period: std::time::Duration::from_millis(500),
        ram_noise_tolerance_mib: 256,
        ram_calm_window: std::time::Duration::from_millis(1500),
    });
    let (weight, cpu_max) = (read("cpu.weight"), read("cpu.max"));
    let shared = actuator.apply(&mut family, ResourceKind::Cpu, YieldStep::LowerShare, None);
    let lowered = read("cpu.weight");
    let capped_step = actuator.apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(500));
    let capped = read("cpu.max");
    let paused_step = actuator.apply(&mut family, ResourceKind::Cpu, YieldStep::Pause, None);
    let frozen = (read("cgroup.freeze"), read("cgroup.events"));
    let reversed = actuator.apply(&mut family, ResourceKind::Cpu, YieldStep::None, None);
    let restored = (read("cpu.weight"), read("cpu.max"), read("cgroup.freeze"));
    // A process outside the verified set in the leaf refuses every cgroup write. A
    // lower share has no rootless fallback, so it fails; a cap may fall back to the
    // rootless duty cycle on the verified family, but the leaf is never written.
    let mut foreign = join(&leaf);
    let refused_share = actuator.apply(&mut family, ResourceKind::Cpu, YieldStep::LowerShare, None);
    let share_untouched = read("cpu.weight");
    let refused_cap = actuator.apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(500));
    let cap_detail = actuator.take_detail().unwrap_or_default();
    let untouched = read("cpu.max");
    for child in [&mut member, &mut foreign] {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    backend.confirm_release().unwrap();
    assert_eq!(shared.unwrap(), YieldStep::LowerShare);
    assert_eq!(lowered, "1");
    assert_eq!(capped_step.unwrap(), YieldStep::Cap);
    assert_ne!(capped, cpu_max, "the cap reads back");
    assert!(
        capped
            .split_whitespace()
            .next()
            .unwrap()
            .parse::<u64>()
            .is_ok(),
        "{capped}"
    );
    assert_eq!(paused_step.unwrap(), YieldStep::Pause);
    assert_eq!(frozen.0, "1");
    assert!(
        frozen.1.lines().any(|line| line == "frozen 1"),
        "{}",
        frozen.1
    );
    assert_eq!(reversed.unwrap(), YieldStep::None);
    assert_eq!(restored, (weight, cpu_max.clone(), "0".to_owned()));
    assert!(
        refused_share.is_err(),
        "a lower share went through with a foreign member in the leaf"
    );
    assert_eq!(
        share_untouched, restored.0,
        "a refused share changed the leaf"
    );
    assert!(
        refused_cap.is_err() || cap_detail.contains("cgroup write refused"),
        "a cap reached the leaf with a foreign member in it: {cap_detail}"
    );
    assert_eq!(untouched, cpu_max, "a refused cap changed the leaf");
}

#[cfg(unix)]
mod runtime_steps {
    use cedegrid::cgroup::{
        CgroupActuator, CgroupConfig, DirectoryLeaf, LeafIo, Limit, runtime_write,
    };
    use cedegrid::contention::{ResourceKind, YieldStep};
    use cedegrid::execution_model::{
        Actuator, ActuatorSettings, AllocationClass, Family, ProcessIdentity,
    };
    use cedegrid::model::Resources;
    use std::{collections::BTreeSet, fs, path::Path, time::Duration};

    fn leaf(dir: &Path, members: &str) {
        for (name, value) in [
            ("cgroup.procs", members),
            ("cpu.weight", "10\n"),
            ("cpu.max", "max 100000\n"),
            ("cgroup.freeze", "0\n"),
            ("cgroup.events", "populated 1\nfrozen 0\n"),
        ] {
            fs::write(dir.join(name), value).unwrap();
        }
    }

    fn read(dir: &Path, name: &str) -> String {
        fs::read_to_string(dir.join(name))
            .unwrap()
            .trim()
            .to_owned()
    }

    /// A managed child joins the leaf beside the leader and the children verified before it,
    /// and nothing else.
    #[test]
    fn a_managed_child_is_verified_beside_the_leader_and_verified_children() {
        use cedegrid::cgroup::check_child_membership;
        let family = BTreeSet::from([11, 12]);
        assert!(check_child_membership(&BTreeSet::from([11, 12, 13]), &family, 13).is_ok());
        // Children that have left the leaf need not be in it.
        assert!(check_child_membership(&BTreeSet::from([11, 13]), &family, 13).is_ok());
        assert!(check_child_membership(&BTreeSet::from([11, 12, 13, 14]), &family, 13).is_err());
        assert!(check_child_membership(&BTreeSet::from([11, 12]), &family, 13).is_err());
    }

    #[test]
    fn limits_round_trip() {
        assert_eq!("max".parse::<Limit>().unwrap(), Limit::Max);
        assert_eq!("12345".parse::<Limit>().unwrap(), Limit::Value(12345));
        assert_eq!(Limit::Max.to_string(), "max");
        assert_eq!(Limit::Value(7).to_string(), "7");
        assert!("-1".parse::<Limit>().is_err());
    }

    #[test]
    fn runtime_writes_need_the_same_leaf_and_exactly_the_verified_members() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("leaf");
        fs::create_dir(&dir).unwrap();
        leaf(&dir, "11\n12\n");
        let io = DirectoryLeaf::open(&dir).unwrap();
        // A foreign PID in the leaf refuses the write.
        assert!(runtime_write(&io, &BTreeSet::from([11]), "cpu.weight", "1").is_err());
        assert_eq!(read(&dir, "cpu.weight"), "10");
        assert_eq!(
            runtime_write(&io, &BTreeSet::from([11, 12]), "cpu.weight", "1").unwrap(),
            "1"
        );
        // A replaced leaf directory refuses the write as well.
        fs::rename(&dir, temp.path().join("old")).unwrap();
        fs::create_dir(&dir).unwrap();
        leaf(&dir, "11\n12\n");
        assert!(!io.same_leaf().unwrap());
        assert!(runtime_write(&io, &BTreeSet::from([11, 12]), "cpu.weight", "1").is_err());
        assert_eq!(read(&dir, "cpu.weight"), "10");
    }

    struct Recorder {
        pids: Vec<u32>,
        signals: Vec<i32>,
    }
    impl Family for Recorder {
        fn identities(&self) -> Vec<ProcessIdentity> {
            self.pids
                .iter()
                .map(|pid| ProcessIdentity {
                    pid: *pid,
                    boot_id: "fixture".into(),
                    start_time: 1,
                    assignment_id: "a".into(),
                    generation: 1,
                })
                .collect()
        }
        fn signal(&mut self, signal: i32) -> anyhow::Result<()> {
            self.signals.push(signal);
            Ok(())
        }
        fn cpu_time(&mut self) -> Option<Duration> {
            None
        }
    }

    fn settings(class: AllocationClass) -> ActuatorSettings {
        ActuatorSettings {
            class,
            requested: Resources {
                cpu_millicores: 1000,
                ..Resources::default()
            },
            period: Duration::from_millis(100),
            ram_noise_tolerance_mib: 256,
            ram_calm_window: Duration::from_millis(30),
        }
    }

    fn config(controls: &[&str]) -> CgroupConfig {
        CgroupConfig {
            authorized_controls: controls.iter().map(|c| c.to_string()).collect(),
            ..CgroupConfig::default()
        }
    }

    #[test]
    fn delegated_controls_apply_read_back_and_restore() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "21\n");
        // The fixture reports the freeze as soon as it is requested.
        fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
        let mut actuator = CgroupActuator::new(
            DirectoryLeaf::open(&dir).unwrap(),
            &config(&["cgroup.procs", "cpu.weight", "cpu.max", "cgroup.freeze"]),
            &settings(AllocationClass::Opportunistic),
        );
        let mut family = Recorder {
            pids: vec![21],
            signals: vec![],
        };
        assert!(
            !actuator
                .unavailable(ResourceKind::Cpu)
                .contains(&YieldStep::LowerShare)
        );
        let step = actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::LowerShare, None)
            .unwrap();
        assert_eq!(
            (step, read(&dir, "cpu.weight")),
            (YieldStep::LowerShare, "1".into())
        );
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(250))
            .unwrap();
        assert_eq!(read(&dir, "cpu.max"), "25000 100000");
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Pause, None)
            .unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "1");
        // Reversal and release restore the launch settings; no signal was needed.
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::None, None)
            .unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "0");
        assert_eq!(read(&dir, "cpu.max"), "max 100000");
        assert_eq!(read(&dir, "cpu.weight"), "10");
        actuator.release(&mut family).unwrap();
        assert!(family.signals.is_empty());
    }

    #[test]
    fn unauthorized_controls_fall_back_to_owned_signals_and_guaranteed_is_never_frozen() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "31\n");
        let mut actuator = CgroupActuator::new(
            DirectoryLeaf::open(&dir).unwrap(),
            &config(&["cgroup.procs", "cpu.weight"]),
            &settings(AllocationClass::Opportunistic),
        );
        let mut family = Recorder {
            pids: vec![31],
            signals: vec![],
        };
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Pause, None)
            .unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "0", "no unauthorized write");
        assert_eq!(family.signals, [libc::SIGSTOP]);
        actuator.release(&mut family).unwrap();
        assert_eq!(family.signals, [libc::SIGSTOP, libc::SIGCONT]);
        let guaranteed = CgroupActuator::new(
            DirectoryLeaf::open(&dir).unwrap(),
            &config(&["cgroup.procs", "cpu.weight", "cgroup.freeze"]),
            &settings(AllocationClass::Guaranteed),
        );
        assert!(
            guaranteed
                .unavailable(ResourceKind::Cpu)
                .contains(&YieldStep::Pause)
        );
        for memory in [ResourceKind::Ram, ResourceKind::GpuMemory] {
            let steps = guaranteed.unavailable(memory);
            assert!(
                [YieldStep::LowerShare, YieldStep::Cap, YieldStep::Pause]
                    .iter()
                    .all(|s| steps.contains(s))
            );
        }
    }

    fn all_controls() -> CgroupConfig {
        config(&["cgroup.procs", "cpu.weight", "cpu.max", "cgroup.freeze"])
    }

    #[test]
    fn a_refused_write_falls_back_to_owned_signals_and_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        // PID 99 is not in the verified family, so every cgroup write is refused.
        leaf(&dir, "41\n99\n");
        let mut actuator = CgroupActuator::new(
            DirectoryLeaf::open(&dir).unwrap(),
            &all_controls(),
            &settings(AllocationClass::Opportunistic),
        );
        let mut family = Recorder {
            pids: vec![41],
            signals: vec![],
        };
        let step = actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(250))
            .unwrap();
        assert_eq!(step, YieldStep::Cap);
        assert_eq!(read(&dir, "cpu.max"), "max 100000");
        assert!(actuator.take_detail().unwrap().contains("membership"));
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Pause, None)
            .unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "0");
        assert_eq!(family.signals, [libc::SIGSTOP]);
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::None, None)
            .unwrap();
        assert_eq!(family.signals, [libc::SIGSTOP, libc::SIGCONT]);
        // LowerShare has no rootless mechanism, so its refusal is an error.
        assert!(
            actuator
                .apply(&mut family, ResourceKind::Cpu, YieldStep::LowerShare, None)
                .is_err()
        );
        assert_eq!(read(&dir, "cpu.weight"), "10");
    }

    /// Undoing a cap this actuator wrote needs only its leaf, whatever its members: lifting acts
    /// on nothing CedeGrid did not restrict itself. A new restriction on that leaf is still
    /// refused and moves to the rootless mechanism, which reaches only the verified family
    /// (FR-078, Principle I; the undo used to be refused too).
    #[test]
    fn a_restore_on_a_joined_leaf_needs_only_the_leaf_and_a_new_cap_still_verifies() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "51\n");
        let mut actuator = CgroupActuator::new(
            DirectoryLeaf::open(&dir).unwrap(),
            &all_controls(),
            &settings(AllocationClass::Opportunistic),
        );
        let mut family = Recorder {
            pids: vec![51],
            signals: vec![],
        };
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(250))
            .unwrap();
        fs::write(dir.join("cgroup.procs"), "51\n52\n").unwrap();
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::None, None)
            .unwrap();
        assert_eq!(read(&dir, "cpu.max"), "max 100000");
        assert!(family.signals.is_empty());
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(100))
            .unwrap();
        assert_eq!(
            read(&dir, "cpu.max"),
            "max 100000",
            "the new cap is refused"
        );
        assert!(
            actuator
                .take_detail()
                .is_some_and(|detail| detail.contains("cgroup write refused")),
            "and moves to the rootless duty"
        );
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Pause, None)
            .unwrap();
        assert_eq!(
            read(&dir, "cgroup.freeze"),
            "0",
            "a new freeze is refused too"
        );
        assert!(
            actuator
                .apply(&mut family, ResourceKind::Cpu, YieldStep::LowerShare, None)
                .is_err(),
            "a lower share has no rootless mechanism"
        );
        assert_eq!(read(&dir, "cpu.weight"), "10");
    }

    #[test]
    fn a_shared_freeze_and_the_launch_quota_hold() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "61\n");
        fs::write(dir.join("cpu.max"), "20000 100000\n").unwrap();
        fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
        let mut config = all_controls();
        config.cpu_max = Some(cedegrid::cgroup::CpuMax {
            quota_us: 20_000,
            period_us: 100_000,
        });
        let mut actuator = CgroupActuator::new(
            DirectoryLeaf::open(&dir).unwrap(),
            &config,
            &settings(AllocationClass::Opportunistic),
        );
        let mut family = Recorder {
            pids: vec![61],
            signals: vec![],
        };
        // A cap never raises the launch-time quota.
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(500))
            .unwrap();
        assert_eq!(read(&dir, "cpu.max"), "20000 100000");
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Pause, None)
            .unwrap();
        actuator
            .apply(
                &mut family,
                ResourceKind::GpuCompute,
                YieldStep::Pause,
                None,
            )
            .unwrap();
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::None, None)
            .unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "1", "GPU compute still paused");
        actuator
            .apply(&mut family, ResourceKind::GpuCompute, YieldStep::None, None)
            .unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "0");
        assert_eq!(read(&dir, "cpu.max"), "20000 100000");
        assert!(family.signals.is_empty());
    }

    /// A family that records when each signal was sent.
    struct Timed {
        pids: Vec<u32>,
        signals: Vec<(i32, std::time::Instant)>,
    }
    impl Family for Timed {
        fn identities(&self) -> Vec<ProcessIdentity> {
            Recorder {
                pids: self.pids.clone(),
                signals: vec![],
            }
            .identities()
        }
        fn signal(&mut self, signal: i32) -> anyhow::Result<()> {
            self.signals.push((signal, std::time::Instant::now()));
            Ok(())
        }
        fn cpu_time(&mut self) -> Option<Duration> {
            None
        }
    }

    fn timed(pid: u32) -> Timed {
        Timed {
            pids: vec![pid],
            signals: vec![],
        }
    }

    /// Ticks the actuator about every millisecond for `periods` duty periods of 100 ms and
    /// returns the share of that time the leaf was frozen and the family stopped.
    fn off_shares(
        actuator: &mut dyn Actuator,
        family: &mut Timed,
        dir: &Path,
        periods: u64,
    ) -> (f64, f64) {
        let start = std::time::Instant::now();
        let end = start + Duration::from_millis(100 * periods);
        let (mut frozen, mut last) = (Duration::ZERO, start);
        let mut was_frozen = read(dir, "cgroup.freeze") == "1";
        while std::time::Instant::now() < end {
            actuator.tick(family).unwrap();
            // The leaf held the state it had after the previous tick until this one.
            let now = std::time::Instant::now();
            if was_frozen {
                frozen += now - last;
            }
            was_frozen = read(dir, "cgroup.freeze") == "1";
            last = now;
            std::thread::sleep(Duration::from_millis(1));
        }
        let (mut stopped, mut since) = (Duration::ZERO, None);
        for (signal, at) in &family.signals {
            match *signal {
                libc::SIGSTOP => since = Some((*at).max(start)),
                libc::SIGCONT => {
                    if let Some(from) = since.take() {
                        stopped += at.saturating_duration_since(from);
                    }
                }
                _ => {}
            }
        }
        if let Some(from) = since {
            stopped += last.saturating_duration_since(from);
        }
        let total = (last - start).as_secs_f64();
        (frozen.as_secs_f64() / total, stopped.as_secs_f64() / total)
    }

    /// Full offers Opportunistic work the GPU compute share Tracked offers, with
    /// `cgroup.freeze` authorized or not, and Guaranteed work no GPU compute step
    /// (FR-024). For every resource, Full offers each step Tracked offers.
    #[test]
    fn full_offers_every_step_tracked_offers() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "71\n");
        let all = [
            "cgroup.procs",
            "cpu.weight",
            "cpu.max",
            "cgroup.freeze",
            "memory.high",
        ];
        for class in [AllocationClass::Opportunistic, AllocationClass::Guaranteed] {
            let tracked = cedegrid::actuation::RootlessActuator::new(&settings(class));
            for controls in [&all[..], &["cgroup.procs"][..]] {
                let full = CgroupActuator::new(
                    DirectoryLeaf::open(&dir).unwrap(),
                    &config(controls),
                    &settings(class),
                );
                for kind in [
                    ResourceKind::Cpu,
                    ResourceKind::GpuCompute,
                    ResourceKind::Ram,
                    ResourceKind::GpuMemory,
                ] {
                    for step in [
                        YieldStep::LowerShare,
                        YieldStep::Cap,
                        YieldStep::Pause,
                        YieldStep::Hint,
                    ] {
                        assert!(
                            tracked.unavailable(kind).contains(&step)
                                || !full.unavailable(kind).contains(&step),
                            "{class:?} {kind:?} {step:?} with {controls:?}"
                        );
                    }
                }
                let gpu = full.unavailable(ResourceKind::GpuCompute);
                match class {
                    AllocationClass::Guaranteed => assert!(
                        gpu.contains(&YieldStep::Cap) && gpu.contains(&YieldStep::Pause),
                        "{controls:?}"
                    ),
                    _ => assert!(!gpu.contains(&YieldStep::Cap), "{controls:?}"),
                }
            }
        }
    }

    /// In Full a GPU compute cap is a duty cycle at the target's share of each period
    /// (research R27). With `cgroup.freeze` authorized its off phase freezes the leaf,
    /// confirmed through `cgroup.events`, and sends no signal; otherwise it stops the
    /// verified family. A duty starts in its on phase.
    #[test]
    fn a_gpu_compute_cap_in_full_is_a_duty_cycle() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "72\n");
        fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
        let mut actuator = CgroupActuator::new(
            DirectoryLeaf::open(&dir).unwrap(),
            &all_controls(),
            &settings(AllocationClass::Opportunistic),
        );
        let mut family = timed(72);
        let step = actuator
            .apply(
                &mut family,
                ResourceKind::GpuCompute,
                YieldStep::Cap,
                Some(500),
            )
            .unwrap();
        assert_eq!(step, YieldStep::Cap);
        assert_eq!(read(&dir, "cgroup.freeze"), "0", "the duty starts running");
        assert!(actuator.next_deadline().is_some());
        let (frozen, _) = off_shares(&mut actuator, &mut family, &dir, 10);
        assert!((frozen - 0.5).abs() < 0.05, "frozen {frozen}");
        assert!(family.signals.is_empty());
        actuator
            .apply(&mut family, ResourceKind::GpuCompute, YieldStep::None, None)
            .unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "0");
        assert_eq!(actuator.next_deadline(), None);

        let mut rootless = CgroupActuator::new(
            DirectoryLeaf::open(&dir).unwrap(),
            &config(&["cgroup.procs", "cpu.weight", "cpu.max"]),
            &settings(AllocationClass::Opportunistic),
        );
        let mut family = timed(72);
        rootless
            .apply(
                &mut family,
                ResourceKind::GpuCompute,
                YieldStep::Cap,
                Some(500),
            )
            .unwrap();
        let (frozen, stopped) = off_shares(&mut rootless, &mut family, &dir, 10);
        assert_eq!(frozen, 0.0, "no unauthorized write");
        assert!((stopped - 0.5).abs() < 0.05, "stopped {stopped}");
        rootless.release(&mut family).unwrap();
        assert_eq!(family.signals.last().map(|s| s.0), Some(libc::SIGCONT));
    }

    /// Two leaves whose GPU compute caps began 37 ms apart freeze and thaw together, their
    /// freeze duties following the node clock modulo the period (FR-021, Q31).
    #[test]
    fn freeze_duties_capped_apart_freeze_and_thaw_together() {
        let temp = tempfile::tempdir().unwrap();
        let (mut actuators, mut families, mut dirs) = (vec![], vec![], vec![]);
        for (index, pid) in [(0, 81), (1, 82)] {
            let dir = temp.path().join(format!("leaf{index}"));
            fs::create_dir(&dir).unwrap();
            leaf(&dir, &format!("{pid}\n"));
            fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
            let mut actuator = CgroupActuator::new(
                DirectoryLeaf::open(&dir).unwrap(),
                &all_controls(),
                &settings(AllocationClass::Opportunistic),
            );
            let mut family = timed(pid);
            actuator
                .apply(
                    &mut family,
                    ResourceKind::GpuCompute,
                    YieldStep::Cap,
                    Some(500),
                )
                .unwrap();
            actuators.push(actuator);
            families.push(family);
            dirs.push(dir);
            std::thread::sleep(Duration::from_millis(37));
        }
        let end = std::time::Instant::now() + Duration::from_millis(1000);
        let (mut samples, mut agree, mut frozen) = (0, 0, 0);
        while std::time::Instant::now() < end {
            for (actuator, family) in actuators.iter_mut().zip(families.iter_mut()) {
                actuator.tick(family).unwrap();
            }
            let states: Vec<bool> = dirs
                .iter()
                .map(|dir| read(dir, "cgroup.freeze") == "1")
                .collect();
            samples += 1;
            agree += usize::from(states[0] == states[1]);
            frozen += usize::from(states[0]);
            std::thread::sleep(Duration::from_millis(1));
        }
        let (agree, frozen) = (
            agree as f64 / samples as f64,
            frozen as f64 / samples as f64,
        );
        assert!(
            agree >= 0.95,
            "the leaves agreed in {agree:.2} of {samples} samples"
        );
        assert!((frozen - 0.5).abs() < 0.08, "frozen {frozen:.2}");
    }

    /// A refused duty freeze, or a leaf that does not report frozen within the off
    /// phase, falls back to the rootless duty and keeps why as the step's detail.
    #[test]
    fn a_refused_duty_freeze_falls_back_to_the_rootless_duty() {
        let temp = tempfile::tempdir().unwrap();
        for (members, events, why) in [
            ("75\n99\n", "populated 1\nfrozen 1\n", "membership"),
            ("75\n", "populated 1\nfrozen 0\n", "frozen"),
        ] {
            let dir = temp.path().join(why);
            fs::create_dir(&dir).unwrap();
            leaf(&dir, members);
            fs::write(dir.join("cgroup.events"), events).unwrap();
            let mut actuator = CgroupActuator::new(
                DirectoryLeaf::open(&dir).unwrap(),
                &all_controls(),
                &settings(AllocationClass::Opportunistic),
            );
            let mut family = timed(75);
            actuator
                .apply(
                    &mut family,
                    ResourceKind::GpuCompute,
                    YieldStep::Cap,
                    Some(500),
                )
                .unwrap();
            let (_, stopped) = off_shares(&mut actuator, &mut family, &dir, 4);
            assert_eq!(read(&dir, "cgroup.freeze"), "0", "{why}");
            assert!(stopped > 0.25, "{why}: stopped {stopped}");
            let detail = actuator.take_detail().unwrap_or_default();
            assert!(
                detail.contains("rootless") && detail.contains(why),
                "{why}: {detail}"
            );
        }
    }

    /// Beside a `cpu.max` cap the GPU duty runs on its own, and reversing either step
    /// leaves the other in place.
    #[test]
    fn a_cpu_max_cap_and_a_gpu_duty_hold_together() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "77\n");
        fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
        let mut actuator = CgroupActuator::new(
            DirectoryLeaf::open(&dir).unwrap(),
            &all_controls(),
            &settings(AllocationClass::Opportunistic),
        );
        let mut family = timed(77);
        let gpu = |actuator: &mut CgroupActuator<DirectoryLeaf>, family: &mut Timed, step| {
            actuator
                .apply(family, ResourceKind::GpuCompute, step, Some(500))
                .unwrap()
        };
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(250))
            .unwrap();
        gpu(&mut actuator, &mut family, YieldStep::Cap);
        let (frozen, _) = off_shares(&mut actuator, &mut family, &dir, 10);
        assert!((frozen - 0.5).abs() < 0.05, "frozen {frozen}");
        assert_eq!(read(&dir, "cpu.max"), "25000 100000");
        gpu(&mut actuator, &mut family, YieldStep::None);
        assert_eq!(
            (read(&dir, "cpu.max"), read(&dir, "cgroup.freeze")),
            ("25000 100000".into(), "0".into())
        );
        gpu(&mut actuator, &mut family, YieldStep::Cap);
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::None, None)
            .unwrap();
        assert_eq!(read(&dir, "cpu.max"), "max 100000");
        let (frozen, _) = off_shares(&mut actuator, &mut family, &dir, 10);
        assert!((frozen - 0.5).abs() < 0.05, "frozen {frozen}");
        actuator.release(&mut family).unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "0");
        assert!(family.signals.is_empty());
    }

    /// A Pause holds the leaf frozen through the GPU duty's on phases; reversing it
    /// resumes the cycle. A duty that starts under a Pause leaves the freeze to it, and
    /// its reversal or `release` thaws only a freeze the duty alone holds.
    #[test]
    fn a_gpu_duty_never_thaws_a_paused_leaf() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "78\n");
        fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
        let mut actuator = CgroupActuator::new(
            DirectoryLeaf::open(&dir).unwrap(),
            &all_controls(),
            &settings(AllocationClass::Opportunistic),
        );
        let mut family = timed(78);
        actuator
            .apply(
                &mut family,
                ResourceKind::GpuCompute,
                YieldStep::Cap,
                Some(500),
            )
            .unwrap();
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Pause, None)
            .unwrap();
        let (frozen, _) = off_shares(&mut actuator, &mut family, &dir, 3);
        assert!(frozen > 0.97, "frozen {frozen}");
        // The duty waits; the only deadline is the frozen leaf's member check (T510).
        assert!(
            actuator
                .next_deadline()
                .is_some_and(|at| at <= std::time::Instant::now() + Duration::from_millis(60)),
            "the cycle waits"
        );
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::None, None)
            .unwrap();
        let (frozen, _) = off_shares(&mut actuator, &mut family, &dir, 10);
        assert!((frozen - 0.5).abs() < 0.05, "frozen {frozen}");
        actuator
            .apply(&mut family, ResourceKind::GpuCompute, YieldStep::None, None)
            .unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "0");

        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Pause, None)
            .unwrap();
        actuator
            .apply(
                &mut family,
                ResourceKind::GpuCompute,
                YieldStep::Cap,
                Some(500),
            )
            .unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "1", "the Pause holds");
        actuator
            .apply(&mut family, ResourceKind::GpuCompute, YieldStep::None, None)
            .unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "1", "the Pause holds");
        actuator.release(&mut family).unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "0");
        assert!(family.signals.is_empty());
    }

    /// Where the CPU cap is the rootless duty, the GPU share joins that cycle at the
    /// smaller duty instead of freezing on a second one, in either order, and reversing
    /// either step leaves the other.
    #[test]
    fn a_rootless_cpu_duty_and_a_gpu_share_run_one_cycle() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "79\n");
        fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
        for gpu_first in [false, true] {
            let mut actuator = CgroupActuator::new(
                DirectoryLeaf::open(&dir).unwrap(),
                &config(&["cgroup.procs", "cpu.weight", "cgroup.freeze"]),
                &settings(AllocationClass::Opportunistic),
            );
            let mut family = timed(79);
            let mut steps = [
                (ResourceKind::Cpu, Some(750)),
                (ResourceKind::GpuCompute, Some(250)),
            ];
            if gpu_first {
                steps.reverse();
            }
            for (kind, target) in steps {
                actuator
                    .apply(&mut family, kind, YieldStep::Cap, target)
                    .unwrap();
            }
            let (frozen, stopped) = off_shares(&mut actuator, &mut family, &dir, 10);
            assert_eq!(frozen, 0.0, "gpu first {gpu_first}");
            assert!(
                (stopped - 0.75).abs() < 0.05,
                "gpu first {gpu_first}: {stopped}"
            );
            let reversed = if gpu_first {
                ResourceKind::GpuCompute
            } else {
                ResourceKind::Cpu
            };
            actuator
                .apply(&mut family, reversed, YieldStep::None, None)
                .unwrap();
            let (frozen, stopped) = off_shares(&mut actuator, &mut family, &dir, 10);
            let left = if gpu_first { 0.25 } else { 0.75 };
            assert_eq!(frozen, 0.0, "gpu first {gpu_first}");
            assert!(
                (stopped - left).abs() < 0.05,
                "gpu first {gpu_first}: {stopped}"
            );
            actuator.release(&mut family).unwrap();
        }
    }

    /// Full and Tracked report the same state for a GPU compute cap, so the policy
    /// credits the applied duty alike (`applied_duty_credit`).
    #[test]
    fn full_and_tracked_report_a_gpu_compute_cap_alike() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "76\n");
        fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
        let mut full = CgroupActuator::new(
            DirectoryLeaf::open(&dir).unwrap(),
            &all_controls(),
            &settings(AllocationClass::Opportunistic),
        );
        let mut tracked =
            cedegrid::actuation::RootlessActuator::new(&settings(AllocationClass::Opportunistic));
        let mut reports = vec![];
        for actuator in [&mut full as &mut dyn Actuator, &mut tracked] {
            let mut family = timed(76);
            let unavailable = actuator.unavailable(ResourceKind::GpuCompute);
            let step = actuator
                .apply(
                    &mut family,
                    ResourceKind::GpuCompute,
                    YieldStep::Cap,
                    Some(500),
                )
                .unwrap();
            reports.push((
                unavailable,
                step,
                actuator.applied_target(ResourceKind::GpuCompute),
                actuator.step_code(ResourceKind::GpuCompute),
            ));
            actuator.release(&mut family).unwrap();
        }
        assert_eq!(reports[0], reports[1]);
    }

    const MIB: u64 = 1024 * 1024;

    /// A leaf with 2048 MiB anonymous, 1024 MiB file and 100 MiB unevictable
    /// memory, `swap_max` as `memory.swap.max`, and no `memory.high` limit.
    fn memory_leaf(dir: &Path, members: &str, swap_max: &str) {
        leaf(dir, members);
        for (name, value) in [
            ("memory.high", "max\n".to_string()),
            ("memory.max", "max\n".to_string()),
            ("memory.swap.max", format!("{swap_max}\n")),
            ("memory.current", format!("{}\n", 3172 * MIB)),
            (
                "memory.events",
                "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n".to_string(),
            ),
            (
                "memory.stat",
                format!(
                    "anon {}\nfile {}\nkernel 0\nunevictable {}\n",
                    2048 * MIB,
                    1024 * MIB,
                    100 * MIB
                ),
            ),
        ] {
            fs::write(dir.join(name), value).unwrap();
        }
    }

    fn ram_config() -> CgroupConfig {
        config(&["cgroup.procs", "memory.high"])
    }

    fn ram_actuator(
        dir: &Path,
        class: AllocationClass,
        swap_bytes: u64,
    ) -> CgroupActuator<DirectoryLeaf> {
        let mut settings = settings(class);
        settings.requested.ram_mib = 4096;
        CgroupActuator::new(
            DirectoryLeaf::open(dir).unwrap().with_host_swap(swap_bytes),
            &ram_config(),
            &settings,
        )
    }

    fn family(pid: u32) -> Recorder {
        Recorder {
            pids: vec![pid],
            signals: vec![],
        }
    }

    /// Without swap anonymous memory cannot be reclaimed, so the cap is never set
    /// below anon + unevictable + the noise tolerance; with swap only unevictable
    /// memory counts.
    #[test]
    fn a_ram_cap_never_goes_below_what_the_leaf_can_reclaim() {
        let unreclaimable = 2048 + 100 + 256;
        for (swap_max, host_swap, floor) in [
            ("0", 8192 * MIB, unreclaimable),
            ("max", 0, unreclaimable),
            ("max", 8192 * MIB, 100 + 256),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path().to_owned();
            memory_leaf(&dir, "71\n", swap_max);
            let mut actuator = ram_actuator(&dir, AllocationClass::Opportunistic, host_swap);
            assert!(
                !actuator
                    .unavailable(ResourceKind::Ram)
                    .contains(&YieldStep::Cap)
            );
            let mut family = family(71);
            actuator
                .apply(&mut family, ResourceKind::Ram, YieldStep::Cap, Some(1024))
                .unwrap();
            let expected = floor.max(1024);
            assert_eq!(
                read(&dir, "memory.high"),
                (expected * MIB).to_string(),
                "{swap_max} {host_swap}"
            );
            assert_eq!(actuator.applied_target(ResourceKind::Ram), Some(expected));
            assert_eq!(
                actuator.step_code(ResourceKind::Ram),
                (expected > 1024).then_some("ram_cap_growth_stop")
            );
            assert!(family.signals.is_empty());
        }
        // The floor is computed from memory.stat.
        let stat = format!("anon {}\nunevictable {}\n", 10 * MIB, MIB);
        assert_eq!(
            cedegrid::cgroup::ram_cap_floor_bytes(&stat, false, 256 * MIB).unwrap(),
            (10 + 1 + 256) * MIB
        );
        assert_eq!(
            cedegrid::cgroup::ram_cap_floor_bytes(&stat, true, 256 * MIB).unwrap(),
            (1 + 256) * MIB
        );
    }

    /// A cap that throttles for a calm window while usage does not fall is raised
    /// to current usage, so it only stops growth, and the cap step is reported
    /// unavailable for the hint to follow.
    #[test]
    fn an_ineffective_ram_cap_is_raised_to_current_usage() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        memory_leaf(&dir, "72\n", "max");
        let mut actuator = ram_actuator(&dir, AllocationClass::Opportunistic, 8192 * MIB);
        let mut family = family(72);
        actuator
            .apply(&mut family, ResourceKind::Ram, YieldStep::Cap, Some(1024))
            .unwrap();
        assert_eq!(read(&dir, "memory.high"), (1024 * MIB).to_string());
        for high in 1..=4 {
            fs::write(
                dir.join("memory.events"),
                format!("low 0\nhigh {high}\nmax 0\noom 0\noom_kill 0\n"),
            )
            .unwrap();
            actuator.tick(&mut family).unwrap();
            std::thread::sleep(Duration::from_millis(15));
        }
        assert_eq!(read(&dir, "memory.high"), (3172 * MIB).to_string());
        assert_eq!(actuator.applied_target(ResourceKind::Ram), Some(3172));
        assert!(
            actuator
                .unavailable(ResourceKind::Ram)
                .contains(&YieldStep::Cap)
        );
        assert_eq!(
            actuator.step_code(ResourceKind::Ram),
            Some("ram_cap_ineffective")
        );
        assert!(
            actuator
                .take_detail()
                .unwrap()
                .contains("ram_cap_ineffective")
        );
    }

    /// Throttling that frees at least the noise tolerance is a working cap.
    #[test]
    fn a_ram_cap_that_frees_memory_stays() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        memory_leaf(&dir, "73\n", "max");
        let mut actuator = ram_actuator(&dir, AllocationClass::Opportunistic, 8192 * MIB);
        let mut family = family(73);
        actuator
            .apply(&mut family, ResourceKind::Ram, YieldStep::Cap, Some(1024))
            .unwrap();
        for (high, current) in [(1, 3172), (2, 2800), (3, 2400), (4, 2000)] {
            fs::write(
                dir.join("memory.events"),
                format!("low 0\nhigh {high}\nmax 0\noom 0\noom_kill 0\n"),
            )
            .unwrap();
            fs::write(dir.join("memory.current"), format!("{}\n", current * MIB)).unwrap();
            actuator.tick(&mut family).unwrap();
            std::thread::sleep(Duration::from_millis(15));
        }
        assert_eq!(read(&dir, "memory.high"), (1024 * MIB).to_string());
        assert!(
            !actuator
                .unavailable(ResourceKind::Ram)
                .contains(&YieldStep::Cap)
        );
        assert_eq!(actuator.step_code(ResourceKind::Ram), None);
    }

    /// A readback that differs from the write makes the RAM cap unavailable for
    /// this allocation, and the step errors so the ladder goes on to the hint.
    #[test]
    fn a_ram_cap_readback_mismatch_makes_the_step_unavailable() {
        struct Stuck(DirectoryLeaf);
        impl LeafIo for Stuck {
            fn same_leaf(&self) -> anyhow::Result<bool> {
                self.0.same_leaf()
            }
            fn read(&self, control: &str) -> anyhow::Result<String> {
                self.0.read(control)
            }
            fn write(&self, control: &str, value: &str) -> anyhow::Result<()> {
                // The kernel keeps memory.high as it was.
                if control == "memory.high" {
                    return Ok(());
                }
                self.0.write(control, value)
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        memory_leaf(&dir, "74\n", "max");
        let mut settings = settings(AllocationClass::Opportunistic);
        settings.requested.ram_mib = 4096;
        let mut actuator = CgroupActuator::new(
            Stuck(DirectoryLeaf::open(&dir).unwrap()),
            &ram_config(),
            &settings,
        );
        let mut family = family(74);
        assert!(
            actuator
                .apply(&mut family, ResourceKind::Ram, YieldStep::Cap, Some(3500))
                .is_err()
        );
        assert!(
            actuator
                .unavailable(ResourceKind::Ram)
                .contains(&YieldStep::Cap)
        );
        assert_eq!(
            actuator.step_code(ResourceKind::Ram),
            Some("ram_cap_refused")
        );
        assert_eq!(actuator.applied_target(ResourceKind::Ram), None);
    }

    /// A cap write that took effect, then a failed readback and a failed restore, leaves
    /// `memory.high` at the cap: the step's error says so, and the next tick restores it.
    #[test]
    fn a_ram_cap_left_by_a_failed_restore_is_reported_and_restored_on_the_next_tick() {
        use std::cell::Cell;
        struct Flaky {
            leaf: DirectoryLeaf,
            fail_read: Cell<bool>,
            fail_write: Cell<bool>,
        }
        impl LeafIo for Flaky {
            fn same_leaf(&self) -> anyhow::Result<bool> {
                self.leaf.same_leaf()
            }
            fn read(&self, control: &str) -> anyhow::Result<String> {
                if control == "memory.high" && self.fail_read.take() {
                    anyhow::bail!("readback failed");
                }
                self.leaf.read(control)
            }
            fn write(&self, control: &str, value: &str) -> anyhow::Result<()> {
                if control == "memory.high" && value == "max" && self.fail_write.take() {
                    anyhow::bail!("restore failed");
                }
                self.leaf.write(control, value)
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        memory_leaf(&dir, "76\n", "max");
        let mut settings = settings(AllocationClass::Opportunistic);
        settings.requested.ram_mib = 4096;
        let leaf = Flaky {
            leaf: DirectoryLeaf::open(&dir).unwrap(),
            fail_read: Cell::new(true),
            fail_write: Cell::new(true),
        };
        let mut actuator = CgroupActuator::new(leaf, &ram_config(), &settings);
        let mut family = family(76);
        let error = actuator
            .apply(&mut family, ResourceKind::Ram, YieldStep::Cap, Some(3500))
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("memory.high could not be restored"),
            "{error:#}"
        );
        assert_eq!(read(&dir, "memory.high"), (3500 * MIB).to_string());
        actuator.tick(&mut family).unwrap();
        assert_eq!(read(&dir, "memory.high"), "max");
    }

    /// After an agent restart, a recorded RAM cap is adopted as the current step
    /// and an unrecorded `memory.high` is reset: no cap stays without a step.
    #[test]
    fn restart_reconcile_adopts_a_recorded_ram_cap_or_resets_an_unrecorded_one() {
        use cedegrid::contention::{ResourceKey, StepRequest, YieldCause};
        use std::collections::BTreeMap;
        for recorded in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path().to_owned();
            memory_leaf(&dir, "75\n", "max");
            fs::write(dir.join("memory.high"), format!("{}\n", 2500 * MIB)).unwrap();
            let mut actuator = ram_actuator(&dir, AllocationClass::Opportunistic, 8192 * MIB);
            let mut family = family(75);
            let applied: BTreeMap<ResourceKey, StepRequest> = if recorded {
                BTreeMap::from([(
                    ResourceKey::Ram,
                    StepRequest {
                        step: YieldStep::Cap,
                        target: Some(2500),
                        cause: YieldCause::FloorBreach,
                        deadline_ms: None,
                    },
                )])
            } else {
                BTreeMap::new()
            };
            actuator.reconcile(&mut family, &applied).unwrap();
            if recorded {
                assert_eq!(read(&dir, "memory.high"), (2500 * MIB).to_string());
                assert_eq!(actuator.applied_target(ResourceKind::Ram), Some(2500));
                actuator
                    .apply(&mut family, ResourceKind::Ram, YieldStep::None, None)
                    .unwrap();
            }
            assert_eq!(read(&dir, "memory.high"), "max", "recorded {recorded}");
            assert_eq!(actuator.applied_target(ResourceKind::Ram), None);
        }
    }

    /// Guaranteed work is never capped below its request, as on CPU.
    #[test]
    fn a_guaranteed_ram_cap_never_goes_below_its_request() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        memory_leaf(&dir, "76\n", "max");
        let mut actuator = ram_actuator(&dir, AllocationClass::Guaranteed, 8192 * MIB);
        let mut family = family(76);
        actuator
            .apply(&mut family, ResourceKind::Ram, YieldStep::Cap, Some(1024))
            .unwrap();
        assert_eq!(read(&dir, "memory.high"), (4096 * MIB).to_string());
    }

    /// Reversal sets `memory.high` back to its launch value: `max`, or the
    /// operator's configured limit.
    #[test]
    fn ram_cap_reversal_restores_the_launch_value() {
        for (configured, launch) in [
            (None, "max".to_string()),
            (Some(8192), (8192 * MIB).to_string()),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path().to_owned();
            memory_leaf(&dir, "77\n", "max");
            fs::write(dir.join("memory.high"), format!("{launch}\n")).unwrap();
            let mut config = ram_config();
            config.memory_high_mib = configured;
            let mut settings = settings(AllocationClass::Opportunistic);
            settings.requested.ram_mib = 4096;
            let mut actuator = CgroupActuator::new(
                DirectoryLeaf::open(&dir)
                    .unwrap()
                    .with_host_swap(8192 * MIB),
                &config,
                &settings,
            );
            let mut family = family(77);
            actuator
                .apply(&mut family, ResourceKind::Ram, YieldStep::Cap, Some(1024))
                .unwrap();
            assert_eq!(read(&dir, "memory.high"), (1024 * MIB).to_string());
            actuator
                .apply(&mut family, ResourceKind::Ram, YieldStep::None, None)
                .unwrap();
            assert_eq!(read(&dir, "memory.high"), launch);
            actuator
                .apply(&mut family, ResourceKind::Ram, YieldStep::Cap, Some(1024))
                .unwrap();
            actuator.release(&mut family).unwrap();
            assert_eq!(read(&dir, "memory.high"), launch);
        }
    }

    /// The cap step writes `memory.high` only, and only to the allocation's own
    /// unchanged leaf holding exactly the verified family.
    #[test]
    fn a_ram_cap_writes_only_memory_high_on_the_own_leaf() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("leaf");
        fs::create_dir(&dir).unwrap();
        memory_leaf(&dir, "78\n", "max");
        let io = DirectoryLeaf::open(&dir).unwrap();
        for control in ["memory.max", "memory.low", "memory.min"] {
            assert!(runtime_write(&io, &BTreeSet::from([78]), control, "1048576").is_err());
        }
        // A foreign PID in the leaf refuses the cap.
        fs::write(dir.join("cgroup.procs"), "78\n99\n").unwrap();
        let mut actuator = ram_actuator(&dir, AllocationClass::Opportunistic, 8192 * MIB);
        let mut family = family(78);
        assert!(
            actuator
                .apply(&mut family, ResourceKind::Ram, YieldStep::Cap, Some(1024))
                .is_err()
        );
        assert_eq!(read(&dir, "memory.high"), "max");
        // So does a leaf CedeGrid did not create: the pinned one was replaced.
        fs::write(dir.join("cgroup.procs"), "78\n").unwrap();
        let mut actuator = ram_actuator(&dir, AllocationClass::Opportunistic, 8192 * MIB);
        fs::rename(&dir, temp.path().join("old")).unwrap();
        fs::create_dir(&dir).unwrap();
        memory_leaf(&dir, "78\n", "max");
        assert!(
            actuator
                .apply(&mut family, ResourceKind::Ram, YieldStep::Cap, Some(1024))
                .is_err()
        );
        assert_eq!(read(&dir, "memory.high"), "max");
        // Without memory.high authorized, the cap stays unavailable, as on rootless.
        let actuator = CgroupActuator::new(
            DirectoryLeaf::open(&dir).unwrap(),
            &all_controls(),
            &settings(AllocationClass::Opportunistic),
        );
        assert!(
            actuator
                .unavailable(ResourceKind::Ram)
                .contains(&YieldStep::Cap)
        );
        assert!(
            actuator
                .unavailable(ResourceKind::GpuMemory)
                .contains(&YieldStep::Cap)
        );
    }

    /// A process that joins a leaf during its duty's off phase is thawed at the next tick, not
    /// at the phase's end, and the share moves to the rootless duty, which reaches only the
    /// verified family, so the leaf is never frozen again (Principle I, T510).
    #[test]
    fn a_process_that_joins_a_frozen_duty_leaf_is_thawed_at_once() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "81\n");
        fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
        let mut actuator = CgroupActuator::new(
            DirectoryLeaf::open(&dir).unwrap(),
            &all_controls(),
            &settings(AllocationClass::Opportunistic),
        );
        let mut family = timed(81);
        actuator
            .apply(
                &mut family,
                ResourceKind::GpuCompute,
                YieldStep::Cap,
                Some(500),
            )
            .unwrap();
        let end = std::time::Instant::now() + Duration::from_secs(2);
        while read(&dir, "cgroup.freeze") != "1" {
            assert!(std::time::Instant::now() < end, "the off phase came");
            actuator.tick(&mut family).unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        let deadline = actuator.next_deadline().unwrap();
        assert!(
            deadline <= std::time::Instant::now() + Duration::from_millis(60),
            "a frozen leaf's members are read again soon"
        );
        fs::write(dir.join("cgroup.procs"), "81\n99\n").unwrap();
        actuator.tick(&mut family).unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "0", "thawed at once");
        // The leaf stays thawed while the rootless duty stops only the family.
        let end = std::time::Instant::now() + Duration::from_millis(300);
        while std::time::Instant::now() < end {
            actuator.tick(&mut family).unwrap();
            assert_eq!(read(&dir, "cgroup.freeze"), "0", "never frozen again");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            family.signals.iter().any(|s| s.0 == libc::SIGSTOP),
            "the rootless duty took over: {:?}",
            family.signals
        );
    }

    /// A process that joins between the membership check and the freeze write is not left
    /// frozen: the members are read again after the write, and the pause moves to the rootless
    /// step (Principle I, T510).
    #[test]
    fn a_process_that_joins_during_the_freeze_write_is_thawed() {
        struct JoinsAtFreeze(DirectoryLeaf, std::path::PathBuf);
        impl LeafIo for JoinsAtFreeze {
            fn same_leaf(&self) -> anyhow::Result<bool> {
                self.0.same_leaf()
            }
            fn read(&self, control: &str) -> anyhow::Result<String> {
                self.0.read(control)
            }
            fn write(&self, control: &str, value: &str) -> anyhow::Result<()> {
                if control == "cgroup.freeze" && value == "1" {
                    fs::write(self.1.join("cgroup.procs"), "41\n77\n")?;
                }
                self.0.write(control, value)
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "41\n");
        fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
        let mut actuator = CgroupActuator::new(
            JoinsAtFreeze(DirectoryLeaf::open(&dir).unwrap(), dir.clone()),
            &config(&["cgroup.procs", "cpu.weight", "cpu.max", "cgroup.freeze"]),
            &settings(AllocationClass::Opportunistic),
        );
        let mut family = Recorder {
            pids: vec![41],
            signals: vec![],
        };
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Pause, None)
            .unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "0", "thawed after the write");
        assert!(
            family.signals.contains(&libc::SIGSTOP),
            "the pause moved to the family: {:?}",
            family.signals
        );
    }

    /// A release thaws a leaf a foreign process joined, at once: the freeze is this actuator's
    /// own, and lifting it acts on nothing it did not restrict (FR-078, Principle I;
    /// the release used to be refused, and only a later tick thawed it).
    #[test]
    fn a_release_thaws_a_leaf_a_process_joined() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "41\n");
        fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
        let mut actuator = CgroupActuator::new(
            DirectoryLeaf::open(&dir).unwrap(),
            &config(&["cgroup.procs", "cpu.weight", "cpu.max", "cgroup.freeze"]),
            &settings(AllocationClass::Opportunistic),
        );
        let mut family = Recorder {
            pids: vec![41],
            signals: vec![],
        };
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Pause, None)
            .unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "1");
        fs::write(dir.join("cgroup.procs"), "41\n77\n").unwrap();
        actuator.release(&mut family).unwrap();
        assert_eq!(
            read(&dir, "cgroup.freeze"),
            "0",
            "thawed though the other process stays"
        );
        assert!(family.signals.is_empty(), "{:?}", family.signals);
    }

    /// A release returns every control this actuator wrote to its launch value whatever the
    /// leaf's members, an empty leaf included, after the leader's exit (FR-078).
    #[test]
    fn a_release_restores_caps_whatever_the_members() {
        for members in ["81\n99\n", ""] {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path().to_owned();
            memory_leaf(&dir, "81\n", "max");
            let mut settings = settings(AllocationClass::Opportunistic);
            settings.requested.ram_mib = 4096;
            let mut actuator = CgroupActuator::new(
                DirectoryLeaf::open(&dir).unwrap(),
                &config(&["cgroup.procs", "cpu.weight", "cpu.max", "memory.high"]),
                &settings,
            );
            let mut family = family(81);
            actuator
                .apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(250))
                .unwrap();
            actuator
                .apply(&mut family, ResourceKind::Ram, YieldStep::Cap, Some(3500))
                .unwrap();
            assert_ne!(read(&dir, "memory.high"), "max");
            fs::write(dir.join("cgroup.procs"), members).unwrap();
            if members.is_empty() {
                family.pids.clear();
            }
            actuator.release(&mut family).unwrap();
            assert_eq!(read(&dir, "cpu.max"), "max 100000", "{members:?}");
            assert_eq!(read(&dir, "memory.high"), "max", "{members:?}");
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "82\n");
        let mut actuator = CgroupActuator::new(
            DirectoryLeaf::open(&dir).unwrap(),
            &all_controls(),
            &settings(AllocationClass::Opportunistic),
        );
        let mut family = family(82);
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::LowerShare, None)
            .unwrap();
        assert_eq!(read(&dir, "cpu.weight"), "1");
        fs::write(dir.join("cgroup.procs"), "82\n99\n").unwrap();
        actuator.release(&mut family).unwrap();
        assert_eq!(read(&dir, "cpu.weight"), "10");
    }

    /// A restore never tightens a control: where the value in place is looser than the launch
    /// value, it stays, and only a write that verifies the members brings the launch value back
    /// (FR-078).
    #[test]
    fn a_restore_never_tightens() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "83\n");
        fs::write(dir.join("cpu.max"), "20000 100000\n").unwrap();
        let mut config = all_controls();
        config.cpu_max = Some(cedegrid::cgroup::CpuMax {
            quota_us: 20_000,
            period_us: 100_000,
        });
        let mut actuator = CgroupActuator::new(
            DirectoryLeaf::open(&dir).unwrap(),
            &config,
            &settings(AllocationClass::Opportunistic),
        );
        let mut family = family(83);
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Cap, Some(100))
            .unwrap();
        assert_eq!(read(&dir, "cpu.max"), "10000 100000");
        // Something loosened it meanwhile, and a foreign process joined.
        fs::write(dir.join("cpu.max"), "max 100000\n").unwrap();
        fs::write(dir.join("cgroup.procs"), "83\n99\n").unwrap();
        assert!(actuator.release(&mut family).is_err(), "no verified write");
        assert_eq!(read(&dir, "cpu.max"), "max 100000", "not tightened");
        fs::write(dir.join("cgroup.procs"), "83\n").unwrap();
        actuator.watch(&mut family).unwrap();
        assert_eq!(read(&dir, "cpu.max"), "20000 100000");
    }

    /// A thaw whose write failed at release is retried by the watch that runs in every phase,
    /// without a tick and without a rootless pause (FR-078).
    #[test]
    fn a_failed_thaw_is_retried_without_a_tick() {
        use std::cell::Cell;
        struct FailThaw {
            leaf: DirectoryLeaf,
            fail: std::rc::Rc<Cell<u32>>,
        }
        impl LeafIo for FailThaw {
            fn same_leaf(&self) -> anyhow::Result<bool> {
                self.leaf.same_leaf()
            }
            fn read(&self, control: &str) -> anyhow::Result<String> {
                self.leaf.read(control)
            }
            fn write(&self, control: &str, value: &str) -> anyhow::Result<()> {
                if control == "cgroup.freeze" && value == "0" && self.fail.get() > 0 {
                    self.fail.set(self.fail.get() - 1);
                    anyhow::bail!("thaw write failed");
                }
                self.leaf.write(control, value)
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "84\n");
        fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
        let fail = std::rc::Rc::new(Cell::new(0));
        let mut actuator = CgroupActuator::new(
            FailThaw {
                leaf: DirectoryLeaf::open(&dir).unwrap(),
                fail: fail.clone(),
            },
            &all_controls(),
            &settings(AllocationClass::Opportunistic),
        );
        let mut family = family(84);
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Pause, None)
            .unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "1");
        fail.set(1);
        assert!(actuator.release(&mut family).is_err());
        assert_eq!(read(&dir, "cgroup.freeze"), "1");
        assert!(
            actuator
                .next_deadline()
                .is_some_and(|at| at <= std::time::Instant::now() + Duration::from_millis(60)),
            "retried within 50 ms"
        );
        actuator.watch(&mut family).unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "0");
        assert!(family.signals.is_empty(), "{:?}", family.signals);
    }

    /// While a recorded freeze holds, a member list that cannot be read twice running counts as
    /// changed and thaws the leaf; one unreadable read followed by a matching one does not
    /// (FR-078, Principle III).
    #[test]
    fn unreadable_members_twice_thaw_a_frozen_leaf() {
        use std::cell::Cell;
        struct Unreadable {
            leaf: DirectoryLeaf,
            fail: std::rc::Rc<Cell<bool>>,
        }
        impl LeafIo for Unreadable {
            fn same_leaf(&self) -> anyhow::Result<bool> {
                self.leaf.same_leaf()
            }
            fn read(&self, control: &str) -> anyhow::Result<String> {
                if control == "cgroup.procs" && self.fail.get() {
                    anyhow::bail!("members unreadable");
                }
                self.leaf.read(control)
            }
            fn write(&self, control: &str, value: &str) -> anyhow::Result<()> {
                self.leaf.write(control, value)
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "85\n");
        fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
        let fail = std::rc::Rc::new(Cell::new(false));
        let mut actuator = CgroupActuator::new(
            Unreadable {
                leaf: DirectoryLeaf::open(&dir).unwrap(),
                fail: fail.clone(),
            },
            &all_controls(),
            &settings(AllocationClass::Opportunistic),
        );
        let mut family = family(85);
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Pause, None)
            .unwrap();
        fail.set(true);
        actuator.watch(&mut family).unwrap();
        assert_eq!(
            read(&dir, "cgroup.freeze"),
            "1",
            "one unreadable read holds"
        );
        fail.set(false);
        actuator.watch(&mut family).unwrap();
        assert_eq!(
            read(&dir, "cgroup.freeze"),
            "1",
            "a matching read resets it"
        );
        fail.set(true);
        actuator.watch(&mut family).unwrap();
        actuator.watch(&mut family).unwrap();
        assert_eq!(
            read(&dir, "cgroup.freeze"),
            "0",
            "the second in a row thaws"
        );
        assert_eq!(
            family.signals,
            [libc::SIGSTOP],
            "the pause moves to the family"
        );
    }

    /// A reattaching supervisor's actuator thaws a freeze it recorded itself whatever the
    /// leaf's members; a freeze it has no record of still needs the members verified
    /// (FR-078).
    #[test]
    fn a_reconcile_thaws_its_own_freeze_whatever_the_members() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        leaf(&dir, "86\n");
        fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
        let mut actuator = CgroupActuator::new(
            DirectoryLeaf::open(&dir).unwrap(),
            &all_controls(),
            &settings(AllocationClass::Opportunistic),
        );
        let mut family = family(86);
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::Pause, None)
            .unwrap();
        fs::write(dir.join("cgroup.procs"), "86\n99\n").unwrap();
        actuator
            .reconcile(&mut family, &std::collections::BTreeMap::new())
            .unwrap();
        assert_eq!(read(&dir, "cgroup.freeze"), "0");
    }

    /// A leaf left frozen by a duty's off phase is thawed when an agent reattaches and the
    /// applied steps record no Pause; a recorded Pause stays frozen; a foreign member refuses
    /// the write (FR-078).
    #[test]
    fn a_reattach_thaws_a_leaf_frozen_without_a_recorded_pause() {
        use cedegrid::contention::{ResourceKey, StepRequest, YieldCause};
        use std::collections::BTreeMap;
        let pause = BTreeMap::from([(
            ResourceKey::Cpu,
            StepRequest {
                step: YieldStep::Pause,
                target: None,
                cause: YieldCause::ExternalGrowth,
                deadline_ms: None,
            },
        )]);
        for (applied, members, thawed) in [
            (BTreeMap::new(), "31\n", true),
            (pause, "31\n", false),
            (BTreeMap::new(), "31\n99\n", false),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path().to_owned();
            leaf(&dir, members);
            fs::write(dir.join("cgroup.freeze"), "1\n").unwrap();
            fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
            let mut actuator = CgroupActuator::new(
                DirectoryLeaf::open(&dir).unwrap(),
                &config(&["cgroup.procs", "cpu.weight", "cpu.max", "cgroup.freeze"]),
                &settings(AllocationClass::Opportunistic),
            );
            let mut family = Recorder {
                pids: vec![31],
                signals: vec![],
            };
            let result = actuator.reconcile(&mut family, &applied);
            assert_eq!(
                read(&dir, "cgroup.freeze"),
                if thawed { "0" } else { "1" },
                "{applied:?} {members:?}"
            );
            if members.contains("99") {
                assert!(result.is_err(), "a foreign member refuses the thaw");
            }
        }
    }
}
