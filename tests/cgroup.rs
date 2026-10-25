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

    #[test]
    fn a_refused_restore_holds_the_step_until_it_can_be_undone() {
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
        assert!(
            actuator
                .apply(&mut family, ResourceKind::Cpu, YieldStep::None, None)
                .is_err()
        );
        assert_eq!(read(&dir, "cpu.max"), "25000 100000");
        fs::write(dir.join("cgroup.procs"), "51\n").unwrap();
        actuator
            .apply(&mut family, ResourceKind::Cpu, YieldStep::None, None)
            .unwrap();
        assert_eq!(read(&dir, "cpu.max"), "max 100000");
        assert!(family.signals.is_empty());
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
}
