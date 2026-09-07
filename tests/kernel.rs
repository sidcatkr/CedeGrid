use resource_manager::kernel::{
    CpuUsageStatus, CpuUsageTracker, KernelCollector, KernelConfig, PressureResource,
    PressureScope, PressureStatus, PressureTracker, ProbeRoots, inspect_control,
    parse_cgroup_mounts, parse_cpu_list, parse_cpu_max, parse_cpu_stat, parse_psi,
    parse_self_cgroup,
};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[test]
fn cpu_monotonic_idle_complement_covers_observed_missing_busy_ticks() {
    let mut tracker = CpuUsageTracker::with_clock_tick_hz(std::num::NonZeroU64::new(100).unwrap());
    let initial = "cpu0 100 0 0 100 0 0 0 0\ncpu1 100 0 0 100 0 0 0 0\n";
    // anchor raw-cpu007: the half-core task accumulated196ms runtime in500ms,
    // while cpu0 exported2 busy ticks and30idle ticks. A tick-ratio alone
    // reports62.5mCPU and cannot support subtracting the task's actual usage.
    let next = "cpu0 102 0 0 130 0 0 0 0\ncpu1 100 0 0 150 0 0 0 0\n";
    tracker.observe(Ok(initial), Some(&[0, 1]), 0, 1500);
    let observed = tracker.observe(Ok(next), Some(&[0, 1]), 500, 1500);
    assert_eq!(observed.status, CpuUsageStatus::Available);
    assert_eq!(observed.busy_millicores, Some(440));
    assert!(observed.busy_millicores.unwrap() >= 392);
    // Missing/corrupt time remains unknown, never clamped into spare capacity.
    let inconsistent = "cpu0 103 0 0 230 0 0 0 0\ncpu1 100 0 0 200 0 0 0 0\n";
    let observed = tracker.observe(Ok(inconsistent), Some(&[0, 1]), 1000, 1500);
    assert_eq!(observed.status, CpuUsageStatus::Unknown);
    assert_eq!(observed.busy_millicores, None);
}

#[test]
fn cpu_idle_complement_preserves_busy_tick_and_iowait_protection() {
    let mut tracker = CpuUsageTracker::with_clock_tick_hz(std::num::NonZeroU64::new(100).unwrap());
    tracker.observe(Ok("cpu0 100 0 0 100 0 0 0 0\n"), Some(&[0]), 0, 1500);
    let observed = tracker.observe(Ok("cpu0 125 0 0 100 25 0 0 0\n"), Some(&[0]), 500, 1500);
    assert_eq!(observed.busy_millicores, Some(1000));
    assert_eq!(
        tracker
            .observe(Ok("cpu0 125 0 0 100 25 0 0 0\n"), Some(&[0]), 600, 1500)
            .status,
        CpuUsageStatus::NoInterval
    );
}

fn psi(some: u64, full: u64) -> String {
    format!(
        "some avg10=1.00 avg60=0.50 avg300=0.10 total={some}\nfull avg10=0.05 avg60=0.00 avg300=0.00 total={full}\n"
    )
}

#[test]
fn cpu_accounting_uses_only_effective_cpus_and_excludes_guest_double_counting() {
    let first = "cpu 0 0 0 0 0 0 0 0 0 0\ncpu0 900 0 0 100 0 0 0 0 400 0\ncpu2 100 0 0 900 0 0 0 0 20 0\ncpu3 200 0 0 800 0 0 0 0 0 0\nctxt 100\n";
    let second = "cpu0 1000 0 0 100 0 0 0 0 450 0\ncpu2 125 0 0 975 0 0 0 0 25 0\ncpu3 250 0 0 850 0 0 0 0 0 0\n";
    let parsed = parse_cpu_stat(first).unwrap();
    assert_eq!(parsed[&0].total, 1000);
    assert_eq!(parsed[&0].idle, 100);
    let mut tracker = CpuUsageTracker::default();
    assert_eq!(
        tracker.observe(Ok(first), Some(&[2, 3]), 0, 1500).status,
        CpuUsageStatus::Baseline
    );
    let measured = tracker.observe(Ok(second), Some(&[2, 3]), 500, 1500);
    assert_eq!(measured.status, CpuUsageStatus::Available);
    assert_eq!(measured.busy_millicores, Some(750));
    assert_eq!(measured.interval_ms, Some(500));
    assert_eq!(
        tracker
            .observe(Ok(second), Some(&[0, 2, 3]), 1000, 1500)
            .status,
        CpuUsageStatus::Baseline
    );
}

#[test]
fn cpu_unknown_stale_and_reset_samples_cannot_establish_free_capacity() {
    let old = "cpu0 100 0 0 900 0 0 0 0\n";
    let new = "cpu0 110 0 0 990 0 0 0 0\n";
    let mut tracker = CpuUsageTracker::default();
    tracker.observe(Ok(old), Some(&[0]), 0, 1500);
    let repeated = tracker.observe(Ok(old), Some(&[0]), 100, 1500);
    assert_eq!(repeated.status, CpuUsageStatus::NoInterval);
    assert!(repeated.busy_millicores.is_none());
    let stale = tracker.observe(Ok(new), Some(&[0]), 2000, 1500);
    assert_eq!(stale.status, CpuUsageStatus::Stale);
    assert!(stale.busy_millicores.is_none());
    assert_eq!(
        tracker.observe(Ok(old), Some(&[0]), 2200, 1500).status,
        CpuUsageStatus::CounterReset
    );
    assert_eq!(
        tracker.observe(Ok(new), Some(&[0, 1]), 2400, 1500).status,
        CpuUsageStatus::Unknown
    );
    assert_eq!(
        tracker.observe(Ok(new), Some(&[0]), 2600, 1500).status,
        CpuUsageStatus::Baseline
    );
    let missing = tracker.observe(
        Err(io::Error::from(io::ErrorKind::PermissionDenied)),
        Some(&[0]),
        2800,
        1500,
    );
    assert_eq!(missing.status, CpuUsageStatus::Unknown);
    assert!(missing.busy_millicores.is_none());
    assert!(parse_cpu_stat("cpu0 1 2\n").is_err());
    assert!(parse_cpu_stat("cpu0 x 0 0 0\n").is_err());
    assert!(parse_cpu_stat("cpu0 1 0 0 0\ncpu0 1 0 0 0\n").is_err());
}

#[test]
fn configuration_is_strict_and_freshness_is_configurable() {
    let config: KernelConfig = serde_json::from_str("{}").unwrap();
    assert!(config.enabled && config.psi);
    assert_eq!(
        config.monitor_interval_ms * config.freshness_intervals,
        1500
    );
    assert!(serde_json::from_str::<KernelConfig>(r#"{"unknown":true}"#).is_err());
    assert!(
        KernelConfig {
            monitor_interval_ms: 0,
            ..config.clone()
        }
        .validate()
        .is_err()
    );
    assert!(
        KernelConfig {
            freshness_intervals: u64::MAX,
            ..config.clone()
        }
        .validate()
        .is_err()
    );
    assert!(
        KernelConfig {
            delegated_root: Some("/valid/../untrusted".into()),
            ..config
        }
        .validate()
        .is_err()
    );
}

#[test]
fn cpu_lists_and_bandwidth_are_strict_and_bounded() {
    assert_eq!(
        parse_cpu_list("0-2,7,2,9-10").unwrap(),
        vec![0, 1, 2, 7, 9, 10]
    );
    assert_eq!(parse_cpu_list("").unwrap(), Vec::<u32>::new());
    for input in ["2-1", "-1", "1,,2", "1-2-3", "0-999999999", "1, 2"] {
        assert!(parse_cpu_list(input).is_err(), "{input}");
    }
    assert_eq!(
        parse_cpu_max("150000 100000").unwrap().ceiling_millicores(),
        Some(1500)
    );
    assert_eq!(
        parse_cpu_max("max 100000").unwrap().ceiling_millicores(),
        None
    );
    for input in [
        "max 0",
        "100 0",
        "0 100",
        "x 100",
        "100",
        "100 100 unexpected",
    ] {
        assert!(parse_cpu_max(input).is_err());
    }
}

#[test]
fn psi_parser_does_not_invent_missing_full_or_accept_invalid_samples() {
    assert!(
        parse_psi("some avg10=0 avg60=0 avg300=0 total=0")
            .unwrap()
            .full
            .is_none()
    );
    for input in [
        "",
        "some avg10=NaN avg60=0 avg300=0 total=1",
        "some avg10=101 avg60=0 avg300=0 total=1",
        "some avg10=0 avg60=0 total=1",
        "some avg10=0 avg60=0 avg300=0 total=-1",
        "some avg10=0 avg10=0 avg60=0 avg300=0 total=1",
    ] {
        assert!(parse_psi(input).is_err(), "{input}");
    }
}

#[test]
fn psi_scope_freshness_resets_and_unavailable_samples_are_explicit() {
    let mut tracker = PressureTracker::default();
    let source = PathBuf::from("/fixture/cpu.pressure");
    let scope = PressureScope::Cgroup {
        path: "/fixture".into(),
    };
    let mut read = |text: String, at| {
        tracker.observe(
            source.clone(),
            scope.clone(),
            PressureResource::Cpu,
            Ok(&text),
            1_000 + at,
            at,
            1500,
        )
    };
    assert_eq!(read(psi(100, 50), 0).status, PressureStatus::Baseline);
    let current = read(psi(200, 75), 500);
    assert_eq!(current.status, PressureStatus::Available);
    assert_eq!(current.some_stall_delta_us, Some(100));
    assert_eq!(current.full_stall_delta_us, Some(25));
    assert_eq!(read(psi(200, 75), 500).status, PressureStatus::NoInterval);
    assert_eq!(read(psi(2, 1), 1_000).status, PressureStatus::CounterReset);
    let stale = read(psi(3, 1), 3_000);
    assert_eq!(stale.status, PressureStatus::Stale);
    assert_eq!(stale.some_stall_delta_us, None);
    assert_eq!(read(psi(4, 1), 3_100).status, PressureStatus::Available);
    let missing = tracker.observe(
        source.clone(),
        scope.clone(),
        PressureResource::Cpu,
        Err(io::Error::from(io::ErrorKind::PermissionDenied)),
        5000,
        4000,
        1500,
    );
    assert_eq!(missing.status, PressureStatus::PermissionDenied);
    assert!(missing.counters.is_none());
    let restored = tracker.observe(
        source,
        scope,
        PressureResource::Cpu,
        Ok(&psi(6, 2)),
        5100,
        4100,
        1500,
    );
    assert_eq!(restored.status, PressureStatus::Baseline);
}

#[test]
fn system_cpu_full_is_undefined_and_scope_changes_reset_baseline() {
    let mut tracker = PressureTracker::default();
    let source = PathBuf::from("/fixture/cpu");
    let system = tracker.observe(
        source.clone(),
        PressureScope::System,
        PressureResource::Cpu,
        Ok(&psi(10, 0)),
        1,
        1,
        1500,
    );
    assert!(system.counters.unwrap().full.is_none());
    assert!(system.detail.contains("undefined"));
    let changed = tracker.observe(
        source,
        PressureScope::Cgroup {
            path: "/different".into(),
        },
        PressureResource::Cpu,
        Ok(&psi(20, 1)),
        2,
        2,
        1500,
    );
    assert_eq!(changed.status, PressureStatus::Baseline);
    assert!(changed.counters.unwrap().full.is_some());
}

#[test]
fn mountinfo_and_membership_resolve_actual_mounts_without_assuming_sys_fs() {
    let mounts =
        parse_cgroup_mounts("22 11 0:20 /tenant /custom\\040root ro,nosuid - cgroup2 cgroup rw\n")
            .unwrap();
    assert_eq!(mounts[0].mount_point, PathBuf::from("/custom root"));
    assert_eq!(mounts[0].root, PathBuf::from("/tenant"));
    assert!(mounts[0].read_only);
    assert_eq!(
        parse_self_cgroup("1:cpu:/v1\n0::/tenant/agent\n").unwrap(),
        Some("/tenant/agent".into())
    );
    assert!(parse_self_cgroup("0::/tenant/../outside").is_err());
    assert!(parse_self_cgroup("0::/tenant (deleted)").is_err());
    assert!(parse_self_cgroup("0::/one\n0::/two").is_err());
    assert!(parse_cgroup_mounts("bad line").is_err());
}

fn write(path: &Path, name: &str, value: &str) {
    fs::create_dir_all(path).unwrap();
    fs::write(path.join(name), value).unwrap();
}

#[test]
fn fixture_probe_records_ancestor_limits_effective_sets_and_hierarchy_without_writes() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let proc = base.join("proc");
    let sys = base.join("sys");
    let mount = base.join("cgroups");
    let parent = mount.join("tenant");
    let leaf = parent.join("manager");
    write(
        &proc.join("self"),
        "mountinfo",
        &format!(
            "22 11 0:20 / {} rw,nosuid - cgroup2 cgroup rw\n",
            mount.display()
        ),
    );
    write(&proc.join("self"), "cgroup", "0::/tenant/manager\n");
    write(
        &proc.join("self"),
        "status",
        "Name:\tfixture\nCpus_allowed_list:\t0-5\n",
    );
    write(&proc.join("sys/kernel"), "osrelease", "6.6.fixture");
    for (path, quota, cpus) in [
        (&mount, "max 100000", "0-7"),
        (&parent, "150000 100000", "0-5"),
        (&leaf, "max 100000", "2-5"),
    ] {
        for (name, value) in [
            ("cgroup.type", "domain"),
            ("cgroup.controllers", "cpu memory cpuset"),
            ("cgroup.subtree_control", "cpu memory cpuset"),
            ("cpu.max", quota),
            ("cpu.weight", "10"),
            ("cpuset.cpus.effective", cpus),
            ("memory.max", "max"),
            ("memory.high", "max"),
            ("memory.current", "123"),
            ("cpu.stat", "usage_usec 100\nnr_throttled 0"),
            ("memory.events", "oom 0"),
            ("cgroup.procs", ""),
        ] {
            write(path, name, value);
        }
    }
    for resource in ["cpu", "memory", "io"] {
        write(&proc.join("pressure"), resource, &psi(100, 0));
        write(&leaf, &format!("{resource}.pressure"), &psi(40, 5));
    }
    write(
        &sys.join("devices/system/cpu/cpu2/topology"),
        "core_id",
        "1",
    );
    write(
        &sys.join("devices/system/cpu/cpu2/topology"),
        "physical_package_id",
        "0",
    );
    write(
        &sys.join("devices/system/cpu/cpu2/topology"),
        "thread_siblings_list",
        "2,6",
    );
    let mut collector = KernelCollector::inspect_at(
        KernelConfig {
            delegated_root: Some(leaf.clone()),
            ..KernelConfig::default()
        },
        ProbeRoots { proc, sys },
    );
    let snapshot = collector.sample();
    assert_eq!(snapshot.kernel_release.as_deref(), Some("6.6.fixture"));
    assert_eq!(snapshot.cpu.effective_cpu_ids, Some(vec![2, 3, 4, 5]));
    assert_eq!(snapshot.cpu.visible_cpu_ceiling_millicores, Some(1500));
    assert_eq!(snapshot.cpu.cores[0].thread_siblings, Some(vec![2, 6]));
    let hierarchy = snapshot.hierarchy.unwrap();
    assert_eq!(hierarchy.ancestors.len(), 3);
    assert_eq!(hierarchy.ancestors[1].path, parent);
    assert!(hierarchy.detail.contains("relative and hierarchical"));
    assert_eq!(snapshot.psi.len(), 6);
    assert!(snapshot.controls.iter().all(|entry| !entry.applied));
    assert!(
        snapshot
            .controls
            .iter()
            .filter(|entry| entry.control == "cpu.max")
            .all(|entry| entry.permitted.is_none())
    );
    assert_eq!(
        fs::read_to_string(leaf.join("cpu.max")).unwrap(),
        "max 100000"
    );
    assert_eq!(fs::read_to_string(leaf.join("cgroup.procs")).unwrap(), "");
}

#[test]
fn control_metadata_does_not_claim_write_permission_or_enforcement() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("cpu.weight");
    fs::write(&file, "100").unwrap();
    let evidence = inspect_control(&file, false);
    assert_eq!(evidence.available, Some(true));
    assert_eq!(evidence.permitted, None);
    assert!(!evidence.applied);
    assert_eq!(inspect_control(&file, true).permitted, Some(false));
    let missing = inspect_control(&temp.path().join("cgroup.kill"), false);
    assert_eq!(missing.available, Some(false));
    assert!(missing.fallback);
}

#[test]
fn invalid_or_disabled_optional_config_does_not_break_baseline() {
    let mut disabled = KernelCollector::new(KernelConfig {
        enabled: false,
        ..KernelConfig::default()
    });
    let disabled = disabled.sample();
    assert!(disabled.controls.iter().all(|control| !control.applied));
    assert!(disabled.psi.is_empty());
    let mut invalid = KernelCollector::new(KernelConfig {
        monitor_interval_ms: 0,
        ..KernelConfig::default()
    });
    assert!(invalid.sample().limitations[0].contains("Invalid"));
}

#[cfg(not(target_os = "linux"))]
#[test]
fn non_linux_reports_unverified_behavior_without_emulation() {
    let snapshot = KernelCollector::new(KernelConfig::default()).sample();
    assert!(snapshot.hierarchy.is_none());
    assert!(
        snapshot
            .controls
            .iter()
            .all(|control| control.available == Some(false) && !control.applied)
    );
    assert!(
        snapshot
            .limitations
            .iter()
            .any(|detail| detail.contains("unverified"))
    );
}
