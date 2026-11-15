use cedegrid::kernel::{
    CpuPressureStatus, CpuUsageStatus, CpuUsageTracker, KernelCollector, KernelConfig,
    PressureResource, PressureScope, PressureStatus, PressureTracker, ProbeRoots, inspect_control,
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
fn cpu_idle_complement_preserves_busy_ticks_and_needs_an_interval() {
    let mut tracker = CpuUsageTracker::with_clock_tick_hz(std::num::NonZeroU64::new(100).unwrap());
    tracker.observe(Ok("cpu0 100 0 0 100 0 0 0 0\n"), Some(&[0]), 0, 1500);
    // Iowait is idle: 25 busy ticks of 50 with one tick of rounding.
    let observed = tracker.observe(Ok("cpu0 125 0 0 100 25 0 0 0\n"), Some(&[0]), 500, 1500);
    assert_eq!(observed.busy_millicores, Some(520));
    assert_eq!(
        tracker
            .observe(Ok("cpu0 125 0 0 100 25 0 0 0\n"), Some(&[0]), 600, 1500)
            .status,
        CpuUsageStatus::NoInterval
    );
}

#[test]
fn cpu_busy_bounds_round_by_one_tick_and_count_iowait_as_idle() {
    let mut tracker = CpuUsageTracker::with_clock_tick_hz(std::num::NonZeroU64::new(100).unwrap());
    tracker.observe(Ok("cpu0 100 0 0 100 0 0 0 0\n"), Some(&[0]), 0, 1500);
    // 25 busy and 25 idle ticks in 500 ms: one tick of rounding either way.
    let plain = tracker.observe(Ok("cpu0 125 0 0 125 0 0 0 0\n"), Some(&[0]), 500, 1500);
    assert_eq!(plain.busy_millicores, Some(520));
    assert_eq!(plain.busy_lower_millicores, Some(480));
    // 25 user and 25 iowait ticks: iowait is idle in both bounds.
    let iowait = tracker.observe(Ok("cpu0 150 0 0 125 25 0 0 0\n"), Some(&[0]), 1000, 1500);
    assert_eq!(iowait.busy_millicores, Some(520));
    assert_eq!(iowait.busy_lower_millicores, Some(480));
    // Unknown usage has no bounds either.
    let repeated = tracker.observe(Ok("cpu0 150 0 0 125 25 0 0 0\n"), Some(&[0]), 1000, 1500);
    assert_eq!(repeated.busy_lower_millicores, None);
}

/// Iowait on a CPU is idle time another task may run in: the busy upper bound does not count
/// it, so own threads blocked on disk are not booked as external CPU (FR-073).
#[test]
fn iowait_is_idle_in_both_busy_bounds() {
    let mut tracker = CpuUsageTracker::with_clock_tick_hz(std::num::NonZeroU64::new(100).unwrap());
    tracker.observe(Ok("cpu0 100 0 0 100 0 0 0 0\n"), Some(&[0]), 0, 1500);
    // 25 user and 25 iowait ticks in 500 ms read as the plain 25 busy and 25 idle ticks do.
    let iowait = tracker.observe(Ok("cpu0 125 0 0 100 25 0 0 0\n"), Some(&[0]), 500, 1500);
    assert_eq!(iowait.busy_millicores, Some(520));
    assert_eq!(iowait.busy_lower_millicores, Some(480));
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

/// A CPU the task may run on but that is offline has no `/proc/stat` counter: it is left out
/// of the CPU set, so CPU telemetry stays known.
#[test]
fn an_allowed_but_offline_cpu_is_left_out_of_the_cpu_set() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let (proc, sys) = (base.join("proc"), base.join("sys"));
    write(
        &proc.join("self"),
        "status",
        "Name:\tfixture\nCpus_allowed_list:\t0-3\n",
    );
    write(&proc.join("sys/kernel"), "osrelease", "6.6.fixture");
    write(&sys.join("devices/system/cpu"), "online", "0-2\n");
    let mut collector = KernelCollector::inspect_at(
        KernelConfig::default(),
        ProbeRoots {
            proc: proc.clone(),
            sys: sys.clone(),
        },
    );
    let snapshot = collector.sample();
    assert_eq!(snapshot.cpu.allowed_cpu_ids, Some(vec![0, 1, 2, 3]));
    assert_eq!(snapshot.cpu.effective_cpu_ids, Some(vec![0, 1, 2]));
    assert_eq!(snapshot.cpu.effective_cpu_capacity_millicores, Some(3000));
    // Without the online list every allowed CPU stays.
    fs::remove_file(sys.join("devices/system/cpu/online")).unwrap();
    let snapshot =
        KernelCollector::inspect_at(KernelConfig::default(), ProbeRoots { proc, sys }).sample();
    assert_eq!(snapshot.cpu.effective_cpu_ids, Some(vec![0, 1, 2, 3]));
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

fn pressure(avg10: f64) -> String {
    format!(
        "some avg10={avg10:.2} avg60=0.00 avg300=0.00 total=100\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"
    )
}

/// Own work runs in user 1000's slice. The root's other children and other users'
/// slices are external; user 1000's own sessions and the agent's cgroup are not.
fn psi_host(base: &Path) -> (ProbeRoots, PathBuf) {
    let proc = base.join("proc");
    let mount = base.join("cgroups");
    write(
        &proc.join("self"),
        "mountinfo",
        &format!(
            "22 11 0:20 / {} rw,nosuid - cgroup2 cgroup rw\n",
            mount.display()
        ),
    );
    let own = "user.slice/user-1000.slice/user@1000.service/app.slice/agent.scope";
    write(&proc.join("self"), "cgroup", &format!("0::/{own}\n"));
    write(&proc.join("pressure"), "cpu", &pressure(70.0));
    for (path, avg10) in [
        ("system.slice", 35.0),
        ("init.scope", 5.0),
        ("user.slice/user-1001.slice", 12.0),
        ("user.slice", 90.0),
        ("user.slice/user-1000.slice", 88.0),
        ("user.slice/user-1000.slice/session-2.scope", 80.0),
        ("user.slice/user-1000.slice/user@1000.service", 92.0),
        (own, 95.0),
    ] {
        write(&mount.join(path), "cpu.pressure", &pressure(avg10));
    }
    (
        ProbeRoots {
            proc,
            sys: base.join("sys"),
        },
        mount,
    )
}

/// FR-072: external PSI is the highest `some` avg10 of the cgroups outside own
/// work's user slice; own and system-wide pressure are shown, never external.
#[test]
fn external_cpu_pressure_reads_only_cgroups_outside_own_user_slice() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let (roots, mount) = psi_host(&base);
    let before = fs::read_to_string(mount.join("system.slice/cpu.pressure")).unwrap();
    let pressure = cedegrid::kernel::cpu_pressure(&roots, None);
    assert_eq!(
        pressure.status,
        CpuPressureStatus::Available,
        "{}",
        pressure.detail
    );
    assert_eq!(pressure.external_some_avg10, Some(35.0));
    assert_eq!(pressure.external_cgroups, 3);
    assert_eq!(pressure.own_some_avg10, Some(95.0));
    assert_eq!(pressure.system_some_avg10, Some(70.0));
    assert_eq!(
        fs::read_to_string(mount.join("system.slice/cpu.pressure")).unwrap(),
        before
    );
    // The collector carries it in the kernel snapshot.
    let mut collector = KernelCollector::inspect_at(KernelConfig::default(), roots);
    let snapshot = collector.sample();
    assert_eq!(
        snapshot.cpu_pressure.map(|p| p.external_some_avg10),
        Some(Some(35.0))
    );
}

/// Without per-cgroup PSI, system-wide pressure cannot tell own from external
/// waiting, so it is no external evidence.
#[test]
fn only_system_wide_pressure_is_no_external_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let (roots, mount) = psi_host(&base);
    for path in ["system.slice", "init.scope", "user.slice/user-1001.slice"] {
        fs::remove_file(mount.join(path).join("cpu.pressure")).unwrap();
    }
    let pressure = cedegrid::kernel::cpu_pressure(&roots, None);
    assert_eq!(pressure.status, CpuPressureStatus::SystemOnly);
    assert_eq!(pressure.external_some_avg10, None);
    assert_eq!(pressure.system_some_avg10, Some(70.0));
}

/// Unreadable or malformed pressure, and a host without PSI, add no evidence.
#[test]
fn unreadable_or_malformed_pressure_is_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let (roots, mount) = psi_host(&base);
    for path in ["system.slice", "init.scope", "user.slice/user-1001.slice"] {
        write(&mount.join(path), "cpu.pressure", "some avg10=oops\n");
    }
    let pressure = cedegrid::kernel::cpu_pressure(&roots, None);
    assert_eq!(pressure.status, CpuPressureStatus::Unknown);
    assert_eq!(pressure.external_some_avg10, None);
    let bare = base.join("bare");
    write(&bare.join("proc/self"), "mountinfo", "");
    write(&bare.join("proc/self"), "cgroup", "0::/\n");
    let pressure = cedegrid::kernel::cpu_pressure(
        &ProbeRoots {
            proc: bare.join("proc"),
            sys: bare.join("sys"),
        },
        None,
    );
    assert_eq!(pressure.status, CpuPressureStatus::Unknown);
    assert_eq!(pressure.external_some_avg10, None);
    assert_eq!(pressure.system_some_avg10, None);
}

/// T141 native gate: on an authorized Linux host, external and own CPU pressure
/// are read without writing anything.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "not tested: requires an authorized Linux host with PSI (CEDEGRID_TEST_PSI=1)"]
fn native_external_cpu_pressure_is_read() {
    assert!(
        std::env::var_os("CEDEGRID_TEST_PSI").is_some(),
        "explicit CEDEGRID_TEST_PSI=1 is required"
    );
    let pressure = cedegrid::kernel::cpu_pressure(&ProbeRoots::default(), None);
    println!(
        "status {:?}, external cgroups {}, external {:?}, own {:?}, system {:?}: {}",
        pressure.status,
        pressure.external_cgroups,
        pressure.external_some_avg10,
        pressure.own_some_avg10,
        pressure.system_some_avg10,
        pressure.detail
    );
    assert_ne!(
        pressure.status,
        CpuPressureStatus::Unknown,
        "{}",
        pressure.detail
    );
}

/// `oom_kill` counts from `/proc/vmstat` and a leaf's `memory.events` (FR-076); a
/// missing or malformed count is unknown, never zero.
#[test]
fn oom_kill_counts_read_from_vmstat_and_memory_events() {
    use cedegrid::kernel::event_count;
    let vmstat = "pgfault 1000\noom_kill 3\npgmajfault 7\n";
    assert_eq!(event_count(vmstat, "oom_kill"), Some(3));
    let events = "low 0\nhigh 12\nmax 1\noom 1\noom_kill 1\noom_group_kill 0\n";
    assert_eq!(event_count(events, "oom_kill"), Some(1));
    assert_eq!(event_count(events, "oom"), Some(1));
    assert_eq!(event_count("pgfault 1000\n", "oom_kill"), None);
    assert_eq!(event_count("oom_kill x\n", "oom_kill"), None);
}
