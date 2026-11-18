//! Read-only, local telemetry. No workload execution or resource controls live here.
//!
//! CPU quantities use logical millicores. GPU utilization is kernel busy time,
//! never a measure of spare compute capacity or proof of interference.
use crate::model::{
    Capability, CapabilityStatus, ComputeActivity, ExternalGpuProcessIdentity,
    GpuExecutionCapability, GpuObservation, GpuSnapshot, PrincipalUsage, SCHEMA_VERSION, Snapshot,
};
use anyhow::{Result, ensure};
use nvml_wrapper::{Nvml, error::NvmlError, struct_wrappers::device::ProcessUtilizationSample};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{CpuRefreshKind, System};

mod gpu;
pub use gpu::GpuReleaseGuard;
pub use gpu::external_gpu_process_identity;
use gpu::*;
mod managed;
#[cfg(target_os = "linux")]
mod processes;
pub use managed::{CedegridCpu, ManagedCollector};
#[cfg(target_os = "linux")]
pub use managed::{own_process_cpu_ns, supervisor_cpu_ns};

const MIB: u64 = 1024 * 1024;

enum GpuCollector {
    Uninitialized,
    Ready(Box<Nvml>),
    Missing(Capability),
}

/// One local observer. NVML loads on the first sample and is optional at runtime.
/// Recreate the collector after changing driver/library availability.
pub struct Collector {
    node_id: String,
    system: System,
    last_cpu_refresh: Instant,
    last_cpu_refresh_started: Instant,
    cpu_window: Option<crate::kernel::CpuAccountingWindow>,
    physical_cores: Option<u32>,
    gpu: GpuCollector,
    gpu_cursors: BTreeMap<String, (u64, Instant)>,
    /// Each device's per-process sample state across intervals (FR-021, R46).
    gpu_samples: BTreeMap<String, GpuSampleState>,
    /// Each device's last readable per-process samples: when, and the busy contexts (FR-021).
    gpu_last_sampled: BTreeMap<String, LastSampled>,
    /// Each device's last active external compute reading and when (FR-021).
    gpu_last_activity: BTreeMap<String, (Instant, ComputeActivity)>,
    gpu_sample_max_age: Duration,
    gpu_config: crate::config::GpuConfig,
    platform_capabilities: BTreeMap<String, Capability>,
    kernel: crate::kernel::KernelCollector,
    managed_pids: BTreeSet<u32>,
    /// The verified identities behind `managed_pids`, for pidfd checks.
    managed_identities: BTreeMap<u32, crate::execution_model::ProcessIdentity>,
    /// Per device, memory attributed to no listed PID at the last measured,
    /// own-only or empty interval (research R18).
    gpu_memory_baseline: BTreeMap<String, u64>,
    managed_gpu_usage: BTreeMap<u32, BTreeMap<String, u64>>,
    gpu_accounting_known: BTreeSet<String>,
    gpu_quiet: BTreeMap<String, QuietState>,
    gpu_memory_tolerance_mib: u64,
    quiet_window_intervals: u32,
    attribution_tolerance_percent: u8,
    /// User IDs below this are system accounts; their idle GPU contexts are background.
    uid_min: u32,
    /// GPU compute's `principal_min_percent`, below which such a context is idle.
    gpu_principal_min_percent: u8,
    /// Every visible process, for principal aggregation only.
    processes: System,
    /// Linux principals and external processes, one `stat` read per process (research R26).
    #[cfg(target_os = "linux")]
    process_table: Option<processes::ProcessTable>,
    /// External context memory per device and PID from the last GPU sample.
    external_gpu_memory: BTreeMap<String, BTreeMap<u32, u64>>,
    /// Measured external GPU compute per device and PID from the last GPU sample.
    external_gpu_compute: BTreeMap<String, BTreeMap<u32, u64>>,
    /// External processes listed for memory peaks, by PID and start time.
    tracked_external: BTreeSet<(u32, u64)>,
}

/// One visible process as principal aggregation sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSample {
    pub pid: u32,
    /// `None` when the owner cannot be read.
    pub uid: Option<u32>,
    pub cpu_millicores: u64,
    pub ram_mib: u64,
    /// Seconds since the epoch; with the PID it identifies the process.
    pub start_time: u64,
    /// The parent PID, where the platform's table reads it.
    pub parent: Option<u32>,
    /// The CedeGrid marker of a task supervisor that counts as CedeGrid work (006 FR-001,
    /// FR-003).
    pub marker: Option<crate::execution_model::WorkMarker>,
}

/// External processes whose memory the policy tracks for admission peaks
/// (FR-060): every external GPU context and every process holding at least 1% of
/// host RAM. Once listed, a process stays listed while it lives, so its peak lasts
/// until it exits. At most the 64 largest are kept.
pub fn external_memory_processes(
    processes: &[ProcessSample],
    gpu_memory: &BTreeMap<String, BTreeMap<u32, u64>>,
    managed: &BTreeSet<u32>,
    tracked: &BTreeSet<(u32, u64)>,
    total_ram_mib: u64,
) -> Vec<crate::model::ExternalProcessMemory> {
    const LIMIT: usize = 64;
    let threshold = (total_ram_mib / 100).max(1);
    let started: BTreeMap<u32, u64> = processes.iter().map(|p| (p.pid, p.start_time)).collect();
    let mut listed: BTreeMap<(u32, u64), crate::model::ExternalProcessMemory> = BTreeMap::new();
    for process in processes {
        let id = (process.pid, process.start_time);
        if managed.contains(&process.pid) || (process.ram_mib < threshold && !tracked.contains(&id))
        {
            continue;
        }
        listed.insert(
            id,
            crate::model::ExternalProcessMemory {
                pid: process.pid,
                start_time: process.start_time,
                ram_mib: process.ram_mib,
                gpu_memory_mib: BTreeMap::new(),
            },
        );
    }
    for (uuid, contexts) in gpu_memory {
        for (pid, mib) in contexts {
            if managed.contains(pid) {
                continue;
            }
            let start_time = started.get(pid).copied().unwrap_or(0);
            let entry = listed.entry((*pid, start_time)).or_insert_with(|| {
                crate::model::ExternalProcessMemory {
                    pid: *pid,
                    start_time,
                    ..Default::default()
                }
            });
            entry.gpu_memory_mib.insert(uuid.clone(), *mib);
        }
    }
    let mut result: Vec<_> = listed.into_values().collect();
    if result.len() > LIMIT {
        result
            .sort_by_key(|p| std::cmp::Reverse(p.ram_mib + p.gpu_memory_mib.values().sum::<u64>()));
        result.truncate(LIMIT);
        result.sort_by_key(|p| (p.pid, p.start_time));
    }
    result
}

/// Group unmanaged processes into principals by owner UID (research R12).
/// Managed processes are own work and are left out. GPU contexts take the
/// owner of their PID; a PID outside the visible process table has an
/// unreadable owner, and all unreadable owners form one principal.
pub fn aggregate_principals(
    processes: &[ProcessSample],
    gpu_memory: &BTreeMap<String, BTreeMap<u32, u64>>,
    gpu_compute: &BTreeMap<String, BTreeMap<u32, u64>>,
    owner_uid: Option<u32>,
    managed: &BTreeSet<u32>,
) -> Vec<PrincipalUsage> {
    fn entry(
        principals: &mut BTreeMap<Option<u32>, PrincipalUsage>,
        uid: Option<u32>,
        owner_uid: Option<u32>,
    ) -> &mut PrincipalUsage {
        principals.entry(uid).or_insert_with(|| PrincipalUsage {
            uid,
            is_owner: uid.is_some() && uid == owner_uid,
            ..PrincipalUsage::default()
        })
    }
    let mut principals = BTreeMap::new();
    let mut owners = BTreeMap::new();
    for process in processes {
        owners.insert(process.pid, process.uid);
        if managed.contains(&process.pid) {
            continue;
        }
        let principal = entry(&mut principals, process.uid, owner_uid);
        principal.cpu_millicores = principal
            .cpu_millicores
            .saturating_add(process.cpu_millicores);
        principal.ram_mib = principal.ram_mib.saturating_add(process.ram_mib);
    }
    for (uuid, contexts) in gpu_memory {
        for (pid, mib) in contexts {
            if managed.contains(pid) {
                continue;
            }
            let usage = entry(
                &mut principals,
                owners.get(pid).copied().flatten(),
                owner_uid,
            )
            .gpu_memory_mib
            .entry(uuid.clone())
            .or_default();
            *usage = usage.saturating_add(*mib);
        }
    }
    // Other accounts' CedeGrid work, by marker (006 FR-002 to FR-005).
    let by_pid: BTreeMap<u32, &ProcessSample> = processes.iter().map(|p| (p.pid, p)).collect();
    let work_of = |pid: u32| cedegrid_work_of(&by_pid, pid);
    for process in processes {
        if managed.contains(&process.pid) || process.uid.is_none() || process.uid == owner_uid {
            continue;
        }
        if let Some(marker) = work_of(process.pid) {
            let work = entry(&mut principals, process.uid, owner_uid)
                .cedegrid_cpu
                .get_or_insert_with(Default::default);
            note_cedegrid_work(work, marker.arrival_unix_ms, marker.cpu);
        }
    }
    for (uuid, contexts) in gpu_memory.iter().chain(gpu_compute) {
        for pid in contexts.keys() {
            let uid = owners.get(pid).copied().flatten();
            if managed.contains(pid) || uid.is_none() || uid == owner_uid {
                continue;
            }
            if let Some(marker) = work_of(*pid) {
                let work = entry(&mut principals, uid, owner_uid)
                    .cedegrid_gpu
                    .entry(uuid.clone())
                    .or_default();
                note_cedegrid_work(work, marker.arrival_unix_ms, marker.gpu);
            }
        }
    }
    // A PID whose owner cannot be read stays with the unattributed principal.
    for (uuid, processes) in gpu_compute {
        for (pid, milli) in processes {
            if managed.contains(pid) {
                continue;
            }
            let usage = entry(
                &mut principals,
                owners.get(pid).copied().flatten(),
                owner_uid,
            )
            .gpu_compute_milli
            .entry(uuid.clone())
            .or_default();
            *usage = usage.saturating_add(*milli);
        }
    }
    principals.into_values().collect()
}

/// The marker in a task supervisor's `/proc/<pid>/cmdline`: a CedeGrid binary started as
/// `__assignment-supervisor <spec> <marker>` (006 FR-001). Anything else has none.
// Only Linux's process table reads markers; the tests run everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn supervisor_marker(cmdline: &[u8]) -> Option<crate::execution_model::WorkMarker> {
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    let program = std::str::from_utf8(args.first()?).ok()?;
    let name = program.rsplit('/').next()?;
    if !name.starts_with("cedegrid") || args.get(1).copied() != Some(b"__assignment-supervisor") {
        return None;
    }
    crate::execution_model::WorkMarker::parse(std::str::from_utf8(args.get(3)?).ok()?)
}

/// Whether a `/proc/<pid>/cgroup` text places the process inside one of `uid`'s units the
/// System mode broker made (006 FR-003).
// Only Linux's process table reads markers; the tests run everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn in_broker_unit(cgroup: &str, uid: u32) -> bool {
    let units = [
        format!("cedegrid-u{uid}.service"),
        format!("cedegrid-idle-u{uid}.service"),
    ];
    cgroup
        .lines()
        .filter_map(|line| line.strip_prefix("0::"))
        .any(|path| {
            path.split('/')
                .any(|part| units.iter().any(|unit| part == unit))
        })
}

/// The marker that makes `pid` CedeGrid work: its own, or its nearest marked ancestor's within
/// 32 levels, through processes of the same account only (006 FR-002).
fn cedegrid_work_of(
    by_pid: &BTreeMap<u32, &ProcessSample>,
    pid: u32,
) -> Option<crate::execution_model::WorkMarker> {
    let mut current = *by_pid.get(&pid)?;
    let uid = current.uid?;
    for _ in 0..32 {
        if current.uid != Some(uid) {
            return None;
        }
        if let Some(marker) = current.marker {
            return Some(marker);
        }
        current = by_pid.get(&current.parent?)?;
    }
    None
}

/// Keeps the earliest arrival, and the earliest among Performance or Aggressive work.
fn note_cedegrid_work(
    work: &mut crate::model::CedeGridWork,
    arrival_unix_ms: u64,
    mode: crate::contention::ContentionMode,
) {
    let earliest =
        |known: Option<u64>| Some(known.map_or(arrival_unix_ms, |k| k.min(arrival_unix_ms)));
    work.arrival_unix_ms = earliest(work.arrival_unix_ms);
    if crate::execution_model::WorkMarker::protected(mode) {
        work.protected_arrival_unix_ms = earliest(work.protected_arrival_unix_ms);
    }
}

/// The owner UID of a process, when the platform exposes it as a number.
fn process_uid(process: &sysinfo::Process) -> Option<u32> {
    #[cfg(unix)]
    {
        process.user_id().map(|uid| **uid)
    }
    #[cfg(not(unix))]
    {
        let _ = process;
        None
    }
}

/// The UID this process runs as, which is the owner principal.
fn owner_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions and cannot fail.
        Some(unsafe { libc::getuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

impl Collector {
    pub fn new(node_id: &str) -> Result<Self> {
        ensure!(!node_id.trim().is_empty(), "node_id must not be empty");
        let mut system = System::new();
        let last_cpu_refresh_started = Instant::now();
        system.refresh_cpu_list(CpuRefreshKind::nothing().with_cpu_usage());
        // The first CPU refresh establishes a baseline, not a utilization sample.
        let last_cpu_refresh = Instant::now();
        let physical_cores =
            visible_physical_cores(System::physical_core_count(), system.cpus().len());
        Ok(Self {
            node_id: node_id.to_owned(),
            system,
            last_cpu_refresh,
            last_cpu_refresh_started,
            cpu_window: None,
            physical_cores,
            gpu: GpuCollector::Uninitialized,
            gpu_cursors: BTreeMap::new(),
            gpu_samples: BTreeMap::new(),
            gpu_last_sampled: BTreeMap::new(),
            gpu_last_activity: BTreeMap::new(),
            gpu_sample_max_age: Duration::from_millis(
                crate::config::Config::default().gpu_sample_max_age_ms(),
            ),
            gpu_config: crate::config::GpuConfig::default(),
            platform_capabilities: platform_capabilities(),
            kernel: crate::kernel::KernelCollector::new(crate::kernel::KernelConfig::default()),
            managed_pids: BTreeSet::new(),
            managed_identities: BTreeMap::new(),
            gpu_memory_baseline: BTreeMap::new(),
            managed_gpu_usage: BTreeMap::new(),
            gpu_accounting_known: BTreeSet::new(),
            gpu_quiet: BTreeMap::new(),
            gpu_memory_tolerance_mib: crate::contention::ResourceThresholds::defaults(
                crate::contention::ResourceKind::GpuMemory,
            )
            .noise_tolerance_mib,
            quiet_window_intervals: crate::contention::ResourceThresholds::defaults(
                crate::contention::ResourceKind::GpuCompute,
            )
            .quiet_window_intervals,
            attribution_tolerance_percent: crate::contention::ResourceThresholds::defaults(
                crate::contention::ResourceKind::GpuCompute,
            )
            .attribution_tolerance_percent,
            uid_min: crate::config::Config::default().contention.uid_min,
            gpu_principal_min_percent: crate::contention::ResourceThresholds::defaults(
                crate::contention::ResourceKind::GpuCompute,
            )
            .principal_min_percent,
            processes: System::new(),
            #[cfg(target_os = "linux")]
            process_table: processes::ProcessTable::new(),
            external_gpu_memory: BTreeMap::new(),
            external_gpu_compute: BTreeMap::new(),
            tracked_external: BTreeSet::new(),
        })
    }

    /// Read-only usage of every visible process, grouped by owner, and the external
    /// processes tracked for memory peaks.
    #[allow(clippy::type_complexity)]
    fn sample_principals(
        &mut self,
    ) -> Option<(
        Vec<PrincipalUsage>,
        Vec<crate::model::ExternalProcessMemory>,
    )> {
        #[cfg(target_os = "linux")]
        if let Some(table) = self.process_table.as_mut() {
            // A table that could not be read is unknown, not a host without processes: no
            // list is reported and the tracked external processes stay tracked.
            let samples = table.sample()?;
            return Some(self.principals_from(&samples));
        }
        if !sysinfo::IS_SUPPORTED_SYSTEM {
            return None;
        }
        self.processes.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::All,
            true,
            sysinfo::ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_user(sysinfo::UpdateKind::OnlyIfNotSet),
        );
        let samples: Vec<ProcessSample> = self
            .processes
            .processes()
            .iter()
            .map(|(pid, process)| ProcessSample {
                pid: pid.as_u32(),
                uid: process_uid(process),
                // sysinfo reports percent of one CPU; 100% is 1000 millicores.
                cpu_millicores: (f64::from(process.cpu_usage()) * 10.0).round() as u64,
                ram_mib: process.memory() / MIB,
                start_time: process.start_time(),
                // Other platforms identify no CedeGrid work: every account is plain there.
                parent: None,
                marker: None,
            })
            .collect();
        Some(self.principals_from(&samples))
    }

    /// Principals and the external processes tracked for memory peaks, from one process sample.
    fn principals_from(
        &mut self,
        samples: &[ProcessSample],
    ) -> (
        Vec<PrincipalUsage>,
        Vec<crate::model::ExternalProcessMemory>,
    ) {
        let external = external_memory_processes(
            samples,
            &self.external_gpu_memory,
            &self.managed_pids,
            &self.tracked_external,
            self.system.total_memory() / MIB,
        );
        self.tracked_external = external.iter().map(|p| (p.pid, p.start_time)).collect();
        (
            aggregate_principals(
                samples,
                &self.external_gpu_memory,
                &self.external_gpu_compute,
                owner_uid(),
                &self.managed_pids,
            ),
            external,
        )
    }

    pub fn with_kernel(node_id: &str, config: crate::kernel::KernelConfig) -> Result<Self> {
        let mut collector = Self::new(node_id)?;
        collector.kernel = crate::kernel::KernelCollector::new(config);
        Ok(collector)
    }

    pub fn for_config(config: &crate::config::Config) -> Result<Self> {
        let mut collector = Self::with_kernel(&config.node_id, config.kernel.clone())?;
        collector.gpu_sample_max_age = Duration::from_millis(config.gpu_sample_max_age_ms());
        collector.gpu_config = config.gpu.clone();
        collector.gpu_memory_tolerance_mib = config
            .contention
            .thresholds(crate::contention::ResourceKind::GpuMemory)
            .noise_tolerance_mib;
        collector.quiet_window_intervals = config
            .contention
            .quiet_window_intervals(config.monitor.interval_ms, config.gpu_sample_max_age_ms());
        collector.attribution_tolerance_percent = config
            .contention
            .thresholds(crate::contention::ResourceKind::GpuCompute)
            .attribution_tolerance_percent;
        collector.uid_min = config.contention.uid_min;
        collector.gpu_principal_min_percent = config
            .contention
            .thresholds(crate::contention::ResourceKind::GpuCompute)
            .principal_min_percent;
        Ok(collector)
    }

    /// Earliest time a new aggregate CPU sample can satisfy the OS interval.
    /// Sampling earlier still reports unknown usage; callers may wait instead.
    pub(crate) fn next_cpu_sample_at(&self) -> Instant {
        self.last_cpu_refresh + sysinfo::MINIMUM_CPU_UPDATE_INTERVAL
    }

    /// Only identities independently verified by the allocation registry belong here.
    /// Callers must verify them again after sampling before trusting attribution.
    pub fn set_managed_pids(&mut self, pids: BTreeSet<u32>) {
        self.managed_pids = pids;
        self.managed_identities
            .retain(|pid, _| self.managed_pids.contains(pid));
    }
    /// Own verified processes, by PID, with the identities a pidfd check needs.
    pub fn set_managed_identities(
        &mut self,
        identities: BTreeMap<u32, crate::execution_model::ProcessIdentity>,
    ) {
        self.managed_pids = identities.keys().copied().collect();
        self.managed_identities = identities;
    }
    /// A process's GPU memory per device. A device whose accounting is unknown has no entry,
    /// so only that reading is unknown and CPU and RAM keep theirs.
    pub fn gpu_usage(
        &self,
        pid: u32,
        requested: &crate::model::Resources,
    ) -> BTreeMap<String, u64> {
        // Include detected undeclared contexts so the agent can reject envelope
        // violations. CUDA visibility remains an advisory execution setting.
        let mut usage = self
            .managed_gpu_usage
            .get(&pid)
            .cloned()
            .unwrap_or_default();
        for id in requested.gpu_memory_mib.keys() {
            usage.entry(id.clone()).or_default();
        }
        usage.retain(|id, _| self.gpu_accounting_known.contains(id));
        usage
    }

    /// Read a snapshot without sleeping. Calls faster than the OS sampling
    /// interval return unknown CPU usage instead of reusing the previous sample.
    pub fn sample(&mut self) -> Result<Snapshot> {
        let observed_at_unix_ms =
            u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
        let mut capabilities = self.platform_capabilities.clone();
        let capacity = (self.system.cpus().len() as u64).saturating_mul(1000);
        let elapsed = self.last_cpu_refresh.elapsed();
        self.cpu_window = None;
        let cpu_busy = if cpu_sample_ready(elapsed) && capacity > 0 && sysinfo::IS_SUPPORTED_SYSTEM
        {
            let started = Instant::now();
            self.system.refresh_cpu_usage();
            self.last_cpu_refresh = Instant::now();
            self.cpu_window = Some(crate::kernel::CpuAccountingWindow {
                started: self.last_cpu_refresh_started,
                ended: self.last_cpu_refresh,
                counter_interval: Duration::ZERO,
            });
            self.last_cpu_refresh_started = started;
            cpu_millicores(self.system.global_cpu_usage(), capacity)
        } else {
            None
        };
        // The OS percentage rounds up for the upper bound; its floor is the lower.
        let cpu_busy_lower = cpu_busy.map(|busy| {
            ((f64::from(self.system.global_cpu_usage()) / 100.0) * capacity as f64)
                .floor()
                .clamp(0.0, busy as f64) as u64
        });
        self.system.refresh_memory();
        let total_ram = self.system.total_memory() / MIB;
        let available_ram = sysinfo::IS_SUPPORTED_SYSTEM
            .then(|| available_ram_mib(self.system.total_memory(), self.system.available_memory()))
            .flatten();
        capabilities.insert("cpu_telemetry".into(), capability(
            if cpu_busy.is_some() { CapabilityStatus::Available } else { CapabilityStatus::Unknown },
            "OS-visible logical CPU time; initial/too-soon samples are unknown. Container quotas and affinity are not verified.",
        ));
        capabilities.insert("cpu_topology".into(), capability(
            if self.physical_cores.is_some() { CapabilityStatus::Available } else { CapabilityStatus::Unknown },
            "Physical core count from the OS; SMT sibling placement and heterogeneous core speeds are not inferred.",
        ));
        capabilities.insert("memory_telemetry".into(), capability(
            if available_ram.is_some() { CapabilityStatus::Available } else { CapabilityStatus::Unknown },
            "OS-reported available RAM; reservations remain scheduling policy, with no kernel enforcement applied.",
        ));
        if !sysinfo::IS_SUPPORTED_SYSTEM {
            for key in ["cpu_telemetry", "cpu_topology", "memory_telemetry"] {
                capabilities.insert(
                    key.into(),
                    capability(
                        CapabilityStatus::Unsupported,
                        "OS telemetry backend unsupported on this platform.",
                    ),
                );
            }
        }
        self.managed_gpu_usage.clear();
        self.gpu_accounting_known.clear();
        self.external_gpu_memory.clear();
        self.external_gpu_compute.clear();
        let (gpu_inventory, gpus) = self.sample_gpus(&mut capabilities, observed_at_unix_ms);
        let (principals, external_processes) = match self.sample_principals() {
            Some((principals, external)) => (Some(principals), Some(external)),
            None => (None, None),
        };
        Ok(Snapshot {
            schema_version: SCHEMA_VERSION,
            node_id: self.node_id.clone(),
            observed_at_unix_ms,
            cpu_capacity_millicores: capacity,
            physical_cores: self.physical_cores,
            cpu_busy_millicores: cpu_busy,
            cpu_busy_lower_millicores: cpu_busy_lower,
            managed_cpu_lower_millicores: None,
            managed_cpu_window_lower_millicores: None,
            cedegrid_cpu_lower_millicores: None,
            cedegrid_cpu_window_lower_millicores: None,
            total_ram_mib: total_ram,
            available_ram_mib: available_ram,
            gpu_inventory,
            gpus,
            capabilities,
            kernel: Some(self.kernel.sample()),
            principals,
            external_processes,
        })
    }

    fn sample_gpus(
        &mut self,
        capabilities: &mut BTreeMap<String, Capability>,
        observed_at_unix_ms: u64,
    ) -> (CapabilityStatus, Vec<GpuSnapshot>) {
        if matches!(self.gpu, GpuCollector::Uninitialized) {
            self.gpu = if cfg!(any(target_os = "linux", target_os = "windows")) {
                match Nvml::init() {
                    Ok(nvml) => GpuCollector::Ready(Box::new(nvml)),
                    Err(error) => GpuCollector::Missing(capability(
                        error_status(&error),
                        format!(
                            "Optional NVML backend unavailable: {error}. GPU capacity is unknown, not zero."
                        ),
                    )),
                }
            } else {
                GpuCollector::Missing(capability(
                    CapabilityStatus::Unsupported,
                    "NVML adapter supports Linux/Windows. GPU capacity on this platform is unknown.",
                ))
            };
        }
        let nvml = match &self.gpu {
            GpuCollector::Ready(nvml) => nvml,
            GpuCollector::Missing(capability) => {
                capabilities.insert("gpu_inventory".into(), capability.clone());
                return (capability.status.clone(), Vec::new());
            }
            GpuCollector::Uninitialized => unreachable!(),
        };
        let count = match nvml.device_count() {
            Ok(count) => count,
            Err(error) => {
                let status = error_status(&error);
                capabilities.insert(
                    "gpu_inventory".into(),
                    capability(status.clone(), format!("NVML enumeration failed: {error}")),
                );
                return (status, Vec::new());
            }
        };
        let mut gpus = Vec::new();
        let inventory = CapabilityStatus::Available;
        for index in 0..count {
            let device = match nvml.device_by_index(index) {
                Ok(device) => device,
                Err(error) => {
                    capabilities.insert(
                        format!("gpu_index_{index}"),
                        capability(error_status(&error), error.to_string()),
                    );
                    continue;
                }
            };
            let uuid = match device.uuid() {
                Ok(uuid) if !uuid.is_empty() => uuid,
                result => {
                    capabilities.insert(
                        format!("gpu_index_{index}"),
                        capability(
                            CapabilityStatus::Unavailable,
                            format!("Stable GPU UUID unavailable: {result:?}"),
                        ),
                    );
                    continue;
                }
            };
            let read = |memory: &std::result::Result<
                nvml_wrapper::struct_wrappers::device::MemoryInfo,
                NvmlError,
            >| {
                memory.as_ref().ok().map(|memory| DeviceMemoryRead {
                    total: memory.total,
                    free: memory.free,
                    reserved: memory.reserved,
                })
            };
            // Read device memory on both sides of process enumeration.
            let memory_before = device.memory_info();
            let utilization_percent = match device.utilization_rates() {
                Ok(utilization) if utilization.gpu <= 100 => {
                    capabilities.insert(format!("gpu:{uuid}:utilization"), capability(CapabilityStatus::Available, "Auxiliary kernel-busy-time percentage. Polls can share an internal sample interval."));
                    Some(utilization.gpu)
                }
                result => {
                    capabilities.insert(
                        format!("gpu:{uuid}:utilization"),
                        capability(
                            CapabilityStatus::Unknown,
                            format!("Utilization unavailable or invalid: {result:?}"),
                        ),
                    );
                    None
                }
            };
            // Capture authorized native identities before querying contexts; verify
            // them again afterward. This never registers or controls the competitor.
            let authorized = self
                .gpu_config
                .best_effort_external_processes
                .get(&uuid)
                .filter(|_| {
                    self.gpu_config.execution_mode
                        == crate::config::GpuExecutionMode::BestEffortOccupied
                });
            let before: Vec<_> = authorized
                .into_iter()
                .flatten()
                .filter(|identity| {
                    external_gpu_process_identity(identity.pid).as_ref() == Some(identity)
                })
                .cloned()
                .collect();
            let compute_info = device.running_compute_processes();
            let graphics_info = device.running_graphics_processes();
            let compute = compute_info
                .as_ref()
                .map(|p| p.iter().map(|p| p.pid).collect::<Vec<_>>());
            let graphics = graphics_info
                .as_ref()
                .map(|p| p.iter().map(|p| p.pid).collect::<Vec<_>>());
            let memory_after = device.memory_info();
            let memory = device_memory(
                read(&memory_before),
                read(&memory_after),
                self.gpu_memory_tolerance_mib,
            );
            let (total_memory_mib, used_memory_mib) = match memory {
                Some(memory) => {
                    capabilities.insert(format!("gpu:{uuid}:memory"), capability(CapabilityStatus::Available, format!("Device-wide bytes read before and after process enumeration; unavailable = total - free, including {} MiB reserved. Attribution known: {}.", memory.reserved_mib, memory.attribution_known)));
                    (memory.total_mib, Some(memory.unavailable_mib))
                }
                None => {
                    capabilities.insert(
                        format!("gpu:{uuid}:memory"),
                        capability(
                            CapabilityStatus::Unavailable,
                            format!("Memory unavailable or inconsistent: {memory_before:?}"),
                        ),
                    );
                    (0, None)
                }
            };
            let mut accounting_known = memory.is_some_and(|memory| memory.attribution_known);
            let external_process_ids = external_process_ids(
                compute.as_deref().ok(),
                graphics.as_deref().ok(),
                &self.managed_pids,
            );
            // Memory of every enumerated context; unattributed is what remains.
            let mut process_memory: Option<BTreeMap<u32, u64>> = Some(BTreeMap::new());
            if let (Ok(compute), Ok(graphics)) = (&compute_info, &graphics_info) {
                for process in compute.iter().chain(graphics) {
                    match (&mut process_memory, &process.used_gpu_memory) {
                        (Some(known), nvml_wrapper::enums::device::UsedGpuMemory::Used(bytes)) => {
                            let entry = known.entry(process.pid).or_default();
                            *entry = (*entry).max(bytes.div_ceil(MIB));
                        }
                        _ => process_memory = None,
                    }
                    if self.managed_pids.contains(&process.pid) {
                        match process.used_gpu_memory {
                            nvml_wrapper::enums::device::UsedGpuMemory::Used(bytes) => {
                                let entry = self
                                    .managed_gpu_usage
                                    .entry(process.pid)
                                    .or_default()
                                    .entry(uuid.clone())
                                    .or_default();
                                // The same PID may appear in both APIs; never sum it twice.
                                *entry = (*entry).max(bytes.div_ceil(MIB));
                            }
                            _ => {
                                accounting_known = false;
                                // Preserve the context key even when its size is unknown
                                // so undeclared-device envelope checks still see it.
                                self.managed_gpu_usage
                                    .entry(process.pid)
                                    .or_default()
                                    .entry(uuid.clone())
                                    .or_default();
                            }
                        }
                    }
                }
            } else {
                accounting_known = false;
                process_memory = None;
            }
            if let Some(contexts) = &process_memory {
                self.external_gpu_memory.insert(
                    uuid.clone(),
                    contexts
                        .iter()
                        .filter(|(pid, _)| !self.managed_pids.contains(pid))
                        .map(|(pid, mib)| (*pid, *mib))
                        .collect(),
                );
            }
            let unattributed_memory_mib = memory
                .filter(|memory| memory.attribution_known)
                .zip(process_memory.as_ref())
                .map(|(memory, processes)| {
                    memory
                        .unavailable_mib
                        .saturating_sub(processes.values().fold(0, |sum, n| sum.saturating_add(*n)))
                });
            capabilities.insert(format!("gpu:{uuid}:processes"), capability(
                if external_process_ids.is_some() { CapabilityStatus::Available } else { CapabilityStatus::Unknown },
                format!("Compute and graphics enumeration must both succeed; Only registry-verified managed identities are excluded; all other PIDs remain external. Compute error: {:?}; graphics error: {:?}", compute.as_ref().err(), graphics.as_ref().err()),
            ));
            if accounting_known {
                self.gpu_accounting_known.insert(uuid.clone());
            }
            let baseline = self.gpu_cursors.get(&uuid).copied();
            let baseline_age = baseline.map(|(_, at)| at.elapsed());
            let previous = fresh_cursor(
                baseline.map(|(cursor, _)| cursor),
                baseline_age,
                self.gpu_sample_max_age,
            );
            let samples = device.process_utilization_stats(previous);
            // The device's own samples over the same interval (R33).
            let device_samples = device.samples(
                nvml_wrapper::enum_wrappers::device::Sampling::GpuUtilization,
                previous,
            );
            // Include query latency in the freshness bound. Preserve the actual
            // submitted cursor in diagnostics even if it expires during the call.
            let baseline_age = baseline.map(|(_, at)| at.elapsed());
            let activity_baseline = fresh_cursor(previous, baseline_age, self.gpu_sample_max_age);
            // A system account's idle context is background, not activity (FR-021).
            let background = background_contexts(
                external_process_ids.as_deref(),
                activity_baseline,
                &samples,
                context_uid,
                self.uid_min,
                self.gpu_principal_min_percent,
            );
            let active_external: Option<Vec<u32>> =
                external_process_ids.as_ref().map(|pids: &Vec<u32>| {
                    pids.iter()
                        .copied()
                        .filter(|pid| !background.contains(pid))
                        .collect()
                });
            let (external_compute, compute_sample_id, next_cursor) = classify_activity(
                active_external.as_deref(),
                activity_baseline,
                samples.as_deref().ok(),
            );
            // The MPS control daemon may serve any device; its server and clients
            // appear in the device's process lists.
            let listed_mps = self
                .processes
                .processes()
                .values()
                .any(|process| is_mps_process(&process.name().to_string_lossy()));
            #[cfg(target_os = "linux")]
            let listed_mps = listed_mps || self.process_table.as_ref().is_some_and(|t| t.mps);
            let mps = listed_mps
                || compute
                    .iter()
                    .chain(&graphics)
                    .flatten()
                    .filter_map(|pid| process_name(*pid))
                    .any(|name| is_mps_process(&name));
            // Only own verified processes listed: external compute is zero and the
            // device's time is own work, without fresh samples (FR-021, R18).
            let queries_ok = compute_info.is_ok() && graphics_info.is_ok();
            let listed = merge_process_lists(compute.as_deref().ok(), graphics.as_deref().ok());
            // Background contexts may be listed beside own work, which must be listed too.
            let own_only = utilization_percent.is_some()
                && listed.iter().flatten().any(|pid| !background.contains(pid))
                && own_only_interval(
                    &OwnOnlyEvidence {
                        queries_ok,
                        listed: listed.as_deref().unwrap_or_default(),
                        mps,
                        unattributed_memory_mib,
                        baseline_mib: self.gpu_memory_baseline.get(&uuid).copied(),
                        tolerance_mib: self.gpu_memory_tolerance_mib,
                    },
                    |pid| {
                        background.contains(&pid)
                            || self
                                .managed_identities
                                .get(&pid)
                                .is_some_and(own_process_verified)
                    },
                );
            // The quiet window counts other accounts' contexts only: the owner's, with a readable
            // user ID, neither break a quiet tick nor restart the window (FR-027).
            let owner_pids: BTreeSet<u32> = owner_uid()
                .map(|owner| {
                    active_external
                        .iter()
                        .flatten()
                        .copied()
                        .filter(|pid| context_uid(*pid) == Some(owner))
                        .collect()
                })
                .unwrap_or_default();
            let quiet = self.gpu_quiet.entry(uuid.clone()).or_default();
            let quiet_ticks = advance_quiet(
                quiet,
                others_of(active_external.as_deref(), &owner_pids).as_deref(),
                own_only || quiet_tick(active_external.as_deref(), previous, &samples, &owner_pids),
            );
            let external_compute =
                quiet_activity(external_compute, quiet_ticks, self.quiet_window_intervals);
            let external_compute = carry_activity(
                external_compute,
                active_external.is_some(),
                self.gpu_last_activity.get(&uuid),
                Instant::now(),
                self.gpu_sample_max_age,
            );
            if external_compute == ComputeActivity::Active {
                self.gpu_last_activity
                    .insert(uuid.clone(), (Instant::now(), external_compute));
            }
            let (compute_amount, compute_by_pid) = match utilization_percent {
                Some(device) if own_only => {
                    self.gpu_samples
                        .entry(uuid.clone())
                        .or_default()
                        .interrupt();
                    (
                        ComputeAmount::Measured {
                            external: 0,
                            own: u64::from(device.min(100)) * 10,
                        },
                        BTreeMap::new(),
                    )
                }
                _ => measure_compute(
                    active_external.as_deref(),
                    &self.managed_pids,
                    activity_baseline,
                    &samples,
                    device_utilization(&device_samples, activity_baseline, utilization_percent),
                    mps,
                    self.attribution_tolerance_percent,
                    &background,
                    self.gpu_samples.entry(uuid.clone()).or_default(),
                ),
            };
            // The record is read before this interval's timestamps are kept (FR-021, Q17).
            let compute_record = compute_sampling(
                activity_baseline,
                &samples,
                &device_samples,
                device_utilization(&device_samples, activity_baseline, utilization_percent)
                    .map(|device| device.percent),
                self.gpu_samples.entry(uuid.clone()).or_default(),
            );
            // NVML_DEVICE_MIG_ENABLE is 1; a device without MIG reports NotSupported.
            let mig_enabled = device.mig_mode().map(|mode| mode.current == 1);
            let time_sliced = time_sliced(queries_ok, mps, &mig_enabled);
            let mut compute_contexts = compute_contexts(
                listed.as_deref().filter(|_| queries_ok),
                &self.managed_pids,
                &background,
                activity_baseline,
                &samples,
                context_uid,
                owner_uid(),
            );
            if let Some(contexts) = compute_contexts.as_mut() {
                contexts.others_quiet =
                    self.quiet_window_intervals > 0 && quiet_ticks >= self.quiet_window_intervals;
                carry_samples(
                    contexts,
                    self.gpu_last_sampled.get(&uuid),
                    Instant::now(),
                    self.gpu_sample_max_age,
                );
                if contexts.samples_readable {
                    self.gpu_last_sampled
                        .insert(uuid.clone(), (Instant::now(), contexts.sampled.clone()));
                }
            }
            // Each listed PID's newest sample starts its next sample's span (FR-021, R46).
            if let (Some(previous), Ok(fresh)) = (activity_baseline, &samples) {
                self.gpu_samples.entry(uuid.clone()).or_default().remember(
                    previous,
                    fresh,
                    listed.as_deref(),
                );
            }
            if sets_memory_baseline(
                own_only,
                queries_ok,
                listed.as_deref(),
                matches!(compute_amount, ComputeAmount::Measured { .. }),
            ) && let Some(unattributed) = unattributed_memory_mib
            {
                self.gpu_memory_baseline.insert(uuid.clone(), unattributed);
            }
            if matches!(compute_amount, ComputeAmount::Measured { .. }) {
                self.external_gpu_compute
                    .insert(uuid.clone(), compute_by_pid);
            }
            if let Some(cursor) = next_cursor {
                // A successful query refreshes the cursor's age even with no newer sample; an
                // unchanged timestamp after a failed one does not.
                let queried = matches!(&samples, Ok(_) | Err(NvmlError::NotFound));
                if previous.is_none()
                    || refreshes_cursor(baseline.map(|(old, _)| old), cursor, queried)
                {
                    self.gpu_cursors
                        .insert(uuid.clone(), (cursor, Instant::now()));
                }
            }
            let mut observation = describe_gpu_observation(
                memory.is_some() && external_process_ids.is_some(),
                previous,
                baseline_age,
                &samples,
                compute_sample_id,
                observed_at_unix_ms,
                self.gpu_sample_max_age,
            );
            if let Some(authorized) = authorized {
                observation.best_effort_external_processes = verify_external_gpu_inventory(
                    external_process_ids.as_deref(),
                    authorized,
                    &before,
                    external_gpu_process_identity,
                );
                capabilities.insert(format!("gpu:{uuid}:best_effort_occupied"), capability(
                    if observation.best_effort_external_processes.is_some() { CapabilityStatus::Available } else { CapabilityStatus::Unavailable },
                    format!("Explicit device-scoped authorization; native boot/start-ticks/UID/PID identities verified before and after complete compute/graphics inventory: {}. External activity remains {:?}; unavailable activity is never zero. External memory remains charged. Interference/slowdown are unmeasured; no hard GPU isolation or continuity guarantee. Basic device accessibility and policy admission are checked separately.", observation.best_effort_external_processes.is_some(), external_compute),
                ));
            }
            capabilities.insert(
                format!("gpu:{uuid}:process_activity"),
                capability(
                    match &samples {
                        Ok(_) if compute_sample_id.is_some() => CapabilityStatus::Available,
                        Ok(_) => CapabilityStatus::Unknown,
                        Err(error) => error_status(error),
                    },
                    serde_json::to_string(&observation).expect("GPU observation is serializable"),
                ),
            );
            capabilities.insert(format!("gpu:{uuid}:execution_mode"), capability(
                if observation.capability == GpuExecutionCapability::InsufficientObservability {
                    CapabilityStatus::Unavailable
                } else { CapabilityStatus::Available },
                format!("{:?}; successful memory and complete compute/graphics queries establish basic device accessibility, not a hardware-health certification. Admission and configured mode are checked separately.", observation.capability),
            ));
            gpus.push(GpuSnapshot {
                uuid,
                total_memory_mib,
                used_memory_mib,
                utilization_percent,
                external_process_ids,
                external_compute,
                compute_sample_id,
                observation: Some(observation),
                reserved_memory_mib: memory.map(|memory| memory.reserved_mib),
                free_memory_mib: memory.map(|memory| memory.free_mib),
                unattributed_memory_mib,
                external_compute_milli: match &compute_amount {
                    ComputeAmount::Measured { external, .. } => Some(*external),
                    ComputeAmount::Unknown(_) => None,
                },
                own_compute_milli: match &compute_amount {
                    ComputeAmount::Measured { own, .. } => Some(*own),
                    ComputeAmount::Unknown(_) => None,
                },
                compute_amount_unknown: match compute_amount {
                    ComputeAmount::Measured { .. } => None,
                    ComputeAmount::Unknown(reason) => Some(reason),
                },
                own_only,
                background_process_ids: background.into_iter().collect(),
                compute_sampling: compute_record,
                time_sliced,
                compute_contexts,
            });
        }
        // A failed per-device query withholds that device's capacity only. Device
        // UUIDs remain stable keys; an unidentifiable index is never schedulable.
        capabilities.insert("gpu_inventory".into(), capability(inventory.clone(), "Stable UUID observations for NVML devices. Individual failures remain per-device diagnostics and never disable other qualified devices. Other vendor adapters are not implemented."));
        (inventory, gpus)
    }
}

/// Whether a listed PID is the verified own process it claims to be, by pidfd.
#[cfg(target_os = "linux")]
fn own_process_verified(identity: &crate::execution_model::ProcessIdentity) -> bool {
    managed::verified_pidfd(identity).is_some()
}

#[cfg(not(target_os = "linux"))]
fn own_process_verified(_identity: &crate::execution_model::ProcessIdentity) -> bool {
    false
}

/// The host's `oom_kill` count from `/proc/vmstat`, where the kernel provides one (FR-076).
#[cfg(target_os = "linux")]
pub fn host_oom_kills() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/vmstat").ok()?;
    crate::kernel::event_count(&text, "oom_kill")
}

#[cfg(not(target_os = "linux"))]
pub fn host_oom_kills() -> Option<u64> {
    None
}

/// Read-only capability discovery; never applies an available control.
pub fn preflight() -> Result<BTreeMap<String, Capability>> {
    Ok(Collector::new("preflight")?.sample()?.capabilities)
}

fn capability(status: CapabilityStatus, detail: impl Into<String>) -> Capability {
    Capability {
        status,
        detail: detail.into(),
        enforced: false,
    }
}

fn cpu_sample_ready(elapsed: Duration) -> bool {
    elapsed >= sysinfo::MINIMUM_CPU_UPDATE_INTERVAL
}

fn cpu_millicores(percent: f32, capacity: u64) -> Option<u64> {
    (percent.is_finite() && (0.0..=100.0).contains(&percent) && capacity > 0)
        .then(|| ((f64::from(percent) / 100.0) * capacity as f64).ceil() as u64)
}

fn visible_physical_cores(physical: Option<usize>, logical: usize) -> Option<u32> {
    physical
        .filter(|count| *count > 0 && *count <= logical)
        .and_then(|count| u32::try_from(count).ok())
}

fn available_ram_mib(total: u64, available: u64) -> Option<u64> {
    (total >= MIB && available <= total).then_some(available / MIB)
}

fn platform_capabilities() -> BTreeMap<String, Capability> {
    let mut capabilities = BTreeMap::new();
    capabilities.insert("observation".into(), capability(CapabilityStatus::Available, "Read-only local collection; no workload execution, signaling, networking, or kernel setting changes."));
    capabilities.insert(
        "gpu_other_vendors".into(),
        capability(
            CapabilityStatus::Unsupported,
            "Non-NVML GPU adapters are not yet implemented; unobserved devices are never admitted.",
        ),
    );
    capabilities.insert("process_supervision".into(), capability(if cfg!(any(target_os="linux",target_os="macos")){CapabilityStatus::Available}else{CapabilityStatus::Unsupported}, "Opt-in supervisor API exists; this observation does not launch work. Rootless requires an explicit single-process/no-escape contract; per-allocation launch evidence reports actual handles and controls."));
    capabilities.insert("pidfd".into(), pidfd_capability());
    for key in [
        "cgroup_delegation",
        "cpu_limit",
        "memory_limit",
        "cgroup_kill",
    ] {
        capabilities.insert(key.into(), capability(
            if cfg!(target_os = "linux") { CapabilityStatus::Unknown } else { CapabilityStatus::Unsupported },
            "No delegated cgroup v2 subtree or controller permissions verified. Configuration alone never implies enforcement.",
        ));
    }
    capabilities
}

#[cfg(target_os = "linux")]
pub(crate) fn pidfd_capability() -> Capability {
    use std::os::fd::{FromRawFd, OwnedFd};
    // SAFETY: pidfd_open obtains a handle to this observer, never signals it.
    // The returned descriptor is owned exactly once and closed on scope exit.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, std::process::id(), 0u32) };
    if fd >= 0 {
        let _fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        capability(
            CapabilityStatus::Available,
            "pidfd_open succeeded for this observer. Signal sending and workload supervision are not applied.",
        )
    } else {
        let error = std::io::Error::last_os_error();
        capability(
            if error.raw_os_error() == Some(libc::ENOSYS) {
                CapabilityStatus::Unsupported
            } else {
                CapabilityStatus::Unavailable
            },
            format!("Read-only pidfd_open probe: {error}"),
        )
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn pidfd_capability() -> Capability {
    capability(
        CapabilityStatus::Unsupported,
        "Linux pidfd interface is not available on this platform.",
    )
}

#[cfg(test)]
mod tests {
    use super::managed::*;
    use super::*;

    /// The Tracked PSS period (FR-085, Q16, R45): K keeps PSS reading at most 0.5% of the
    /// scope's CPU on average, from 1 to 10, from the cost of the last timed pass.
    #[test]
    fn the_pss_period_follows_the_measured_cost() {
        let ms = Duration::from_millis;
        // 4 CPUs at 500 ms intervals: 10 ms of PSS reading per interval on average.
        let scope = ms(2000);
        assert_eq!(pss_period(ms(43), scope), 5);
        assert_eq!(pss_period(ms(10), scope), 1);
        assert_eq!(pss_period(ms(3), scope), 1);
        assert_eq!(pss_period(Duration::ZERO, scope), 1);
        assert_eq!(pss_period(ms(1000), scope), 10);
        assert_eq!(pss_period(ms(1), Duration::ZERO), 10);
    }

    /// Between passes each member keeps its last PSS and the family adds its RSS growth since
    /// that pass, never a fall; the period's last interval reads again, and so does any change
    /// in the members.
    #[test]
    fn family_pss_is_read_every_k_intervals_and_estimated_between() {
        const MIB: u64 = 1 << 20;
        let members = |list: &[(u32, u64)]| list.iter().copied().collect::<BTreeSet<_>>();
        let family = members(&[(10, 1), (11, 2)]);
        let mut pss = FamilyPss::default();
        assert!(pss.due(&family), "the first interval reads");
        pss.passed(
            BTreeMap::from([((10, 1), (900, 1000 * MIB)), ((11, 2), (130, 1000 * MIB))]),
            Duration::from_millis(25),
            Duration::from_millis(2000),
        );
        assert_eq!(pss.period(), 3);
        assert!(!pss.due(&family));
        // RSS grew by 64 MiB in one member and fell by 16 MiB in the other.
        let (held, growth) = pss.estimate(&BTreeMap::from([
            ((10, 1), 1064 * MIB),
            ((11, 2), 984 * MIB),
        ]));
        assert_eq!(held, BTreeMap::from([((10, 1), 900), ((11, 2), 130)]));
        assert_eq!(growth, 48);
        assert!(!pss.due(&family));
        let (_, fell) = pss.estimate(&BTreeMap::from([
            ((10, 1), 900 * MIB),
            ((11, 2), 900 * MIB),
        ]));
        assert_eq!(fell, 0);
        assert!(pss.due(&family), "the period's third interval reads again");
        let mut joined = family.clone();
        joined.insert((12, 3));
        let mut fresh = FamilyPss::default();
        fresh.passed(
            BTreeMap::from([((10, 1), (900, MIB)), ((11, 2), (130, MIB))]),
            Duration::from_millis(25),
            Duration::from_millis(2000),
        );
        assert!(!fresh.due(&family));
        assert!(fresh.due(&joined), "a new member forces a pass");
        assert!(fresh.due(&members(&[(10, 1)])), "so does one that left");
    }

    /// Only a Tracked family of more than its leader is read on a period; a family of one and
    /// a Full leaf keep their readings.
    #[test]
    fn only_tracked_families_take_the_pss_period() {
        assert!(family_pss_applies(2, false));
        assert!(!family_pss_applies(1, false));
        assert!(!family_pss_applies(5, true));
    }

    fn process(pid: u32, uid: Option<u32>, cpu_millicores: u64, ram_mib: u64) -> ProcessSample {
        ProcessSample {
            pid,
            uid,
            cpu_millicores,
            ram_mib,
            start_time: u64::from(pid) * 10,
            parent: None,
            marker: None,
        }
    }

    #[test]
    fn external_memory_processes_are_tracked_while_they_live() {
        // 1% of 32000 MiB is 320 MiB.
        let processes = [
            process(1, Some(1000), 0, 4000),
            process(2, Some(1001), 0, 100),
            process(3, Some(1001), 0, 50),
            // Managed work is own work, never external.
            process(6, Some(1000), 0, 9000),
        ];
        let gpu = BTreeMap::from([("GPU-a".to_string(), BTreeMap::from([(3, 512)]))]);
        let managed = BTreeSet::from([6]);
        let listed = |tracked: &BTreeSet<(u32, u64)>, processes: &[ProcessSample]| {
            external_memory_processes(processes, &gpu, &managed, tracked, 32_000)
        };
        let first = listed(&BTreeSet::new(), &processes);
        let ids: Vec<_> = first.iter().map(|p| (p.pid, p.start_time)).collect();
        // The large process and every GPU context, by identity.
        assert_eq!(ids, [(1, 10), (3, 30)]);
        assert_eq!(first[1].gpu_memory_mib["GPU-a"], 512);
        // Once listed, a process stays while it lives, however small it gets.
        let tracked: BTreeSet<_> = ids.into_iter().collect();
        let shrunk = [
            process(1, Some(1000), 0, 10),
            process(2, Some(1001), 0, 100),
        ];
        let gpu_gone = BTreeMap::new();
        let second = external_memory_processes(&shrunk, &gpu_gone, &managed, &tracked, 32_000);
        assert_eq!(
            second
                .iter()
                .map(|p| (p.pid, p.ram_mib))
                .collect::<Vec<_>>(),
            [(1, 10)]
        );
        // A reused PID with another start time is another process.
        let reused = [ProcessSample {
            start_time: 99,
            ..process(1, Some(1000), 0, 10)
        }];
        assert!(
            external_memory_processes(&reused, &gpu_gone, &managed, &tracked, 32_000).is_empty()
        );
    }

    #[test]
    fn at_most_the_64_largest_external_processes_are_listed() {
        // 70 processes above 1% of RAM: the six smallest stay in the unlisted rest.
        let processes: Vec<_> = (1..=70)
            .map(|pid| process(pid, Some(1001), 0, 400 + u64::from(pid)))
            .collect();
        let listed = external_memory_processes(
            &processes,
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            32_000,
        );
        assert_eq!(listed.len(), 64);
        assert!(listed.iter().all(|p| p.pid > 6), "{listed:?}");
    }

    /// Another account's CedeGrid work, a marked supervisor of that account and what descends
    /// from it, makes it a CedeGrid principal with its earliest arrivals; GPU contexts count per
    /// device. Unmarked work, work under another account's supervisor and the owner stay plain
    /// (006 FR-002 to FR-005).
    #[test]
    fn other_accounts_cedegrid_work_is_identified_by_its_marker() {
        use crate::contention::ContentionMode::{Neutral, Performance};
        use crate::execution_model::WorkMarker;
        use crate::model::CedeGridWork;
        let marked = |cpu, gpu, arrival_unix_ms| {
            Some(WorkMarker {
                cpu,
                gpu,
                arrival_unix_ms,
            })
        };
        let processes = [
            // 1001: a Neutral run that arrived at 500 and a Performance run at 900, with a task
            // and the task's own child.
            ProcessSample {
                marker: marked(Neutral, Neutral, 500),
                ..process(10, Some(1001), 5, 1)
            },
            ProcessSample {
                parent: Some(10),
                ..process(11, Some(1001), 900, 100)
            },
            ProcessSample {
                marker: marked(Performance, Performance, 900),
                ..process(20, Some(1001), 5, 1)
            },
            ProcessSample {
                parent: Some(20),
                ..process(21, Some(1001), 1000, 100)
            },
            ProcessSample {
                parent: Some(21),
                ..process(22, Some(1001), 0, 1)
            },
            // 1002 starts its work by hand.
            process(30, Some(1002), 2000, 50),
            // A 1003 process under 1001's supervisor is not 1003's CedeGrid work.
            ProcessSample {
                parent: Some(20),
                ..process(40, Some(1003), 100, 1)
            },
            // The owner's own markers make no principal of it.
            ProcessSample {
                marker: marked(Performance, Performance, 100),
                ..process(50, Some(1000), 1, 1)
            },
        ];
        let gpu = BTreeMap::from([
            ("GPU-a".to_string(), BTreeMap::from([(22, 512)])),
            ("GPU-b".to_string(), BTreeMap::from([(30, 256), (11, 64)])),
        ]);
        let compute = BTreeMap::from([("GPU-a".to_string(), BTreeMap::from([(22, 900)]))]);
        let principals =
            aggregate_principals(&processes, &gpu, &compute, Some(1000), &BTreeSet::new());
        let by = |uid| principals.iter().find(|p| p.uid == Some(uid)).unwrap();
        assert_eq!(
            by(1001).cedegrid_cpu,
            Some(CedeGridWork {
                arrival_unix_ms: Some(500),
                protected_arrival_unix_ms: Some(900),
            })
        );
        assert_eq!(
            by(1001).cedegrid_gpu,
            BTreeMap::from([
                (
                    "GPU-a".into(),
                    CedeGridWork {
                        arrival_unix_ms: Some(900),
                        protected_arrival_unix_ms: Some(900),
                    }
                ),
                (
                    "GPU-b".into(),
                    CedeGridWork {
                        arrival_unix_ms: Some(500),
                        protected_arrival_unix_ms: None,
                    }
                ),
            ])
        );
        for plain in [1002, 1003, 1000] {
            assert_eq!(by(plain).cedegrid_cpu, None, "{plain}");
            assert!(by(plain).cedegrid_gpu.is_empty(), "{plain}");
        }
    }

    /// 006 SC-008: identification adds little to a sample: with 200 other processes, a quarter
    /// of them CedeGrid work under 25 supervisors, the fastest of 50 aggregations takes under a
    /// millisecond, which is 0.2% of one CPU at a 500 ms interval. Command lines are read once
    /// per process by the table, not here.
    #[test]
    fn identification_costs_under_a_millisecond_at_200_processes() {
        use crate::contention::ContentionMode::Performance;
        use crate::execution_model::WorkMarker;
        let mut processes: Vec<ProcessSample> = (1..=200)
            .map(|pid| process(pid, Some(1001 + pid % 8), 10, 10))
            .collect();
        for supervisor in processes.iter_mut().filter(|p| p.pid % 8 == 0) {
            supervisor.marker = Some(WorkMarker {
                cpu: Performance,
                gpu: Performance,
                arrival_unix_ms: u64::from(supervisor.pid),
            });
        }
        // Each supervisor's account runs a task under it, and the rest of its processes do not.
        for child in processes.iter_mut().filter(|p| p.pid % 8 == 4) {
            child.parent = Some(child.pid + 4);
            child.uid = Some(1001);
        }
        let gpu = BTreeMap::new();
        let fastest = (0..50)
            .map(|_| {
                let started = std::time::Instant::now();
                let principals =
                    aggregate_principals(&processes, &gpu, &gpu, Some(1000), &BTreeSet::new());
                assert!(principals.iter().any(|p| p.cedegrid_cpu.is_some()));
                started.elapsed()
            })
            .min()
            .unwrap();
        assert!(fastest < std::time::Duration::from_millis(1), "{fastest:?}");
    }

    #[test]
    fn external_processes_aggregate_into_principals_by_owner() {
        let processes = [
            process(1, Some(1000), 200, 10),
            process(2, Some(1001), 300, 20),
            process(3, Some(1001), 100, 30),
            process(4, None, 50, 1),
            process(5, None, 70, 2),
            // Managed work is own work, never a principal's usage.
            process(6, Some(1000), 900, 500),
        ];
        let gpu = BTreeMap::from([(
            "GPU-a".to_string(),
            // PID 99 is outside the visible process table, so its owner is unreadable.
            BTreeMap::from([(2, 512), (99, 256)]),
        )]);
        let compute = BTreeMap::from([(
            "GPU-a".to_string(),
            BTreeMap::from([(1, 50), (2, 400), (6, 300), (99, 100)]),
        )]);
        let principals =
            aggregate_principals(&processes, &gpu, &compute, Some(1000), &BTreeSet::from([6]));
        assert_eq!(
            principals,
            vec![
                PrincipalUsage {
                    uid: None,
                    is_owner: false,
                    cpu_millicores: 120,
                    ram_mib: 3,
                    gpu_memory_mib: BTreeMap::from([("GPU-a".into(), 256)]),
                    gpu_compute_milli: BTreeMap::from([("GPU-a".into(), 100)]),
                    ..PrincipalUsage::default()
                },
                PrincipalUsage {
                    uid: Some(1000),
                    is_owner: true,
                    cpu_millicores: 200,
                    ram_mib: 10,
                    gpu_memory_mib: BTreeMap::new(),
                    gpu_compute_milli: BTreeMap::from([("GPU-a".into(), 50)]),
                    ..PrincipalUsage::default()
                },
                PrincipalUsage {
                    uid: Some(1001),
                    is_owner: false,
                    cpu_millicores: 400,
                    ram_mib: 50,
                    gpu_memory_mib: BTreeMap::from([("GPU-a".into(), 512)]),
                    gpu_compute_milli: BTreeMap::from([("GPU-a".into(), 400)]),
                    ..PrincipalUsage::default()
                },
            ]
        );
    }

    /// Own RAM counts shared memory: `Pss_Anon` and `Pss_Shmem` from `smaps_rollup`, and a
    /// leaf's `anon` and `shmem` from `memory.stat`, without its page cache.
    #[test]
    fn own_ram_counts_shared_memory() {
        let rollup = "Rss:  9000 kB\nPss:  8000 kB\nPss_Anon:  2048 kB\nPss_File:  4096 kB\nPss_Shmem:  3072 kB\n";
        assert_eq!(own_memory_from_rollup(rollup), Some(5));
        // An older kernel lists no Pss_Shmem.
        assert_eq!(own_memory_from_rollup("Pss_Anon:  2048 kB\n"), Some(2));
        assert_eq!(own_memory_from_rollup("Pss_Shmem:  3072 kB\n"), None);
        let leaf = crate::namespace::TestStateDir(
            tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap(),
        );
        std::fs::write(
            leaf.path().join("memory.stat"),
            "anon 5242880\nfile 104857600\nkernel 1048576\nshmem 2097152\nanon_thp 0\n",
        )
        .unwrap();
        assert_eq!(leaf_memory_mib(leaf.path()), Some(7));
        std::fs::write(leaf.path().join("memory.stat"), "file 104857600\n").unwrap();
        assert_eq!(leaf_memory_mib(leaf.path()), None);
    }

    /// A per-process sample cursor's age restarts on a newer timestamp and on a successful query
    /// with nothing newer, never on a failed one.
    #[test]
    fn a_successful_query_without_new_samples_refreshes_the_cursor() {
        assert!(refreshes_cursor(None, 100, false));
        assert!(refreshes_cursor(Some(100), 120, true));
        assert!(refreshes_cursor(Some(100), 100, true));
        assert!(!refreshes_cursor(Some(100), 100, false));
    }

    fn gpu_sample(pid: u32, timestamp: u64, sm_util: u32) -> ProcessUtilizationSample {
        ProcessUtilizationSample {
            pid,
            timestamp,
            sm_util,
            mem_util: 0,
            enc_util: 0,
            dec_util: 0,
        }
    }

    /// A system account's context (user ID below `uid_min`) is background while per-process
    /// utilization is readable and its own stays below `principal_min_percent`; a user's
    /// context, an unreadable query or one without a fresh baseline never is (FR-021).
    #[test]
    fn system_account_contexts_are_background_only_while_readable_and_idle() {
        let uid = |pid: u32| match pid {
            10 | 12 => Some(120),
            11 => Some(1000),
            _ => None,
        };
        let fresh: std::result::Result<_, NvmlError> = Ok(vec![
            gpu_sample(10, 110, 2),
            gpu_sample(12, 110, 20),
            gpu_sample(11, 110, 50),
            // Older than the cursor: not this interval.
            gpu_sample(10, 90, 90),
        ]);
        assert_eq!(
            background_contexts(Some(&[10, 11, 12, 13]), Some(100), &fresh, uid, 1000, 5),
            BTreeSet::from([10])
        );
        // No samples: every system context is idle.
        assert_eq!(
            background_contexts(
                Some(&[10, 11, 12]),
                Some(100),
                &Err(NvmlError::NotFound),
                uid,
                1000,
                5
            ),
            BTreeSet::from([10, 12])
        );
        assert!(
            background_contexts(
                Some(&[10]),
                Some(100),
                &Err(NvmlError::NotSupported),
                uid,
                1000,
                5
            )
            .is_empty()
        );
        assert!(background_contexts(Some(&[10]), None, &fresh, uid, 1000, 5).is_empty());
        assert!(background_contexts(None, Some(100), &fresh, uid, 1000, 5).is_empty());
    }

    /// Background contexts count as neither external compute nor unknown: own work keeps the
    /// device's time less theirs, and listing only them is idle (FR-021).
    #[test]
    fn background_contexts_are_not_external_compute() {
        let own = BTreeSet::from([7]);
        let background = BTreeSet::from([42]);
        let samples = Ok(vec![gpu_sample(42, 110, 3), gpu_sample(7, 110, 60)]);
        assert_eq!(
            measure_compute(
                Some(&[]),
                &own,
                Some(100),
                &samples,
                Some(63.into()),
                false,
                15,
                &background,
                &mut GpuSampleState::default()
            )
            .0,
            ComputeAmount::Measured {
                external: 0,
                own: 600
            }
        );
        let both = Ok(vec![
            gpu_sample(42, 110, 3),
            gpu_sample(43, 110, 20),
            gpu_sample(7, 110, 60),
        ]);
        let (amount, by_pid) = measure_compute(
            Some(&[43]),
            &own,
            Some(100),
            &both,
            Some(83.into()),
            false,
            15,
            &background,
            &mut GpuSampleState::default(),
        );
        assert_eq!(
            amount,
            ComputeAmount::Measured {
                external: 200,
                own: 600
            }
        );
        assert_eq!(by_pid, BTreeMap::from([(43, 200)]));
        assert_eq!(
            classify_activity(Some(&[]), Some(100), None).0,
            ComputeActivity::Idle
        );
    }

    fn device_sample(
        timestamp: u64,
        percent: u32,
    ) -> nvml_wrapper::struct_wrappers::device::Sample {
        nvml_wrapper::struct_wrappers::device::Sample {
            timestamp,
            value: nvml_wrapper::enums::device::SampleValue::U32(percent),
        }
    }

    /// Consecutive 500 ms intervals as the 034 rerun recorded them: the device sampled 3 times an
    /// interval, each process once, the cursor following the newest per-process sample.
    struct Intervals {
        state: GpuSampleState,
        cursor: u64,
    }

    impl Intervals {
        const W: u64 = 500_000;
        const PERIOD: u64 = 166_667;
        fn new() -> Self {
            Self {
                state: GpuSampleState::default(),
                cursor: 1_000_000,
            }
        }
        /// One interval: `samples` are (PID, device periods before the window's end, sm_util),
        /// `listed` the external PIDs listed, 7 being own.
        fn next(
            &mut self,
            samples: &[(u32, u64, u32)],
            device_percent: u32,
            listed: &[u32],
        ) -> ComputeAmount {
            let end = self.cursor + Self::W;
            let device: Vec<_> = (1..=3)
                .map(|i| device_sample(self.cursor + i * Self::W / 3, device_percent))
                .collect();
            let fresh: Vec<_> = samples
                .iter()
                .map(|(pid, before, sm)| gpu_sample(*pid, end - before * Self::PERIOD, *sm))
                .collect();
            let (amount, _) = measure_compute(
                Some(listed),
                &BTreeSet::from([7]),
                Some(self.cursor),
                &Ok(fresh.clone()),
                device_utilization(&Ok(device), Some(self.cursor), Some(device_percent)),
                false,
                15,
                &BTreeSet::new(),
                &mut self.state,
            );
            let all: Vec<u32> = listed.iter().copied().chain([7]).collect();
            self.state.remember(self.cursor, &fresh, Some(&all));
            self.cursor = fresh
                .iter()
                .map(|s| s.timestamp)
                .max()
                .unwrap_or(self.cursor)
                .max(self.cursor);
            amount
        }
    }

    /// A measured shape (FR-021, Q17, R46): the other user's process sampled at the window's
    /// end, the duty-capped own probe 1 or 2 device periods before it, once each per interval.
    /// Each sample counts its whole span since its process's previous one, at most one window, so
    /// after the first interval, whose probe sample covers only from the window's start, both
    /// read in full: 5% and 50% against the device's 55%. Dividing by the device's 3 samples read
    /// 2% and 17%.
    #[test]
    fn a_gpu_sample_counts_its_span_since_its_previous_sample() {
        for before in [1, 2] {
            let mut run = Intervals::new();
            let first = run.next(&[(42, 0, 5), (7, before, 50)], 55, &[42]);
            assert!(
                !matches!(first, ComputeAmount::Measured { own: 500, .. }),
                "the first interval cannot know the probe's span: {first:?}"
            );
            for _ in 0..3 {
                assert_eq!(
                    run.next(&[(42, 0, 5), (7, before, 50)], 55, &[42]),
                    ComputeAmount::Measured {
                        external: 50,
                        own: 500
                    },
                    "{before} periods before the end"
                );
            }
        }
    }

    /// The span's edges (FR-021, Q17): a process idle for 3 intervals and then busy counts at most
    /// one window; one first seen mid-run, or a PID reused after it left the device's list, covers
    /// from the window's start; own work stopped after half the window counts half; an agent
    /// restart loses the timestamps and costs one interval.
    #[test]
    fn a_gpu_sample_span_starts_from_what_is_known() {
        let steady = [(42, 0, 10), (7, 1, 40)];
        let mut run = Intervals::new();
        run.next(&steady, 50, &[42, 43]);
        run.next(&steady, 50, &[42, 43]);
        // 43 listed but idle for 3 intervals, then busy at 30%: one window's worth, not four.
        for _ in 0..3 {
            run.next(&steady, 50, &[42, 43]);
        }
        assert_eq!(
            run.next(&[(42, 0, 10), (7, 1, 40), (43, 1, 30)], 80, &[42, 43]),
            ComputeAmount::Measured {
                external: 400,
                own: 400
            }
        );
        // 44 first seen 1 period before the end: 2 of 3 periods of the window, 20 of its 30%.
        assert_eq!(
            run.next(&[(42, 0, 10), (7, 1, 40), (44, 1, 30)], 70, &[42, 44]),
            ComputeAmount::Measured {
                external: 300,
                own: 400
            }
        );
        // 44 leaves the list, and a new process takes its PID: it starts from the window again.
        run.next(&steady, 50, &[42]);
        assert_eq!(
            run.next(&[(42, 0, 10), (7, 1, 40), (44, 1, 30)], 70, &[42, 44]),
            ComputeAmount::Measured {
                external: 300,
                own: 400
            }
        );
        // Own work sampled half a window after its previous sample, then stopped: half its 40%.
        let mut half = Intervals::new();
        half.next(&[(42, 0, 10), (7, 0, 40)], 50, &[42]);
        let end = half.cursor + Intervals::W;
        let samples = vec![
            gpu_sample(42, end, 10),
            gpu_sample(7, end - Intervals::W / 2, 40),
        ];
        let device: Vec<_> = (1..=3)
            .map(|i| device_sample(half.cursor + i * Intervals::W / 3, 30))
            .collect();
        assert_eq!(
            measure_compute(
                Some(&[42]),
                &BTreeSet::from([7]),
                Some(half.cursor),
                &Ok(samples),
                device_utilization(&Ok(device), Some(half.cursor), Some(30)),
                false,
                15,
                &BTreeSet::new(),
                &mut half.state,
            )
            .0,
            ComputeAmount::Measured {
                external: 100,
                own: 200
            }
        );
        // An agent restart forgets every timestamp: one interval reads the probe from the window's
        // start, the next in full again.
        let mut restart = Intervals::new();
        for _ in 0..2 {
            restart.next(&steady, 50, &[42]);
        }
        restart.state = GpuSampleState::default();
        assert_ne!(
            restart.next(&steady, 50, &[42]),
            ComputeAmount::Measured {
                external: 100,
                own: 400
            }
        );
        assert_eq!(
            restart.next(&steady, 50, &[42]),
            ComputeAmount::Measured {
                external: 100,
                own: 400
            }
        );
    }

    /// The 034 rerun's sparse samples (FR-021, Q17, R47): an interval that misses the busy probe's
    /// sample fails the tolerance alone, so the shares and the device summed over it and the 3
    /// before it decide, and the amounts are those 4 intervals' averages, the part nobody accounts
    /// for counting as external. An interval that passes alone keeps its own amounts; fewer than
    /// 4 comparable intervals, or an interval that cannot be compared in between, leave it unknown.
    #[test]
    fn a_missed_gpu_sample_is_compared_over_the_last_four_intervals() {
        let both = [(42, 0, 5), (7, 1, 50)];
        let mut run = Intervals::new();
        for _ in 0..4 {
            run.next(&both, 55, &[42]);
        }
        // The probe's sample is missed: 5% against 55% alone; over the last 4, 170 against 220.
        assert_eq!(
            run.next(&[(42, 0, 5)], 55, &[42]),
            ComputeAmount::Measured {
                external: 170,
                own: 380
            }
        );
        // An interval that passes alone keeps its own amounts.
        assert_eq!(
            run.next(&[(42, 0, 5), (7, 1, 40)], 45, &[42]),
            ComputeAmount::Measured {
                external: 50,
                own: 400
            }
        );
        // Too few intervals: a missed sample right after the start reads unknown.
        let mut early = Intervals::new();
        early.next(&both, 55, &[42]);
        early.next(&both, 55, &[42]);
        assert!(matches!(
            early.next(&[(42, 0, 5)], 55, &[42]),
            ComputeAmount::Unknown(_)
        ));
        // An interval that cannot be compared clears the intervals before it.
        let mut cleared = Intervals::new();
        for _ in 0..4 {
            cleared.next(&both, 55, &[42]);
        }
        let mps = measure_compute(
            Some(&[42]),
            &BTreeSet::from([7]),
            Some(cleared.cursor),
            &Ok(vec![]),
            Some(55.into()),
            true,
            15,
            &BTreeSet::new(),
            &mut cleared.state,
        );
        assert!(matches!(mps.0, ComputeAmount::Unknown(_)));
        for _ in 0..2 {
            cleared.next(&both, 55, &[42]);
        }
        assert!(matches!(
            cleared.next(&[(42, 0, 5)], 55, &[42]),
            ComputeAmount::Unknown(_)
        ));
    }

    /// Replays a recorded run's GPU observations through `measure_compute` (FR-021, Q17): set
    /// `CEDEGRID_GPU_REPLAY` to a JSON file of windows, each a `name` and its GPU `snapshots` in
    /// order, as the test host's trace tool writes them. Each interval's previous samples come from
    /// its record. Prints per window how many intervals read measured alone, over the last 4 or
    /// unknown, and how many match the reading the agent recorded.
    #[test]
    #[ignore]
    fn replay_gpu_trace() {
        let path = std::env::var_os("CEDEGRID_GPU_REPLAY")
            .expect("CEDEGRID_GPU_REPLAY names the trace to replay");
        let windows: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        for window in windows {
            let snapshots: Vec<crate::model::GpuSnapshot> =
                serde_json::from_value(window["snapshots"].clone()).unwrap();
            let mut state = GpuSampleState::default();
            let (mut alone, mut over_4, mut unknown, mut same, mut replayed) = (0, 0, 0, 0, 0);
            for snapshot in &snapshots {
                let Some(record) = &snapshot.compute_sampling else {
                    continue;
                };
                let listed = snapshot.external_process_ids.clone().unwrap_or_default();
                let background: BTreeSet<u32> =
                    snapshot.background_process_ids.iter().copied().collect();
                let own: BTreeSet<u32> = record
                    .processes
                    .iter()
                    .map(|p| p.pid)
                    .filter(|pid| !listed.contains(pid) && !background.contains(pid))
                    .collect();
                let samples: Vec<_> = record
                    .processes
                    .iter()
                    .flat_map(|p| p.samples.iter().map(|(at, sm)| gpu_sample(p.pid, *at, *sm)))
                    .collect();
                let mut known: Vec<_> = record
                    .processes
                    .iter()
                    .filter_map(|p| p.previous_us.map(|at| gpu_sample(p.pid, at, 0)))
                    .collect();
                known.sort_by_key(|s| s.timestamp);
                state.remember(0, &known, None);
                let device = record.device_percent.map(|percent| DeviceUtilization {
                    percent,
                    samples: Some(record.device_samples),
                });
                let external: Vec<u32> = listed
                    .iter()
                    .copied()
                    .filter(|pid| !background.contains(pid))
                    .collect();
                let (amount, _) = measure_compute(
                    Some(&external),
                    &own,
                    Some(record.window_start_us),
                    &Ok(samples),
                    device,
                    false,
                    15,
                    &background,
                    &mut state,
                );
                replayed += 1;
                match &amount {
                    ComputeAmount::Measured { external, own } => {
                        if state.over_last_4 {
                            over_4 += 1
                        } else {
                            alone += 1
                        }
                        same += usize::from(
                            snapshot.external_compute_milli == Some(*external)
                                && snapshot.own_compute_milli == Some(*own),
                        );
                    }
                    ComputeAmount::Unknown(_) => {
                        unknown += 1;
                        same += usize::from(snapshot.compute_amount_unknown.is_some());
                    }
                }
            }
            println!(
                "replay {}: {replayed} intervals, measured alone {alone}, over the last 4 {over_4}, unknown {unknown}; equal to the recorded reading {same}",
                window["name"].as_str().unwrap_or("?")
            );
        }
    }

    /// The 033 batch's shape (FR-021, R43, R46): the device is sampled about 3 times as often as
    /// each process, so dividing by the device's count read every share at a third. Each sample
    /// now counts its span since its process's previous one; with no previous sample the span
    /// starts at the window's start, so a single sample reads in full.
    #[test]
    fn gpu_compute_counts_against_the_per_process_period() {
        let own = BTreeSet::from([7]);
        let none = BTreeSet::new();
        let device = |percent| {
            device_utilization(
                &Ok((1..=9)
                    .map(|i| device_sample(100 + i * 600 / 9, percent))
                    .collect()),
                Some(100),
                Some(percent),
            )
        };
        let every = |pid, sm| [300, 500, 700].map(|at| gpu_sample(pid, at, sm)).to_vec();
        let measure = |listed: &[u32], samples: Vec<ProcessUtilizationSample>, percent| {
            measure_compute(
                Some(listed),
                &own,
                Some(100),
                &Ok(samples),
                device(percent),
                false,
                15,
                &none,
                &mut GpuSampleState::default(),
            )
        };
        // Own and external busy.
        let (amount, by_pid) = measure(&[42], [every(7, 60), every(42, 30)].concat(), 90);
        assert_eq!(
            amount,
            ComputeAmount::Measured {
                external: 300,
                own: 600
            }
        );
        assert_eq!(by_pid, BTreeMap::from([(42, 300)]));
        // Own alone, and external alone.
        assert_eq!(
            measure(&[], every(7, 60), 60).0,
            ComputeAmount::Measured {
                external: 0,
                own: 600
            }
        );
        assert_eq!(
            measure(&[42], every(42, 30), 30).0,
            ComputeAmount::Measured {
                external: 300,
                own: 0
            }
        );
        // Own stopped for the rest of the window after its first sample: idle for that time.
        let stopped = [vec![gpu_sample(7, 300, 60)], every(42, 30)].concat();
        assert_eq!(
            measure(&[42], stopped, 50).0,
            ComputeAmount::Measured {
                external: 300,
                own: 200
            }
        );
        // A process seen once covers from the window's start: 400 of 600 µs of its 30%.
        let once = [every(7, 60), vec![gpu_sample(43, 500, 30)]].concat();
        assert_eq!(
            measure(&[43], once, 80).0,
            ComputeAmount::Measured {
                external: 200,
                own: 600
            }
        );
        // No samples at all: as today.
        let not_found = |percent| {
            measure_compute(
                Some(&[42]),
                &own,
                Some(100),
                &Err(NvmlError::NotFound),
                device(percent),
                false,
                15,
                &none,
                &mut GpuSampleState::default(),
            )
            .0
        };
        assert_eq!(
            not_found(0),
            ComputeAmount::Measured {
                external: 0,
                own: 0
            }
        );
        assert_eq!(
            not_found(40),
            ComputeAmount::Unknown(
                "summed per-process utilization 0% differs from device utilization 40% by more than 15 points".into()
            )
        );
        // One sample each, at the window's end: each covers the whole window.
        let single = vec![gpu_sample(7, 700, 60), gpu_sample(42, 700, 30)];
        assert_eq!(
            measure(&[42], single, 90).0,
            ComputeAmount::Measured {
                external: 300,
                own: 600
            }
        );
    }

    /// Each interval keeps what a replay needs (FR-021, Q17): the device's count and first and last
    /// timestamps, the window, whether the amount came from the interval alone or the last 4,
    /// and per process its previous sample, each sample's timestamp and sm_util, at most 32, and
    /// the time counted; with no baseline there is nothing to show and the snapshot leaves the
    /// field out.
    #[test]
    fn an_interval_records_how_its_gpu_samples_were_counted() {
        use crate::model::{ComparedOver, ComputeSampling, ProcessSampling};
        let device: Vec<_> = (1..=3).map(|i| device_sample(100 + i * 200, 60)).collect();
        let samples = vec![
            gpu_sample(7, 500, 40),
            gpu_sample(42, 700, 20),
            gpu_sample(42, 400, 20),
        ];
        let mut state = GpuSampleState::default();
        state.remember(0, &[gpu_sample(7, 90, 40)], None);
        let record = compute_sampling(
            Some(100),
            &Ok(samples),
            &Ok(device.clone()),
            Some(60),
            &state,
        );
        assert_eq!(
            record,
            Some(ComputeSampling {
                device_samples: 3,
                device_first_us: Some(300),
                device_last_us: Some(700),
                device_percent: Some(60),
                window_start_us: 100,
                window_end_us: Some(700),
                compared_over: ComparedOver::Interval,
                processes: vec![
                    ProcessSampling {
                        pid: 7,
                        previous_us: Some(90),
                        samples: vec![(500, 40)],
                        counted_us: 410,
                    },
                    ProcessSampling {
                        pid: 42,
                        previous_us: None,
                        samples: vec![(400, 20), (700, 20)],
                        counted_us: 600,
                    },
                ],
            })
        );
        let many: Vec<_> = (1..=40).map(|i| gpu_sample(42, 100 + i * 10, 5)).collect();
        let bounded =
            compute_sampling(Some(100), &Ok(many), &Ok(device.clone()), Some(60), &state).unwrap();
        assert_eq!(bounded.processes[0].samples.len(), 32);
        assert_eq!(bounded.processes[0].counted_us, 400);
        state.over_last_4 = true;
        assert_eq!(
            compute_sampling(
                Some(100),
                &Ok(vec![]),
                &Ok(device.clone()),
                Some(60),
                &state
            )
            .unwrap()
            .compared_over,
            ComparedOver::Last4
        );
        assert_eq!(
            compute_sampling(None, &Ok(vec![]), &Ok(device), Some(60), &state),
            None
        );
        let mut snap: crate::model::GpuSnapshot = serde_json::from_value(serde_json::json!({
            "uuid": "GPU-a", "total_memory_mib": 1, "used_memory_mib": null,
            "utilization_percent": null, "external_process_ids": null,
            "external_compute": "unknown", "compute_sample_id": null
        }))
        .unwrap();
        assert!(
            !serde_json::to_string(&snap)
                .unwrap()
                .contains("compute_sampling")
        );
        snap.compute_sampling = record;
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"samples\":[[500,40]]"), "{json}");
        let back: crate::model::GpuSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.compute_sampling, snap.compute_sampling);
    }

    /// Two measured shapes (FR-021, R33). An own process duty-capped to a third of
    /// the interval at 60% beside a light 10% user: the device reads 30%, and each process at
    /// its share of the whole interval accounts for it, so compute is measured, not unknown.
    /// A saturating pair whose means round to 99 of 100 points leaves no remainder to count
    /// as a third principal. A disagreement beyond the tolerance still reads unknown.
    #[test]
    fn gpu_compute_counts_each_process_over_the_whole_interval() {
        let own = BTreeSet::from([7]);
        let mut capped = vec![];
        let mut device = vec![];
        for (i, at) in (110..=160).step_by(10).enumerate() {
            capped.push(gpu_sample(42, at, 10));
            let running = i < 2;
            if running {
                capped.push(gpu_sample(7, at, 60));
            }
            device.push(device_sample(at, if running { 70 } else { 10 }));
        }
        let interval = device_utilization(&Ok(device.clone()), Some(100), Some(70));
        assert_eq!(
            interval,
            Some(DeviceUtilization {
                percent: 30,
                samples: Some(6)
            })
        );
        let (amount, by_pid) = measure_compute(
            Some(&[42]),
            &own,
            Some(100),
            &Ok(capped.clone()),
            interval,
            false,
            15,
            &BTreeSet::new(),
            &mut GpuSampleState::default(),
        );
        assert_eq!(
            amount,
            ComputeAmount::Measured {
                external: 100,
                own: 200
            }
        );
        assert_eq!(by_pid, BTreeMap::from([(42, 100)]));
        // Without the device's samples the light user's six stand for the interval.
        assert_eq!(
            measure_compute(
                Some(&[42]),
                &own,
                Some(100),
                &Ok(capped),
                Some(30.into()),
                false,
                15,
                &BTreeSet::new(),
                &mut GpuSampleState::default()
            )
            .0,
            ComputeAmount::Measured {
                external: 100,
                own: 200
            }
        );
        // Saturated: own at 49.4% and the user at 50% of five samples on a device at 100%.
        let mut saturated = vec![];
        for (at, own_util, user) in [
            (110, 50, 50),
            (120, 49, 50),
            (130, 50, 50),
            (140, 49, 50),
            (150, 49, 50),
        ] {
            saturated.push(gpu_sample(7, at, own_util));
            saturated.push(gpu_sample(42, at, user));
        }
        let (amount, by_pid) = measure_compute(
            Some(&[42]),
            &own,
            Some(100),
            &Ok(saturated),
            Some(DeviceUtilization {
                percent: 100,
                samples: Some(5),
            }),
            false,
            15,
            &BTreeSet::new(),
            &mut GpuSampleState::default(),
        );
        assert_eq!(
            amount,
            ComputeAmount::Measured {
                external: 500,
                own: 490
            }
        );
        assert_eq!(by_pid, BTreeMap::from([(42, 500)]));
        // Shares that account for 30 points of a device at 50% are beyond the tolerance.
        assert!(matches!(
            measure_compute(
                Some(&[42]),
                &own,
                Some(100),
                &Ok(device_sample_free(&device)),
                Some(DeviceUtilization {
                    percent: 50,
                    samples: Some(6),
                }),
                false,
                15,
                &BTreeSet::new(), &mut GpuSampleState::default())
            .0,
            ComputeAmount::Unknown(reason) if reason.contains("differs")
        ));
        // Device samples older than the cursor, or none, fall back to the driver's rate.
        assert_eq!(
            device_utilization(&Ok(device), Some(200), Some(70)),
            Some(70.into())
        );
        assert_eq!(
            device_utilization(&Err(NvmlError::NotSupported), Some(100), None),
            None
        );
    }

    /// The capped shape's process samples, for the disagreement case.
    fn device_sample_free(
        device: &[nvml_wrapper::struct_wrappers::device::Sample],
    ) -> Vec<ProcessUtilizationSample> {
        device
            .iter()
            .enumerate()
            .flat_map(|(i, sample)| {
                let mut out = vec![gpu_sample(42, sample.timestamp, 10)];
                if i < 2 {
                    out.push(gpu_sample(7, sample.timestamp, 60));
                }
                out
            })
            .collect()
    }

    /// External GPU compute is measured from fresh per-process samples, averaged
    /// per PID and capped at 100, when their sum is within the tolerance of device
    /// utilization; otherwise, and without support, a baseline, a complete
    /// inventory or with MPS, it stays unknown (FR-021).
    #[test]
    fn external_gpu_compute_is_measured_only_when_samples_account_for_the_device() {
        let own = BTreeSet::from([7]);
        let ok = |samples: Vec<ProcessUtilizationSample>| -> std::result::Result<_, NvmlError> {
            Ok(samples)
        };
        let measure = |samples: &std::result::Result<Vec<ProcessUtilizationSample>, NvmlError>,
                       device: Option<u32>| {
            measure_compute(
                Some(&[42]),
                &own,
                Some(100),
                samples,
                device.map(Into::into),
                false,
                15,
                &BTreeSet::new(),
                &mut GpuSampleState::default(),
            )
            .0
        };
        // One external PID at 5% beside own work at 94%, on a device at 100%: one point per
        // sampled process is rounding, not unattributed usage (R33).
        let five = ok(vec![
            gpu_sample(42, 110, 5),
            gpu_sample(7, 110, 94),
            gpu_sample(42, 120, 5),
            gpu_sample(7, 120, 94),
            // Older than the cursor: not this interval.
            gpu_sample(42, 90, 100),
        ]);
        assert_eq!(
            measure(&five, Some(100)),
            ComputeAmount::Measured {
                external: 50,
                own: 940
            }
        );
        // Beyond that, device time nobody accounts for is external while an external PID
        // is listed.
        assert_eq!(
            measure(&five, Some(103)),
            ComputeAmount::Measured {
                external: 90,
                own: 940
            }
        );
        // At 40%, averaged over the PID's fresh samples.
        let forty = ok(vec![
            gpu_sample(42, 110, 30),
            gpu_sample(42, 120, 50),
            gpu_sample(7, 110, 60),
            gpu_sample(7, 120, 60),
        ]);
        assert_eq!(
            measure(&forty, Some(100)),
            ComputeAmount::Measured {
                external: 400,
                own: 600
            }
        );
        // Within 15 points of the device it is measured; beyond, unknown.
        assert!(matches!(
            measure(&forty, Some(85)),
            ComputeAmount::Measured { .. }
        ));
        assert!(
            matches!(measure(&forty, Some(84)), ComputeAmount::Unknown(reason) if reason.contains("differs"))
        );
        // No fresh samples on a busy device are no evidence. Within the tolerance,
        // device time nobody accounts for is external while an external PID is
        // listed: NotFound at 12% with PID 42 listed is 120, not 0.
        let none = Err(NvmlError::NotFound);
        assert!(matches!(
            measure(&none, Some(60)),
            ComputeAmount::Unknown(_)
        ));
        assert_eq!(
            measure(&none, Some(12)),
            ComputeAmount::Measured {
                external: 120,
                own: 0
            }
        );
        // With own PIDs only, the remainder is sampling error on own work.
        let own_only = ok(vec![gpu_sample(7, 110, 85)]);
        assert_eq!(
            measure_compute(
                Some(&[]),
                &own,
                Some(100),
                &own_only,
                Some(100.into()),
                false,
                15,
                &BTreeSet::new(),
                &mut GpuSampleState::default()
            )
            .0,
            ComputeAmount::Measured {
                external: 0,
                own: 1000
            }
        );
        // Unsupported queries, an incomplete inventory, no baseline and MPS.
        assert!(matches!(
            measure(&Err(NvmlError::NotSupported), Some(100)),
            ComputeAmount::Unknown(_)
        ));
        assert!(matches!(measure(&five, None), ComputeAmount::Unknown(_)));
        assert!(matches!(
            measure_compute(
                None,
                &own,
                Some(100),
                &five,
                Some(100.into()),
                false,
                15,
                &BTreeSet::new(),
                &mut GpuSampleState::default()
            )
            .0,
            ComputeAmount::Unknown(_)
        ));
        assert!(matches!(
            measure_compute(
                Some(&[42]),
                &own,
                None,
                &five,
                Some(100.into()),
                false,
                15,
                &BTreeSet::new(),
                &mut GpuSampleState::default()
            )
            .0,
            ComputeAmount::Unknown(_)
        ));
        assert!(matches!(
            measure_compute(Some(&[42]), &own, Some(100), &five, Some(100.into()), true, 15, &BTreeSet::new(), &mut GpuSampleState::default()).0,
            ComputeAmount::Unknown(reason) if reason.contains("MPS")
        ));
        // A PID above 100 counts as 100.
        let over = ok(vec![gpu_sample(42, 110, 250)]);
        assert_eq!(
            measure(&over, Some(100)),
            ComputeAmount::Measured {
                external: 1000,
                own: 0
            }
        );
        assert!(is_mps_process("nvidia-cuda-mps-server"));
        assert!(is_mps_process("nvidia-cuda-mps-control"));
        assert!(!is_mps_process("python3"));
    }

    /// A device time-slices its busy contexts only with neither MPS nor MIG, and an unreadable
    /// state is not taken as time-sliced (FR-021, Q22).
    #[test]
    fn a_device_is_time_sliced_only_without_mps_or_mig() {
        assert_eq!(time_sliced(true, false, &Ok(false)), Some(true));
        assert_eq!(
            time_sliced(true, false, &Err(NvmlError::NotSupported)),
            Some(true)
        );
        assert_eq!(time_sliced(true, true, &Ok(false)), Some(false));
        assert_eq!(
            time_sliced(false, true, &Err(NvmlError::Unknown)),
            Some(false)
        );
        assert_eq!(time_sliced(true, false, &Ok(true)), Some(false));
        assert_eq!(time_sliced(false, false, &Ok(false)), None);
        assert_eq!(time_sliced(true, false, &Err(NvmlError::Unknown)), None);
        assert_eq!(time_sliced(true, false, &Err(NvmlError::GpuLost)), None);
    }

    /// The interval's contexts: own work's listed PIDs, the owner's unmanaged ones, and each
    /// listed non-background PID with a positive sample newer than the cursor, with its UID
    /// (FR-021, Q22). Without a complete inventory there are none.
    #[test]
    fn compute_contexts_list_own_owner_and_sampled_pids() {
        let ok = |samples| -> std::result::Result<Vec<ProcessUtilizationSample>, NvmlError> {
            Ok(samples)
        };
        let own = BTreeSet::from([10, 11]);
        let background = BTreeSet::from([30]);
        let uid_of = |pid: u32| match pid {
            10 | 11 | 20 => Some(1000),
            21 | 22 => Some(2002),
            30 => Some(0),
            _ => None,
        };
        let listed = [10, 11, 20, 21, 22, 23, 30];
        let samples = ok(vec![
            gpu_sample(10, 150, 66),
            gpu_sample(20, 160, 33),
            gpu_sample(21, 170, 99),
            // Idle, stale, unlisted and background samples are no busy context.
            gpu_sample(22, 180, 0),
            gpu_sample(23, 90, 50),
            gpu_sample(40, 190, 50),
            gpu_sample(30, 200, 20),
        ]);
        let contexts = compute_contexts(
            Some(&listed),
            &own,
            &background,
            Some(100),
            &samples,
            uid_of,
            Some(1000),
        )
        .unwrap();
        assert_eq!(contexts.own, vec![10, 11]);
        assert_eq!(contexts.owner, vec![20]);
        assert_eq!(
            contexts.sampled,
            vec![(10, Some(1000)), (20, Some(1000)), (21, Some(2002))]
        );
        // An unreadable UID stays unattributed.
        let unread = ok(vec![gpu_sample(23, 150, 50)]);
        let contexts = compute_contexts(
            Some(&listed),
            &own,
            &background,
            Some(100),
            &unread,
            uid_of,
            Some(1000),
        )
        .unwrap();
        assert_eq!(contexts.sampled, vec![(23, None)]);
        // No fresh cursor or failed samples: the listing stands, nothing is sampled.
        for (previous, samples) in [
            (None, ok(vec![gpu_sample(10, 150, 66)])),
            (Some(100), Err(NvmlError::Unknown)),
        ] {
            let contexts = compute_contexts(
                Some(&listed),
                &own,
                &background,
                previous,
                &samples,
                uid_of,
                Some(1000),
            )
            .unwrap();
            assert_eq!(contexts.own, vec![10, 11]);
            assert!(contexts.sampled.is_empty());
        }
        assert_eq!(
            compute_contexts(
                None,
                &own,
                &background,
                Some(100),
                &samples,
                uid_of,
                Some(1000)
            ),
            None
        );
    }

    /// The listing the time-slice duty counts while the amounts are unknown: every listed PID
    /// that is neither own work's, the owner's nor background, with its UID where readable
    /// (FR-021, Q29).
    /// External activity read as unknown only for want of a new sample stays active within the
    /// sample age of an active reading, with the listing known; an unknown listing, an older
    /// reading or a quiet one stays unknown (FR-021).
    #[test]
    fn unknown_activity_keeps_the_last_known_reading_for_the_sample_age() {
        use crate::model::ComputeActivity::*;
        let now = Instant::now();
        let age = Duration::from_millis(2000);
        let last = (now, Active);
        let soon = now + Duration::from_millis(500);
        assert_eq!(
            carry_activity(Unknown, true, Some(&last), soon, age),
            Active
        );
        assert_eq!(
            carry_activity(Unknown, false, Some(&last), soon, age),
            Unknown
        );
        let late = now + Duration::from_millis(2500);
        assert_eq!(
            carry_activity(Unknown, true, Some(&last), late, age),
            Unknown
        );
        assert_eq!(carry_activity(Unknown, true, None, soon, age), Unknown);
        assert_eq!(carry_activity(Quiet, true, Some(&last), soon, age), Quiet);
        // Only an active reading carries over: unknown is never quiet.
        let quiet = (now, Quiet);
        assert_eq!(
            carry_activity(Unknown, true, Some(&quiet), soon, age),
            Unknown
        );
    }

    /// An interval without readable samples reads as the last readable one within the sample
    /// age, for the contexts still listed; past the age it stays unreadable (FR-021, T628).
    #[test]
    fn unreadable_samples_carry_the_last_readable_ones_for_the_sample_age() {
        let now = Instant::now();
        let unreadable = || crate::model::GpuComputeContexts {
            own: vec![10],
            owner: vec![50],
            sampled: vec![],
            listed: vec![(900, Some(2002))],
            samples_readable: false,
            others_quiet: true,
        };
        let last = (now, vec![(50, Some(1000)), (77, Some(2003))]);
        let age = Duration::from_millis(2000);
        let mut carried = unreadable();
        carry_samples(
            &mut carried,
            Some(&last),
            now + Duration::from_millis(500),
            age,
        );
        assert!(carried.samples_readable);
        assert_eq!(
            carried.sampled,
            vec![(50, Some(1000))],
            "77 is no longer listed"
        );
        let mut stale = unreadable();
        carry_samples(
            &mut stale,
            Some(&last),
            now + Duration::from_millis(2500),
            age,
        );
        assert!(!stale.samples_readable);
        let mut none = unreadable();
        carry_samples(&mut none, None, now, age);
        assert!(!none.samples_readable);
        let mut read = unreadable();
        read.samples_readable = true;
        carry_samples(&mut read, Some(&last), now, age);
        assert!(
            read.sampled.is_empty(),
            "a readable interval keeps its own samples"
        );
    }

    #[test]
    fn compute_contexts_list_the_external_pids_with_their_uids() {
        let uid_of = |pid: u32| match pid {
            10 | 20 => Some(1000),
            21 | 22 => Some(2002),
            30 => Some(0),
            _ => None,
        };
        let listed = [10, 20, 21, 22, 23, 30];
        for (previous, samples) in [
            (Some(100), Ok(vec![gpu_sample(21, 150, 33)])),
            (None, Err(NvmlError::Unknown)),
        ] {
            let contexts = compute_contexts(
                Some(&listed),
                &BTreeSet::from([10]),
                &BTreeSet::from([30]),
                previous,
                &samples,
                uid_of,
                Some(1000),
            )
            .unwrap();
            assert_eq!(
                contexts.listed,
                vec![(21, Some(2002)), (22, Some(2002)), (23, None)]
            );
            // Whether the samples were read, so an empty `sampled` is known (Q33).
            assert_eq!(contexts.samples_readable, previous.is_some());
        }
    }

    /// The device's external PIDs are every listed process less the managed ones, so the owner's
    /// unmanaged processes are among them, as are other users' (FR-022, Q36).
    #[test]
    fn the_owners_unmanaged_processes_are_listed_as_external() {
        let managed = BTreeSet::from([10]);
        assert_eq!(
            external_process_ids(Some(&[10, 20]), Some(&[21]), &managed),
            Some(vec![20, 21])
        );
        assert_eq!(external_process_ids(None, Some(&[20]), &managed), None);
    }

    #[test]
    fn missing_or_partial_process_inventory_never_means_empty() {
        assert_eq!(merge_process_lists(None, Some(&[])), None);
        assert_eq!(merge_process_lists(Some(&[]), None), None);
        assert_eq!(merge_process_lists(Some(&[0]), Some(&[])), None);
        assert_eq!(
            merge_process_lists(Some(&[1, 2]), Some(&[2, 3])),
            Some(vec![1, 2, 3])
        );
    }

    #[test]
    fn only_complete_empty_process_lists_establish_idle() {
        for samples in [None, Some(&[][..])] {
            assert_eq!(
                classify_activity(None, None, samples).0,
                ComputeActivity::Unknown
            );
            assert_eq!(
                classify_activity(Some(&[1]), None, samples).0,
                ComputeActivity::Unknown
            );
            assert_eq!(
                classify_activity(Some(&[]), None, samples).0,
                ComputeActivity::Idle
            );
        }
    }

    #[test]
    fn gpu_activity_requires_new_timestamp_and_known_external_pid() {
        let samples = [gpu_sample(42, 100, 50)];
        assert_eq!(
            classify_activity(Some(&[42]), None, Some(&samples)),
            (ComputeActivity::Unknown, None, Some(100))
        );
        assert_eq!(
            classify_activity(Some(&[42]), Some(100), Some(&samples)).0,
            ComputeActivity::Unknown
        );
        assert_eq!(
            classify_activity(Some(&[42]), Some(99), Some(&samples)),
            (ComputeActivity::Active, Some(100), Some(100))
        );
        assert_eq!(
            classify_activity(Some(&[7]), Some(99), Some(&samples)).0,
            ComputeActivity::Unknown
        );
        assert_eq!(
            classify_activity(None, Some(99), Some(&samples)).0,
            ComputeActivity::Unknown
        );
        assert_eq!(
            classify_activity(Some(&[42]), Some(101), Some(&samples)).2,
            Some(101)
        );
    }

    #[test]
    fn zero_or_missing_compute_samples_do_not_mean_idle() {
        let samples = [gpu_sample(42, 100, 0)];
        assert_eq!(
            classify_activity(Some(&[42]), Some(99), Some(&samples)).0,
            ComputeActivity::Unknown
        );
        assert_eq!(
            classify_activity(Some(&[42]), Some(99), None),
            (ComputeActivity::Unknown, None, Some(99))
        );
        assert_eq!(error_status(&NvmlError::NoData), CapabilityStatus::Unknown);
        assert_eq!(
            error_status(&NvmlError::NotSupported),
            CapabilityStatus::Unsupported
        );
        assert_eq!(
            error_status(&NvmlError::LibraryNotFound),
            CapabilityStatus::Unavailable
        );
    }

    #[test]
    fn unavailable_device_memory_is_total_minus_free() {
        let read = DeviceMemoryRead {
            total: 24 * 1024 * MIB,
            free: 20 * 1024 * MIB,
            reserved: 300 * MIB,
        };
        let memory = device_memory(Some(read), Some(read), 256).unwrap();
        assert_eq!(memory.total_mib, 24 * 1024);
        // Unavailable includes driver-reserved memory that `used` leaves out.
        assert_eq!(memory.unavailable_mib, 4 * 1024);
        assert_eq!(memory.free_mib, 20 * 1024);
        assert_eq!(memory.reserved_mib, 300);
        assert!(memory.attribution_known);
        // Rounding never invents free memory.
        let odd = DeviceMemoryRead {
            total: 4 * MIB + 1,
            free: MIB + 1,
            reserved: 0,
        };
        let memory = device_memory(Some(odd), Some(odd), 256).unwrap();
        assert_eq!(
            (memory.total_mib, memory.unavailable_mib, memory.free_mib),
            (4, 3, 1)
        );
        let inconsistent = DeviceMemoryRead {
            total: 4 * MIB,
            free: 5 * MIB,
            reserved: 0,
        };
        assert_eq!(device_memory(Some(inconsistent), Some(read), 256), None);
        assert_eq!(device_memory(None, Some(read), 256), None);
    }

    #[test]
    fn a_double_read_disagreement_beyond_tolerance_makes_attribution_unknown() {
        let before = DeviceMemoryRead {
            total: 24 * 1024 * MIB,
            free: 20 * 1024 * MIB,
            reserved: 300 * MIB,
        };
        let moved = |mib: u64| DeviceMemoryRead {
            free: before.free - mib * MIB,
            ..before
        };
        let within = device_memory(Some(before), Some(moved(200)), 256).unwrap();
        assert!(within.attribution_known);
        // The larger of the two reads is reported as unavailable.
        assert_eq!(within.unavailable_mib, 4 * 1024 + 200);
        let beyond = device_memory(Some(before), Some(moved(300)), 256).unwrap();
        assert!(!beyond.attribution_known);
        assert_eq!(beyond.unavailable_mib, 4 * 1024 + 300);
        let failed_second_read = device_memory(Some(before), None, 256).unwrap();
        assert!(!failed_second_read.attribution_known);
    }

    /// The quiet window counts other accounts' contexts only: the owner's busy samples neither
    /// break a quiet tick nor restart the window, and an owner's new context starts nothing,
    /// while another account's busy sample or new context does (FR-027).
    #[test]
    fn the_owners_compute_neither_breaks_nor_restarts_other_accounts_quiet_window() {
        type Samples = std::result::Result<Vec<ProcessUtilizationSample>, NvmlError>;
        const OWNER: u32 = 42;
        const OTHER: u32 = 7;
        // The owner's processes: one computing from the start and one that starts later.
        let owner = BTreeSet::from([OWNER, 43]);
        let tick = |state: &mut QuietState, pids: &[u32], samples: &Samples| {
            let others = others_of(Some(pids), &owner);
            let healthy = quiet_tick(Some(pids), Some(100), samples, &owner);
            advance_quiet(state, others.as_deref(), healthy)
        };
        let owner_busy: Samples = Ok(vec![gpu_sample(OWNER, 101, 60)]);
        let mut state = QuietState::default();
        for expected in 1..=20 {
            assert_eq!(tick(&mut state, &[OWNER, OTHER], &owner_busy), expected);
        }
        // An owner's new context starts nothing.
        assert_eq!(tick(&mut state, &[OWNER, OTHER, 43], &owner_busy), 21);
        // Another account's busy sample resets the window, and its new context restarts it.
        let other_busy: Samples = Ok(vec![gpu_sample(OTHER, 101, 30)]);
        assert_eq!(tick(&mut state, &[OWNER, OTHER], &other_busy), 0);
        for _ in 0..5 {
            tick(&mut state, &[OWNER, OTHER], &owner_busy);
        }
        assert_eq!(tick(&mut state, &[OWNER, OTHER, 9], &owner_busy), 1);
    }

    #[test]
    fn quiet_requires_a_window_of_healthy_samples_over_a_complete_inventory() {
        type Samples = std::result::Result<Vec<ProcessUtilizationSample>, NvmlError>;
        let window = 20;
        fn tick(
            state: &mut QuietState,
            pids: Option<&[u32]>,
            samples: &Samples,
        ) -> ComputeActivity {
            let healthy = quiet_tick(pids, Some(100), samples, &BTreeSet::new());
            quiet_activity(
                ComputeActivity::Unknown,
                advance_quiet(state, pids, healthy),
                20,
            )
        }
        let not_found: Samples = Err(NvmlError::NotFound);
        let mut state = QuietState::default();
        for _ in 1..window {
            assert_eq!(
                tick(&mut state, Some(&[42]), &not_found),
                ComputeActivity::Unknown
            );
        }
        assert_eq!(
            tick(&mut state, Some(&[42]), &not_found),
            ComputeActivity::Quiet
        );
        // Successful zero samples are healthy evidence as well.
        let zero: Samples = Ok(vec![gpu_sample(42, 101, 0)]);
        assert_eq!(tick(&mut state, Some(&[42]), &zero), ComputeActivity::Quiet);
        // A process that leaves keeps the window; a new one restarts it.
        let mut state = QuietState::default();
        for _ in 0..window {
            tick(&mut state, Some(&[42, 7]), &not_found);
        }
        assert_eq!(
            tick(&mut state, Some(&[42]), &not_found),
            ComputeActivity::Quiet
        );
        assert_eq!(
            tick(&mut state, Some(&[42, 9]), &not_found),
            ComputeActivity::Unknown
        );
        // Incomplete inventory, query errors and positive samples reset the window.
        let resets: [(Option<&[u32]>, Samples); 3] = [
            (None, Err(NvmlError::NotFound)),
            (Some(&[42]), Err(NvmlError::NoPermission)),
            (Some(&[42]), Ok(vec![gpu_sample(42, 101, 30)])),
        ];
        for (pids, samples) in resets {
            let mut state = QuietState::default();
            for _ in 0..window {
                tick(&mut state, Some(&[42]), &not_found);
            }
            assert_eq!(tick(&mut state, pids, &samples), ComputeActivity::Unknown);
            assert_eq!(state.ticks, 0);
        }
        // Known activity is never overridden.
        assert_eq!(
            quiet_activity(ComputeActivity::Active, 99, window),
            ComputeActivity::Active
        );
        assert_eq!(
            quiet_activity(ComputeActivity::Idle, 0, window),
            ComputeActivity::Idle
        );
    }

    #[test]
    fn an_idle_devices_health_never_becomes_insufficient_observability() {
        let max_age = Duration::from_millis(731);
        for (previous, age, samples) in [
            (None, None, Err(NvmlError::NotFound)),
            (
                Some(100),
                Some(Duration::from_millis(20)),
                Err(NvmlError::NotFound),
            ),
            (Some(100), Some(Duration::from_secs(5)), Ok(vec![])),
            (None, None, Ok(vec![gpu_sample(42, 90, 0)])),
        ] {
            let observation =
                describe_gpu_observation(true, previous, age, &samples, None, 1234, max_age);
            assert_eq!(
                observation.capability,
                GpuExecutionCapability::ContentionAware,
                "{samples:?}"
            );
            assert!(!observation.fresh_after_baseline);
        }
        for samples in [Err(NvmlError::GpuLost), Ok(vec![])] {
            let basic_known = samples.is_err();
            assert_eq!(
                describe_gpu_observation(basic_known, None, None, &samples, None, 1234, max_age)
                    .capability,
                GpuExecutionCapability::InsufficientObservability
            );
        }
    }

    #[test]
    fn managed_cpu_upper_bound_adds_quantization_over_the_process_interval() {
        let lower = cpu_usage_lower_bound(500 * MS, Duration::from_secs(1), 22 * MS).unwrap();
        let upper = cpu_usage_upper_bound(500 * MS, Duration::from_millis(900), 22 * MS).unwrap();
        assert_eq!((lower, upper), (478, 580));
        assert_eq!(cpu_usage_upper_bound(MS, Duration::ZERO, 2 * MS), None);
    }

    #[test]
    fn cpu_units_are_logical_millicores_with_real_elapsed_time() {
        assert_eq!(cpu_millicores(25.0, 8000), Some(2000));
        assert_eq!(cpu_millicores(100.0, 8000), Some(8000));
        assert_eq!(cpu_millicores(f32::NAN, 8000), None);
        assert_eq!(cpu_millicores(101.0, 8000), None);
        assert_eq!(cpu_millicores(1.0, 0), None);
        if !sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.is_zero() {
            assert!(!cpu_sample_ready(Duration::ZERO));
        }
        assert!(cpu_sample_ready(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL));
    }

    #[tokio::test]
    async fn waiting_for_cpu_eligibility_produces_fresh_usage_without_reusing_early_samples() {
        if !sysinfo::IS_SUPPORTED_SYSTEM || sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.is_zero() {
            return;
        }
        let mut managed = ManagedCollector::new(&crate::config::Config::default()).unwrap();
        managed.collector.last_cpu_refresh = Instant::now();
        let early = managed.collector.sample().unwrap();
        assert!(early.cpu_busy_millicores.is_none());
        let eligible = managed.next_cpu_sample_at();
        tokio::time::sleep_until(tokio::time::Instant::from_std(eligible)).await;
        let (fresh, allocations) = managed.sample_with_children(&[], &[]).unwrap();
        assert!(fresh.cpu_busy_millicores.is_some());
        assert!(allocations.is_empty());
        assert!(managed.next_cpu_sample_at() > eligible);
        assert!(fresh.observed_at_unix_ms >= early.observed_at_unix_ms);
    }

    #[test]
    fn process_cpu_quantization_cannot_prove_false_single_core_excess() {
        // A one-core worker may advance an integral millisecond counter by
        // 501ms during a 500.5ms observed interval. Its budget remains1000m.
        assert!(
            cpu_usage_lower_bound(501 * MS, Duration::from_micros(500_500), 2 * MS).unwrap()
                <= 1000
        );
        // Full nanoseconds and enclosing refresh endpoints avoid truncated-wall
        // and variable collector-cost bias. Actual extra-thread demand survives.
        assert_eq!(
            cpu_usage_lower_bound(1100 * MS, Duration::from_secs(1), 2 * MS),
            Some(1098)
        );
        assert!(
            cpu_usage_lower_bound(525 * MS, Duration::from_millis(525), 22 * MS).unwrap() <= 1000
        );
        assert_eq!(
            cpu_usage_lower_bound(MS, Duration::from_secs(1), 2 * MS),
            Some(0)
        );
        assert_eq!(cpu_usage_lower_bound(MS, Duration::ZERO, 2 * MS), None);
    }

    #[test]
    fn aligned_cpu_new_worker_uses_the_whole_aggregate_interval() {
        let started = Instant::now();
        let window = crate::kernel::CpuAccountingWindow {
            started,
            ended: started + Duration::from_millis(510),
            counter_interval: Duration::from_millis(510),
        };
        // The native failure compared a partial first worker interval against
        // a full 510ms node interval. An unchanged 500m reservation is charged
        // separately; its partial CPU delta must not be scaled to a full rate.
        let old_wrong_rate =
            cpu_usage_lower_bound(170 * MS, Duration::from_millis(300), 22 * MS).unwrap();
        let aligned = aligned_cpu_usage_lower_bound(
            170 * MS,
            started + Duration::from_millis(200),
            started + Duration::from_millis(500),
            window,
            22 * MS,
        )
        .unwrap();
        assert!(old_wrong_rate > 322);
        assert_eq!(aligned, 290);
        assert!(aligned <= 322);
        // Neither a missing baseline nor execution outside aggregate endpoints
        // is converted into a measurement of zero CPU.
        assert_eq!(
            aligned_cpu_usage_lower_bound(
                170 * MS,
                started - Duration::from_millis(1),
                window.ended,
                window,
                22 * MS
            ),
            None
        );
        assert_eq!(
            aligned_cpu_usage_lower_bound(
                170 * MS,
                started,
                window.ended + Duration::from_millis(1),
                window,
                22 * MS
            ),
            None
        );
    }

    #[test]
    fn aligned_cpu_uses_counter_timebase_and_excludes_collection_gaps() {
        let started = Instant::now();
        let window = crate::kernel::CpuAccountingWindow {
            started,
            ended: started + Duration::from_millis(510),
            // Per-core /proc/stat ratios use tick totals, not wall time. Use
            // the greatest selected-core interval to lower-bound subtraction
            // even when tick totals disagree with the enclosing wall clock.
            counter_interval: Duration::from_millis(600),
        };
        assert_eq!(
            aligned_cpu_usage_lower_bound(
                300 * MS,
                started + Duration::from_millis(50),
                started + Duration::from_millis(450),
                window,
                22 * MS,
            ),
            Some(463)
        );
        // Actual extra-thread excess remains observable; the envelope and
        // rounding allowance were not increased to hide the native failure.
        assert!(
            aligned_cpu_usage_lower_bound(900 * MS, started, window.ended, window, 22 * MS)
                .unwrap()
                > 1000
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_policy_allocations_preserve_class_even_without_process_identity() {
        use crate::execution_model::{AllocationClass, ExecutionPhase, ExecutionRecord};
        let config = crate::config::Config::default();
        let mut collector = ManagedCollector::new(&config).unwrap();
        let mut record = ExecutionRecord {
            task_id: "protected-reservation".into(),
            assignment_id: "protected-reservation".into(),
            generation: 1,
            class: AllocationClass::Guaranteed,
            phase: ExecutionPhase::Reserved,
            resources: crate::model::Resources {
                cpu_millicores: 1000,
                ..Default::default()
            },
            identity: None,
            backend: "rootless".into(),
            evidence: vec![],
            detail: String::new(),
            modes: Default::default(),
        };
        for phase in [
            ExecutionPhase::Reserved,
            ExecutionPhase::NeedsReconciliation,
        ] {
            record.phase = phase;
            let (_, allocations) = collector.sample(std::slice::from_ref(&record)).unwrap();
            assert_eq!(allocations.len(), 1);
            assert_eq!(allocations[0].class, AllocationClass::Guaranteed);
            assert_eq!(allocations[0].requested.cpu_millicores, 1000);
            assert!(allocations[0].observed.is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn released_records_drop_counter_and_cached_process_handles() {
        use crate::execution_model::{AllocationClass, ExecutionPhase, ExecutionRecord};
        let config = crate::config::Config::default();
        let mut collector = ManagedCollector::new(&config).unwrap();
        let identity =
            crate::supervision::process_identity(std::process::id(), "measurement-test", 1)
                .unwrap();
        let record = ExecutionRecord {
            task_id: "measurement-test".into(),
            assignment_id: "measurement-test".into(),
            generation: 1,
            class: AllocationClass::Guaranteed,
            phase: ExecutionPhase::Running,
            resources: crate::model::Resources::default(),
            identity: Some(identity),
            backend: "rootless".into(),
            evidence: vec![],
            detail: String::new(),
            modes: Default::default(),
        };
        collector.prime(&[record]);
        assert_eq!(collector.previous.len(), 1);
        // Linux keeps a pidfd per verified process; other platforms a sysinfo entry.
        #[cfg(target_os = "linux")]
        assert!(!collector.handles.is_empty());
        #[cfg(not(target_os = "linux"))]
        assert_eq!(collector.processes.processes().len(), 1);
        collector.sample(&[]).unwrap();
        assert!(collector.previous.is_empty());
        #[cfg(target_os = "linux")]
        assert!(collector.handles.is_empty());
        #[cfg(not(target_os = "linux"))]
        assert!(collector.processes.processes().is_empty());
    }

    #[test]
    fn inconsistent_or_missing_topology_is_unknown() {
        assert_eq!(visible_physical_cores(Some(8), 16), Some(8));
        assert_eq!(visible_physical_cores(Some(16), 8), None);
        assert_eq!(visible_physical_cores(Some(0), 8), None);
        assert_eq!(visible_physical_cores(Some(8), 0), None);
        assert_eq!(visible_physical_cores(None, 8), None);
    }

    #[test]
    fn gpu_memory_rounding_never_invents_headroom() {
        assert_eq!(gpu_memory_mib(MIB - 1, 0), None);
        assert_eq!(gpu_memory_mib(4 * MIB, 4 * MIB + 1), None);
        assert_eq!(gpu_memory_mib(4 * MIB + 1, 4 * MIB + 1), Some((4, 4)));
        assert_eq!(gpu_memory_mib(4 * MIB + 1, MIB + 1), Some((4, 2)));
        for total in [MIB, 4 * MIB + 1, u64::MAX] {
            for used in [0, 1, total / 2, total] {
                let (reported_total, reported_used) = gpu_memory_mib(total, used).unwrap();
                assert!(reported_used <= reported_total);
                assert!((reported_total - reported_used) * MIB <= total - used);
            }
        }
    }

    #[test]
    fn inconsistent_memory_is_unknown_instead_of_optimistically_clamped() {
        assert_eq!(available_ram_mib(4 * MIB, 4 * MIB + 1), None);
        assert_eq!(available_ram_mib(0, 0), None);
        assert_eq!(available_ram_mib(4 * MIB, MIB + 1), Some(1));
    }
    #[test]
    fn process_cursor_uses_driver_microseconds_and_rebaselines_after_gap() {
        let max_age = Duration::from_millis(731);
        assert_eq!(
            fresh_cursor(Some(1_750_000_123_456_789), Some(max_age), max_age),
            Some(1_750_000_123_456_789)
        );
        assert_eq!(
            fresh_cursor(Some(7), Some(max_age + Duration::from_nanos(1)), max_age),
            None
        );
        assert_eq!(fresh_cursor(Some(7), None, max_age), None);
        assert_eq!(fresh_cursor(None, Some(Duration::ZERO), max_age), None);
    }

    #[test]
    fn gpu_api_diagnostics_distinguish_no_new_samples_errors_and_actual_fresh_evidence() {
        let max_age = Duration::from_millis(731);
        for (response, expected) in [
            (Err(NvmlError::NotFound), "NotFound"),
            (Err(NvmlError::NotSupported), "NotSupported"),
            (Err(NvmlError::NoPermission), "NoPermission"),
            (Err(NvmlError::GpuLost), "GpuLost"),
            (Ok(vec![]), "NVML_SUCCESS"),
        ] {
            let observation = describe_gpu_observation(
                true,
                Some(100),
                Some(Duration::from_millis(20)),
                &response,
                None,
                1234,
                max_age,
            );
            assert_eq!(observation.activity_api_status, expected);
            assert_eq!(observation.query_cursor_us, 100);
            assert_eq!(
                observation.capability,
                match expected {
                    "GpuLost" => GpuExecutionCapability::InsufficientObservability,
                    // Healthy queries without a new sample are not activity evidence,
                    // but they are healthy observation (research R8).
                    "NotFound" | "NVML_SUCCESS" => GpuExecutionCapability::ContentionAware,
                    _ => GpuExecutionCapability::ConservativeNonSharing,
                }
            );
            assert!(!observation.fresh_after_baseline);
            assert!(
                observation
                    .freshness_decision
                    .contains("not_evidence_of_idleness")
            );
        }
        let samples = Ok(vec![gpu_sample(42, 101, 20)]);
        let fresh = describe_gpu_observation(
            true,
            Some(100),
            Some(Duration::from_millis(20)),
            &samples,
            Some(101),
            1234,
            max_age,
        );
        assert_eq!(fresh.capability, GpuExecutionCapability::ContentionAware);
        assert_eq!(fresh.newest_sample_timestamp_us, Some(101));
        assert_eq!(fresh.sample_max_age_ms, 731);
        let missing_basic = describe_gpu_observation(
            false,
            Some(100),
            Some(Duration::from_millis(20)),
            &samples,
            Some(101),
            1234,
            max_age,
        );
        assert_eq!(
            missing_basic.capability,
            GpuExecutionCapability::InsufficientObservability
        );
    }

    #[test]
    fn per_device_accounting_does_not_poison_other_device_or_cpu_admission() {
        let mut collector = Collector::new("scoped-gpu-accounting").unwrap();
        collector.gpu_accounting_known.insert("GPU-good".into());
        collector
            .managed_gpu_usage
            .insert(42, BTreeMap::from([("GPU-good".into(), 123)]));
        let good = crate::model::Resources {
            gpu_memory_mib: BTreeMap::from([("GPU-good".into(), 200)]),
            ..Default::default()
        };
        let bad = crate::model::Resources {
            gpu_memory_mib: BTreeMap::from([("GPU-bad".into(), 200)]),
            ..Default::default()
        };
        assert_eq!(collector.gpu_usage(42, &good)["GPU-good"], 123);
        // A device of unknown accounting has no entry; only that reading is unknown.
        assert!(!collector.gpu_usage(42, &bad).contains_key("GPU-bad"));
        let both = crate::model::Resources {
            gpu_memory_mib: BTreeMap::from([("GPU-good".into(), 200), ("GPU-bad".into(), 200)]),
            ..Default::default()
        };
        assert_eq!(
            collector.gpu_usage(42, &both),
            BTreeMap::from([("GPU-good".into(), 123)])
        );
        assert!(
            collector
                .gpu_usage(7, &crate::model::Resources::default())
                .is_empty()
        );
        // Known but undeclared contexts still reach the envelope checker.
        assert_eq!(
            collector.gpu_usage(42, &crate::model::Resources::default())["GPU-good"],
            123
        );
    }
    #[test]
    fn a_slow_process_query_keeps_its_raw_cursor_but_cannot_publish_fresh_activity() {
        let age = Duration::from_millis(732);
        let max_age = Duration::from_millis(731);
        let cursor = Some(100);
        let samples = Ok(vec![gpu_sample(42, 101, 70)]);
        let (_, fresh_id, next) = classify_activity(
            Some(&[42]),
            fresh_cursor(cursor, Some(age), max_age),
            samples.as_deref().ok(),
        );
        assert_eq!(fresh_id, None);
        assert_eq!(next, Some(101));
        let observation =
            describe_gpu_observation(true, cursor, Some(age), &samples, fresh_id, 1234, max_age);
        assert_eq!(observation.query_cursor_us, 100);
        assert_eq!(observation.newest_sample_timestamp_us, Some(101));
        // Healthy observation, but not fresh activity evidence.
        assert_eq!(
            observation.capability,
            GpuExecutionCapability::ContentionAware
        );
        assert!(!observation.fresh_after_baseline);
        assert!(observation.freshness_decision.contains("baseline_expired"));
    }

    fn external_identity_fixture() -> ExternalGpuProcessIdentity {
        ExternalGpuProcessIdentity {
            pid: 42,
            boot_id: "fixture-boot".into(),
            start_ticks: 731,
            uid: 1000,
        }
    }

    #[test]
    fn best_effort_external_inventory_requires_exact_identity_before_and_after() {
        let original = external_identity_fixture();
        let expected = vec![original.clone()];
        assert_eq!(
            verify_external_gpu_inventory(Some(&[42]), &expected, &expected, |_| Some(
                original.clone()
            )),
            Some(expected.clone())
        );
        assert_eq!(
            verify_external_gpu_inventory(None, &expected, &expected, |_| Some(original.clone())),
            None
        );
        assert_eq!(
            verify_external_gpu_inventory(Some(&[42]), &expected, &[], |_| Some(original.clone())),
            None
        );
        assert_eq!(
            verify_external_gpu_inventory(Some(&[42]), &expected, &expected, |_| None),
            None
        );
        assert_eq!(
            verify_external_gpu_inventory(Some(&[42, 42]), &expected, &expected, |_| Some(
                original.clone()
            )),
            None
        );
        assert_eq!(
            verify_external_gpu_inventory(Some(&[]), &expected, &expected, |_| None),
            Some(vec![])
        );
        for case in 0..4 {
            let mut changed = original.clone();
            match case {
                0 => changed.pid += 1,
                1 => changed.boot_id.push('x'),
                2 => changed.start_ticks += 1,
                3 => changed.uid += 1,
                _ => unreachable!(),
            }
            assert_eq!(
                verify_external_gpu_inventory(Some(&[42]), &expected, &expected, |_| Some(
                    changed.clone()
                )),
                None
            );
        }
        // A second context with the same UID is still unrelated and unauthorized.
        let mut unrelated = original.clone();
        unrelated.pid += 1;
        assert_eq!(
            verify_external_gpu_inventory(Some(&[42, 43]), &expected, &expected, |pid| Some(
                if pid == 42 {
                    original.clone()
                } else {
                    unrelated.clone()
                }
            )),
            None
        );
    }

    #[test]
    fn best_effort_injected_unavailable_activity_retains_unknown_and_error_evidence() {
        // Synthetic NVML failure injection only: no actual GPU result is claimed.
        let identity = external_identity_fixture();
        for error in [
            NvmlError::NotSupported,
            NvmlError::NotFound,
            NvmlError::NoPermission,
        ] {
            let samples = Err(error);
            let (activity, fresh_id, _) =
                classify_activity(Some(&[42]), Some(100), samples.as_deref().ok());
            assert_eq!(activity, ComputeActivity::Unknown);
            assert_eq!(fresh_id, None);
            let mut observation = describe_gpu_observation(
                true,
                Some(100),
                Some(Duration::from_millis(20)),
                &samples,
                fresh_id,
                777,
                Duration::from_millis(2000),
            );
            let error_detail = observation.activity_api_status.clone();
            observation.best_effort_external_processes = verify_external_gpu_inventory(
                Some(&[42]),
                std::slice::from_ref(&identity),
                std::slice::from_ref(&identity),
                |_| Some(identity.clone()),
            );
            // NOT_FOUND is a healthy query without samples; the others cannot
            // observe activity at all.
            assert_eq!(
                observation.capability,
                if error_detail == "NotFound" {
                    GpuExecutionCapability::ContentionAware
                } else {
                    GpuExecutionCapability::ConservativeNonSharing
                }
            );
            assert_eq!(observation.activity_api_status, error_detail);
            assert!(observation.best_effort_external_processes.is_some());
            assert!(!observation.fresh_after_baseline);
        }
        for error in [
            NvmlError::GpuLost,
            NvmlError::ResetRequired,
            NvmlError::DriverNotLoaded,
        ] {
            let observation = describe_gpu_observation(
                true,
                Some(100),
                Some(Duration::ZERO),
                &Err(error),
                None,
                777,
                Duration::from_millis(2000),
            );
            assert_eq!(
                observation.capability,
                GpuExecutionCapability::InsufficientObservability
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn external_gpu_identity_uses_live_native_process_markers() {
        let identity = external_gpu_process_identity(std::process::id()).unwrap();
        assert_eq!(identity.pid, std::process::id());
        let native = crate::supervision::process_identity(identity.pid, "fixture", 0).unwrap();
        assert_eq!(identity.start_ticks, native.start_time);
        assert_eq!(identity.boot_id, native.boot_id);
        assert!(external_gpu_process_identity(0).is_none());
    }

    const MS: u64 = 1_000_000;

    #[test]
    fn own_cpu_sources_carry_their_resolution_and_one_tick_per_running_thread() {
        let tick = MS;
        assert_eq!(leaf_quantization_ns(1, 4, tick), 1_000 + tick);
        assert_eq!(leaf_quantization_ns(9, 4, tick), 1_000 + 4 * tick);
        assert_eq!(clock_quantization_ns(1, 4, tick), 1 + tick);
        // A process always has at least its leader thread.
        assert_eq!(clock_quantization_ns(0, 4, tick), 1 + tick);
        assert_eq!(
            stat_quantization_ns(),
            cpu_counter_quantization_ms().map(|ms| ms * MS)
        );
        // A busy worker that used 250 ms of a 500 ms window: the clock loses about
        // 2 m with a 1 ms tick, the tick counters 44 m.
        let window = Duration::from_millis(500);
        assert_eq!(
            cpu_usage_lower_bound(250 * MS, window, clock_quantization_ns(1, 4, tick)),
            Some(497)
        );
        assert_eq!(cpu_usage_lower_bound(250 * MS, window, 22 * MS), Some(456));
    }

    #[test]
    fn the_scheduler_tick_comes_from_config_hz_or_the_slowest_kernel_tick() {
        assert_eq!(
            scheduler_tick_ns(Some("CONFIG_HZ_1000=y\nCONFIG_HZ=1000\n")),
            MS
        );
        assert_eq!(scheduler_tick_ns(Some("CONFIG_HZ=250\n")), 4 * MS);
        assert_eq!(scheduler_tick_ns(Some("# CONFIG_HZ is not set\n")), 10 * MS);
        assert_eq!(scheduler_tick_ns(Some("CONFIG_HZ=0\n")), 10 * MS);
        assert_eq!(scheduler_tick_ns(None), 10 * MS);
        assert!(!nohz_full_active(None));
        assert!(!nohz_full_active(Some("(null)\n")));
        assert!(!nohz_full_active(Some("\n")));
        assert!(nohz_full_active(Some("2-7\n")));
    }

    #[test]
    fn a_reading_from_another_source_or_running_backwards_gives_no_delta() {
        let clock = |ns, quantization_ns| CpuReading {
            source: CpuSource::Clock,
            ns,
            quantization_ns,
            threads: 1,
        };
        assert_eq!(cpu_delta(clock(100, 10), clock(400, 30)), Some((300, 30)));
        assert_eq!(cpu_delta(clock(400, 10), clock(100, 10)), None);
        let stat = CpuReading {
            source: CpuSource::Stat,
            ..clock(100, 10)
        };
        assert_eq!(cpu_delta(stat, clock(400, 10)), None);
        assert_eq!(cpu_delta(clock(100, 10), stat), None);
    }

    /// A fake leaf: `cgroup.procs` answers in turn, `cpu.stat` is fixed.
    fn leaf_files(
        members: Vec<&'static str>,
        cpu_stat: &'static str,
    ) -> impl FnMut(&str) -> Option<String> {
        let mut members = members.into_iter();
        move |file| match file {
            "cgroup.procs" => members.next().map(str::to_owned),
            "cpu.stat" => Some(cpu_stat.to_owned()),
            _ => None,
        }
    }

    #[test]
    fn the_leaf_counts_only_a_verified_own_membership() {
        let stat = "usage_usec 123456\nuser_usec 100000\nsystem_usec 23456\n";
        let family = BTreeSet::from([10, 11]);
        let one = |_| Some(1);
        let (reading, members) = leaf_reading_with(
            leaf_files(vec!["10\n", "10\n"], stat),
            10,
            &family,
            4,
            MS,
            one,
        )
        .unwrap();
        assert_eq!(
            reading,
            CpuReading {
                source: CpuSource::Leaf,
                ns: 123_456_000,
                quantization_ns: 1_000 + MS,
                threads: 1,
            }
        );
        assert_eq!(members, BTreeSet::from([10]));
        // A child of the family inside the leaf is counted by the leaf.
        let (_, members) = leaf_reading_with(
            leaf_files(vec!["10\n11\n", "11\n10\n"], stat),
            10,
            &family,
            4,
            MS,
            one,
        )
        .unwrap();
        assert_eq!(members, BTreeSet::from([10, 11]));
        // A foreign member, a membership change, a missing leader, no usage line or
        // an unreadable thread count each fall back.
        for (procs, stat) in [
            (vec!["10\n99\n", "10\n99\n"], stat),
            (vec!["10\n", "10\n11\n"], stat),
            (vec!["11\n", "11\n"], stat),
            (vec!["10\n", "10\n"], "user_usec 1\n"),
            (vec!["", ""], stat),
        ] {
            assert!(leaf_reading_with(leaf_files(procs, stat), 10, &family, 4, MS, one).is_none());
        }
        assert!(
            leaf_reading_with(
                leaf_files(vec!["10\n", "10\n"], stat),
                10,
                &family,
                4,
                MS,
                |_| None
            )
            .is_none()
        );
    }

    /// Full: a worker the leader forked joins the verified family, so a leaf of
    /// family members is read again; a worker outside the family, whose IDs differ, still
    /// refuses the leaf reading and is left to its owner (FR-085).
    #[test]
    fn a_forked_worker_in_the_family_keeps_the_leaf_reading() {
        let stat = "usage_usec 500000\n";
        let (leader, worker) = ("10\n20\n", "20\n10\n");
        let read = |family: &BTreeSet<u32>| {
            leaf_reading_with(
                leaf_files(vec![leader, worker], stat),
                10,
                family,
                4,
                MS,
                |_| Some(1),
            )
        };
        let (_, members) = read(&BTreeSet::from([10, 20])).expect("a leaf of family members");
        assert_eq!(members, BTreeSet::from([10, 20]));
        assert!(read(&BTreeSet::from([10])).is_none());
    }

    /// A family's CPU and memory count as own work: 4 forked workers reading a buffer they
    /// share copy-on-write with the leader add their CPU to the allocation and read as about
    /// one buffer by summed PSS, and none of it is the owner's unmanaged use (FR-085, R37).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_forked_family_is_measured_as_own_work() {
        use crate::execution_model::{AllocationClass, ExecutionPhase, ExecutionRecord};
        use std::io::BufRead;
        let python = std::env::var("CEDEGRID_TEST_PYTHON").unwrap_or_else(|_| "python3".into());
        let script = "import os, sys, time\n\
            buf = bytearray(b'x') * (192 << 20)\n\
            pids = []\n\
            for _ in range(4):\n    pid = os.fork()\n    if pid == 0:\n        \
            end = time.time() + 6\n        i = 0\n        \
            while time.time() < end:\n            i = (i + buf[i] + 4093) % len(buf)\n        \
            os._exit(0)\n    pids.append(pid)\n\
            print(' '.join(map(str, pids)), flush=True)\n\
            for pid in pids:\n    os.waitpid(pid, 0)\n";
        let mut leader = std::process::Command::new(python)
            .args(["-c", script])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(leader.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let identity = |pid: u32| crate::supervision::process_identity(pid, "family", 1).unwrap();
        let record = ExecutionRecord {
            task_id: "family".into(),
            assignment_id: "family".into(),
            generation: 1,
            class: AllocationClass::Opportunistic,
            phase: ExecutionPhase::Running,
            resources: crate::model::Resources::default(),
            identity: Some(identity(leader.id())),
            backend: "rootless".into(),
            evidence: vec![],
            detail: String::new(),
            modes: Default::default(),
        };
        let workers: Vec<_> = line
            .split_whitespace()
            .map(|pid| ("family".to_owned(), identity(pid.parse().unwrap())))
            .collect();
        assert_eq!(workers.len(), 4);
        let mut collector = ManagedCollector::new(&crate::config::Config::default()).unwrap();
        let sample = |collector: &mut ManagedCollector| {
            std::thread::sleep(
                collector
                    .next_cpu_sample_at()
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(800)),
            );
            collector
                .sample_with_descendants(std::slice::from_ref(&record), &[], &workers)
                .unwrap()
        };
        let (_, first) = sample(&mut collector);
        assert_eq!(first[0].usage_unknown.as_deref(), Some("first_interval"));
        let (snapshot, allocations) = sample(&mut collector);
        leader.kill().unwrap();
        leader.wait().unwrap();
        assert_eq!(allocations[0].usage_unknown, None);
        let observed = allocations[0].observed.clone().expect("a measured family");
        assert!(observed.cpu_millicores >= 2500, "{observed:?}");
        assert!(
            (150..=400).contains(&observed.ram_mib),
            "a shared 192 MiB buffer read as {} MiB",
            observed.ram_mib
        );
        let owner = snapshot
            .principals
            .iter()
            .flatten()
            .find(|principal| principal.is_owner)
            .map_or(0, |principal| principal.cpu_millicores);
        assert!(owner < 1500, "the owner's unmanaged use holds {owner} m");
    }

    #[test]
    fn a_family_member_inside_the_leaf_is_counted_once_by_the_leaf() {
        let parents = BTreeMap::from([
            ("managed-child:in".to_owned(), "a".to_owned()),
            ("managed-child:out".to_owned(), "a".to_owned()),
        ]);
        let (parent, members) = ("a".to_owned(), BTreeSet::from([10, 11]));
        let covered = BTreeMap::from([(&parent, &members)]);
        assert!(counted_by_leaf(
            &parents,
            &covered,
            &"managed-child:in".into(),
            11
        ));
        assert!(!counted_by_leaf(
            &parents,
            &covered,
            &"managed-child:out".into(),
            12
        ));
        // The allocation itself carries the leaf reading.
        assert!(!counted_by_leaf(&parents, &covered, &parent, 10));
        assert!(!counted_by_leaf(
            &parents,
            &BTreeMap::new(),
            &"managed-child:in".into(),
            11
        ));
    }

    fn own_only_evidence(listed: &[u32]) -> OwnOnlyEvidence<'_> {
        OwnOnlyEvidence {
            queries_ok: true,
            listed,
            mps: false,
            unattributed_memory_mib: Some(300),
            baseline_mib: Some(300),
            tolerance_mib: 32,
        }
    }

    #[test]
    fn an_inventory_of_only_own_verified_pids_is_own_only() {
        let own = |pid| pid == 42 || pid == 43;
        // A new own PID is own work like any other.
        assert!(own_only_interval(&own_only_evidence(&[42, 43]), own));
        // Memory growth within the tolerance still passes.
        let within = OwnOnlyEvidence {
            unattributed_memory_mib: Some(332),
            ..own_only_evidence(&[42])
        };
        assert!(own_only_interval(&within, own));
    }

    #[test]
    fn each_failing_own_only_condition_alone_keeps_the_existing_path() {
        let own = |pid| pid == 42;
        let cases = [
            // An external graphics process, a PID 0 entry, an unresolvable PID.
            own_only_evidence(&[42, 7]),
            own_only_evidence(&[42, 0]),
            own_only_evidence(&[42, 43]),
            OwnOnlyEvidence {
                mps: true,
                ..own_only_evidence(&[42])
            },
            OwnOnlyEvidence {
                unattributed_memory_mib: Some(333),
                ..own_only_evidence(&[42])
            },
            OwnOnlyEvidence {
                queries_ok: false,
                ..own_only_evidence(&[42])
            },
            OwnOnlyEvidence {
                unattributed_memory_mib: None,
                ..own_only_evidence(&[42])
            },
            // No baseline yet, or nothing listed.
            OwnOnlyEvidence {
                baseline_mib: None,
                ..own_only_evidence(&[42])
            },
            own_only_evidence(&[]),
        ];
        for case in cases {
            assert!(!own_only_interval(&case, own), "{:?}", case.listed);
        }
    }

    #[test]
    fn an_empty_interval_sets_the_memory_baseline_but_stays_on_the_existing_path() {
        assert!(sets_memory_baseline(false, true, Some(&[]), false));
        assert!(!own_only_interval(&own_only_evidence(&[]), |_| true));
        assert!(sets_memory_baseline(true, true, Some(&[42]), true));
        assert!(sets_memory_baseline(false, true, Some(&[7]), true));
        // An unknown interval with a listed process, a failed query or an
        // incomplete inventory sets none.
        assert!(!sets_memory_baseline(false, true, Some(&[7]), false));
        assert!(!sets_memory_baseline(false, false, Some(&[]), false));
        assert!(!sets_memory_baseline(false, true, None, false));
    }

    fn precise(delta_ns: u64, threads: u64, tick: u64) -> CpuPart {
        CpuPart {
            delta_ns,
            quantization_ns: clock_quantization_ns(threads, 4, tick),
            precise: Some((1, threads)),
            interval: Duration::from_millis(500),
        }
    }

    #[test]
    fn the_summed_bound_takes_one_tick_per_scope_cpu_for_many_single_thread_processes() {
        // 38 tasks of 100 m on 4 CPUs over 500 ms at a 1 ms tick: 3800 m used.
        let window = Duration::from_millis(500);
        let parts: Vec<CpuPart> = (0..38).map(|_| precise(50 * MS, 1, MS)).collect();
        let per_process: u64 = parts
            .iter()
            .map(|part| cpu_usage_lower_bound(part.delta_ns, window, part.quantization_ns).unwrap())
            .sum();
        assert_eq!(per_process, 3686);
        assert_eq!(summed_lower_bound(&parts, 4, MS), Some(3791));
    }

    #[test]
    fn the_summed_bound_matches_one_multi_thread_process() {
        let window = Duration::from_millis(500);
        let parts = [precise(2000 * MS, 8, MS)];
        assert_eq!(
            summed_lower_bound(&parts, 4, MS),
            cpu_usage_lower_bound(2000 * MS, window, clock_quantization_ns(8, 4, MS))
        );
    }

    #[test]
    fn the_summed_bound_never_exceeds_the_raw_delta_nor_falls_below_the_per_process_sum() {
        let window = Duration::from_millis(500);
        let stat = CpuPart {
            delta_ns: 50 * MS,
            quantization_ns: 22 * MS,
            precise: None,
            interval: window,
        };
        for parts in [
            vec![precise(50 * MS, 1, MS)],
            vec![precise(MS / 2, 1, MS), precise(0, 1, MS)],
            vec![stat, precise(250 * MS, 2, MS)],
            (0..9).map(|_| precise(100 * MS, 1, 4 * MS)).collect(),
        ] {
            let raw = parts.iter().map(|part| part.delta_ns).sum::<u64>() * 1000 / 500_000_000;
            let per_process: u64 = parts
                .iter()
                .map(|part| {
                    cpu_usage_lower_bound(part.delta_ns, window, part.quantization_ns).unwrap()
                })
                .sum();
            let summed = summed_lower_bound(&parts, 4, MS).unwrap();
            assert!(summed <= raw, "{summed} > {raw}");
            assert!(summed >= per_process, "{summed} < {per_process}");
        }
        // The tick counters keep their own rounding in full.
        assert_eq!(summed_lower_bound(&[stat], 4, MS), Some(56));
    }

    /// Each part is divided by its own interval, and the summed bound subtracts one tick per
    /// scope CPU but nothing for the length of the read pass (FR-073).
    #[test]
    fn the_summed_bound_divides_each_part_by_its_own_interval_without_a_read_pass_term() {
        // 117 tasks of 100 m on 14 CPUs, each read over 454 ms of a 500 ms interval.
        let parts: Vec<CpuPart> = (0..117)
            .map(|_| CpuPart {
                delta_ns: 45_400_000,
                quantization_ns: clock_quantization_ns(1, 14, MS),
                precise: Some((1, 1)),
                interval: Duration::from_millis(454),
            })
            .collect();
        let summed = summed_lower_bound(&parts, 14, MS).unwrap();
        // 11,700 m used; one tick per CPU over 454 ms is 30 m.
        assert_eq!(summed, 11_669);
        // Mixed sources keep their own errors: a tick-counter part subtracts its whole rounding,
        // a leaf part its 1 µs, a clock part 1 ns, and the tick lag is shared.
        let mixed = [
            CpuPart {
                delta_ns: 50 * MS,
                quantization_ns: 22 * MS,
                precise: None,
                interval: Duration::from_millis(500),
            },
            CpuPart {
                delta_ns: 100 * MS,
                quantization_ns: leaf_quantization_ns(2, 14, MS),
                precise: Some((1_000, 2)),
                interval: Duration::from_millis(500),
            },
            CpuPart {
                delta_ns: 100 * MS,
                quantization_ns: clock_quantization_ns(1, 14, MS),
                precise: Some((1, 1)),
                interval: Duration::from_millis(500),
            },
        ];
        // (50 - 22) + (100 - 0.001) + (100 - 0.000001) ms, less 3 ticks, over 500 ms.
        assert_eq!(summed_lower_bound(&mixed, 14, MS), Some(449));
    }

    /// An established process's delta is divided by its own read interval; one whose baseline
    /// was taken at its launch keeps the aggregate interval, so a partial first delta is never
    /// scaled to a full rate; a baseline before the aggregate interval stays unknown (FR-073).
    #[test]
    fn an_established_process_is_divided_by_its_own_read_interval_and_a_new_one_by_the_aggregate() {
        let started = Instant::now();
        let window = crate::kernel::CpuAccountingWindow {
            started,
            ended: started + Duration::from_millis(500),
            counter_interval: Duration::from_millis(500),
        };
        let q = clock_quantization_ns(1, 14, MS);
        let at = |ms: u64| started + Duration::from_millis(ms);
        assert_eq!(
            own_cpu_lower_bound(45_400_000, at(20), at(474), window, q, false),
            Some((97, Duration::from_millis(454)))
        );
        // The same delta at the aggregate divisor reads 10% low.
        assert_eq!(
            cpu_usage_lower_bound(45_400_000, window.duration(), q),
            Some(88)
        );
        assert_eq!(
            own_cpu_lower_bound(27_400_000, at(200), at(474), window, q, true),
            Some((52, Duration::from_millis(500)))
        );
        assert_eq!(
            own_cpu_lower_bound(
                45_400_000,
                started - Duration::from_millis(1),
                at(474),
                window,
                q,
                false
            ),
            None
        );
        assert_eq!(
            own_cpu_lower_bound(45_400_000, at(20), at(501), window, q, false),
            None
        );
    }

    /// In Full, own CPU comes from the delegated root's `cpu.stat`, read next to the host's CPU
    /// counters, only while the root holds no process and its children are exactly the own
    /// leaves; its error is 1 µs plus one tick per own thread that can run, at most one per scope
    /// CPU, with no read-pass term (FR-073).
    #[test]
    fn the_delegated_root_counts_own_cpu_only_when_its_children_are_the_own_leaves() {
        use std::ffi::OsString;
        let leaves: BTreeSet<OsString> = ["cedegrid-aa", "cedegrid-bb"].map(OsString::from).into();
        assert!(full_parent_applies(&leaves, "", &leaves));
        assert!(!full_parent_applies(&leaves, "1234\n", &leaves));
        let mut more = leaves.clone();
        more.insert(OsString::from("other"));
        assert!(!full_parent_applies(&more, "", &leaves));
        assert!(!full_parent_applies(&leaves, "", &more));
        assert!(!full_parent_applies(&BTreeSet::new(), "", &BTreeSet::new()));
        // 117 threads on 14 CPUs over 500 ms: 11,700 m used, 14 ticks and 1 µs subtracted.
        assert_eq!(
            parent_lower_bound(5_850_000_000, 117, 14, MS, Duration::from_millis(500)),
            Some(11_671)
        );
        assert_eq!(
            parse_usage_usec("usage_usec 42\nuser_usec 40\n"),
            Some(42_000)
        );
        assert_eq!(parse_usage_usec("user_usec 40\n"), None);
    }

    /// Host counters at 100 ticks a second for `cpus` CPUs each busy for `busy(t)` nanoseconds up
    /// to `t`, as `/proc/stat` text.
    fn proc_stat(cpus: u32, t_ns: u64, busy: impl Fn(u64) -> u64) -> String {
        let tick = 10_000_000;
        (0..cpus)
            .map(|id| {
                let busy_ns = busy(t_ns);
                format!(
                    "cpu{id} {} 0 0 {} 0 0 0 0\n",
                    busy_ns / tick,
                    (t_ns - busy_ns) / tick
                )
            })
            .collect()
    }

    /// A process clock that lags its true runtime by up to one tick at each read.
    fn lagged(runtime_ns: u64, seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        runtime_ns.saturating_sub((*seed >> 33) % MS)
    }

    /// 117 tasks of 100 m on 14 CPUs with no external work, read one after another over 46 ms
    /// before and after each aggregate sample: over the 8-interval window, host busy less own
    /// CPU stays at or below 1% of capacity (FR-073).
    #[test]
    fn a_window_keeps_the_external_estimate_within_one_percent_of_capacity_at_117_tasks() {
        const TASKS: u64 = 117;
        const CPUS: u32 = 14;
        let rate = 100; // millicores per task
        let per_cpu = |t: u64| t * TASKS * rate / 1000 / u64::from(CPUS);
        let runtime = |t: u64| t * rate / 1000;
        let mut tracker = crate::kernel::CpuUsageTracker::with_clock_tick_hz(
            std::num::NonZeroU64::new(100).unwrap(),
        );
        let ids: Vec<u32> = (0..CPUS).collect();
        let mut seed = 11;
        // Baseline read times and clocks per interval: after sample k, then end reads before k+1.
        let base_at = |k: u64, i: u64| k * 500 * MS + MS + 46 * MS * i / TASKS;
        let end_at = |k: u64, i: u64| k * 500 * MS - 47 * MS + 46 * MS * i / TASKS;
        let mut baselines: Vec<Vec<(u64, u64)>> = Vec::new();
        let mut checked = 0;
        for k in 0..20u64 {
            let usage = tracker.observe(
                Ok(&proc_stat(CPUS, k * 500 * MS, per_cpu)),
                Some(&ids),
                k * 500,
                1500,
            );
            if k >= 9 {
                let window = usage.busy_window_millicores.unwrap();
                let parts: Vec<CpuPart> = (0..TASKS)
                    .map(|i| {
                        let (start_ns, start_clock) = baselines[(k - 8) as usize][i as usize];
                        let end_ns = end_at(k, i);
                        CpuPart {
                            delta_ns: lagged(runtime(end_ns), &mut seed) - start_clock,
                            quantization_ns: clock_quantization_ns(1, u64::from(CPUS), MS),
                            precise: Some((1, 1)),
                            interval: Duration::from_nanos(end_ns - start_ns),
                        }
                    })
                    .collect();
                let own = summed_lower_bound(&parts, u64::from(CPUS), MS).unwrap();
                let external = window.saturating_sub(own);
                assert!(
                    external <= 140,
                    "interval {k}: window {window} m, own {own} m"
                );
                checked += 1;
            }
            baselines.push(
                (0..TASKS)
                    .map(|i| {
                        let at = base_at(k, i);
                        (at, lagged(runtime(at), &mut seed))
                    })
                    .collect(),
            );
        }
        assert_eq!(checked, 11);
    }

    /// Own work bursting from 2000 m to 3600 m on 4 CPUs, measured over each process's own read
    /// interval: the per-interval external estimate does not rise beyond its own bound, so the
    /// burst does not read as external growth (FR-073).
    #[test]
    fn own_work_bursting_on_four_cpus_does_not_read_as_external_growth() {
        const TASKS: u64 = 16;
        let burst_at = 2_250 * MS;
        let runtime = |t: u64| {
            if t <= burst_at {
                t * 125 / 1000
            } else {
                burst_at * 125 / 1000 + (t - burst_at) * 225 / 1000
            }
        };
        let per_cpu = |t: u64| runtime(t) * TASKS / 4;
        let mut tracker = crate::kernel::CpuUsageTracker::with_clock_tick_hz(
            std::num::NonZeroU64::new(100).unwrap(),
        );
        let ids = [0, 1, 2, 3];
        let mut seed = 5;
        let base_at = |k: u64, i: u64| k * 500 * MS + MS + 20 * MS * i / TASKS;
        let end_at = |k: u64, i: u64| k * 500 * MS - 21 * MS + 20 * MS * i / TASKS;
        let mut previous: Option<Vec<(u64, u64)>> = None;
        let mut estimates = Vec::new();
        for k in 0..10u64 {
            let usage = tracker.observe(
                Ok(&proc_stat(4, k * 500 * MS, per_cpu)),
                Some(&ids),
                k * 500,
                1500,
            );
            if let (Some(old), Some(busy), Some(lower)) = (
                &previous,
                usage.busy_millicores,
                usage.busy_lower_millicores,
            ) {
                let q = clock_quantization_ns(1, 4, MS);
                let parts: Vec<CpuPart> = (0..TASKS)
                    .map(|i| {
                        let (start_ns, start_clock) = old[i as usize];
                        let end_ns = end_at(k, i);
                        CpuPart {
                            delta_ns: lagged(runtime(end_ns), &mut seed) - start_clock,
                            quantization_ns: q,
                            precise: Some((1, 1)),
                            interval: Duration::from_nanos(end_ns - start_ns),
                        }
                    })
                    .collect();
                let own = summed_lower_bound(&parts, 4, MS).unwrap();
                let own_upper: u64 = parts
                    .iter()
                    .map(|part| cpu_usage_upper_bound(part.delta_ns, part.interval, q).unwrap())
                    .sum();
                estimates.push((busy.saturating_sub(own), (busy - lower) + (own_upper - own)));
            }
            previous = Some(
                (0..TASKS)
                    .map(|i| {
                        let at = base_at(k, i);
                        (at, lagged(runtime(at), &mut seed))
                    })
                    .collect(),
            );
        }
        for pair in estimates.windows(2) {
            let ((before, _), (after, bound)) = (pair[0], pair[1]);
            assert!(
                after <= before + bound,
                "external {before} -> {after} m beyond {bound} m"
            );
        }
    }

    /// Native CPU clock checks, run only when asked for on a Linux host.
    #[cfg(target_os = "linux")]
    fn cpu_clock_tests_enabled() -> bool {
        std::env::var_os("CEDEGRID_TEST_CPU_CLOCK").is_some_and(|value| value == "1")
    }

    #[cfg(target_os = "linux")]
    fn stat_cpu_ticks(pid: u32) -> u64 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let fields: Vec<u64> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .skip(11)
            .take(2)
            .map(|field| field.parse().unwrap())
            .collect();
        fields[0] + fields[1]
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "native CPU clock check: set CEDEGRID_TEST_CPU_CLOCK=1 on a Linux host"]
    fn the_process_clock_reads_a_busy_child_within_two_percent_of_wall_time() {
        if !cpu_clock_tests_enabled() {
            return;
        }
        let mut child = std::process::Command::new("sh")
            .args(["-c", "while :; do :; done"])
            .spawn()
            .unwrap();
        let identity = crate::supervision::process_identity(child.id(), "cpu-clock", 0).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let tick = scheduler_tick_ns(None);
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
        let (started, ticks_before) = (Instant::now(), stat_cpu_ticks(child.id()));
        let before = clock_reading(&identity, 1, tick).unwrap();
        std::thread::sleep(Duration::from_secs(2));
        let after = clock_reading(&identity, 1, tick).unwrap();
        let (wall, ticks_after) = (started.elapsed(), stat_cpu_ticks(child.id()));
        child.kill().unwrap();
        child.wait().unwrap();
        let (delta, _) = cpu_delta(before, after).unwrap();
        let wall = wall.as_nanos() as f64;
        assert!(
            (delta as f64 - wall).abs() <= wall * 0.02,
            "clock {delta} ns over {wall} ns of wall time"
        );
        let stat_ns = (ticks_after - ticks_before) * 1_000_000_000 / hz;
        assert!(
            delta.abs_diff(stat_ns) <= 2 * 1_000_000_000 / hz,
            "clock {delta} ns against /proc stat {stat_ns} ns"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "native CPU clock check: set CEDEGRID_TEST_CPU_CLOCK=1 on a Linux host"]
    fn the_process_clock_counts_threads_that_already_exited() {
        if !cpu_clock_tests_enabled() {
            return;
        }
        let python = std::env::var("CEDEGRID_TEST_PYTHON").unwrap_or_else(|_| "python3".into());
        // The main thread only joins; 100 short threads each spin for 10 ms.
        let script = "import threading,time,sys\n\
            def spin():\n    end=time.process_time()+0.01\n    while time.process_time()<end:pass\n\
            sys.stdin.readline()\n\
            for _ in range(100):\n    t=threading.Thread(target=spin);t.start();t.join()\n\
            sys.stdin.readline()\n";
        let mut child = std::process::Command::new(python)
            .args(["-c", script])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let identity = crate::supervision::process_identity(child.id(), "cpu-clock", 0).unwrap();
        let leader = |pid: u32| -> u64 {
            std::fs::read_to_string(format!("/proc/{pid}/schedstat"))
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap()
        };
        let tick = scheduler_tick_ns(None);
        std::thread::sleep(Duration::from_millis(300));
        let (before, leader_before) = (
            clock_reading(&identity, 1, tick).unwrap(),
            leader(child.id()),
        );
        use std::io::Write;
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
        // The threads together spin for at least 1 s of CPU time.
        std::thread::sleep(Duration::from_secs(3));
        let (after, leader_after) = (
            clock_reading(&identity, 1, tick).unwrap(),
            leader(child.id()),
        );
        child.stdin.as_mut().unwrap().write_all(b"\n").unwrap();
        child.wait().unwrap();
        let (delta, _) = cpu_delta(before, after).unwrap();
        assert!(
            delta >= 950 * MS,
            "clock counted {delta} ns of 1 s of exited threads"
        );
        assert!(
            leader_after - leader_before < delta / 2,
            "the leader thread alone ran {} ns",
            leader_after - leader_before
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "native CPU clock check: set CEDEGRID_TEST_CPU_CLOCK=1 on a Linux host"]
    fn an_exited_or_reused_pid_gives_no_clock_reading() {
        if !cpu_clock_tests_enabled() {
            return;
        }
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let identity = crate::supervision::process_identity(child.id(), "cpu-clock", 0).unwrap();
        assert!(clock_reading(&identity, 1, MS).is_some());
        // Another process's start time at this PID is a reused PID.
        let reused = crate::execution_model::ProcessIdentity {
            start_time: identity.start_time + 1,
            ..identity.clone()
        };
        assert!(clock_reading(&reused, 1, MS).is_none());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(clock_reading(&identity, 1, MS).is_none());
    }

    /// CedeGrid's own CPU is the agent's and its supervisors' clock deltas over wall time, per
    /// interval and over the window; a supervisor first seen is a baseline, and one gone adds
    /// nothing (FR-069).
    #[test]
    fn cedegrid_cpu_is_the_agent_and_supervisor_clock_deltas() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut cpu = CedegridCpu::default();
        assert_eq!(
            cpu.sample(at(0), 1_000_000, &[("a".into(), 5_000_000)]),
            None
        );
        // 500 ms later: the agent used 10 ms and supervisor a 15 ms; b is new.
        let first = cpu.sample(
            at(500),
            11_000_000,
            &[("a".into(), 20_000_000), ("b".into(), 90_000_000)],
        );
        assert_eq!(first, Some((50, 50)));
        // a is gone; the agent used 5 ms and b 45 ms.
        let second = cpu.sample(at(1000), 16_000_000, &[("b".into(), 135_000_000)]);
        assert_eq!(second, Some((100, 75)));
        // The window keeps the last eight intervals.
        for n in 0..8 {
            cpu.sample(at(1500 + n * 500), 16_000_000, &[("b".into(), 135_000_000)]);
        }
        assert_eq!(
            cpu.sample(at(5500), 16_000_000, &[("b".into(), 135_000_000)]),
            Some((0, 0))
        );
    }

    /// The agent reads its own and a supervisor's whole-process CPU from process clocks.
    #[cfg(target_os = "linux")]
    #[test]
    fn cedegrid_clocks_read_whole_process_cpu() {
        let before = own_process_cpu_ns().unwrap();
        let spin = Instant::now();
        while spin.elapsed() < Duration::from_millis(50) {
            std::hint::black_box(spin.elapsed());
        }
        assert!(own_process_cpu_ns().unwrap() >= before + 20_000_000);
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("while :; do :; done")
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(700));
        let used = supervisor_cpu_ns(child.id(), None).unwrap();
        assert!(used >= 100_000_000, "{used}");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// An own process is verified through `/proc` once; later checks go through the pidfd held
    /// since, a mismatched identity gets no handle, and an exited process loses its handle.
    #[cfg(target_os = "linux")]
    #[test]
    fn held_handles_reverify_own_processes_without_rereading_identity() {
        use std::cell::Cell;
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let identity = crate::supervision::process_identity(child.id(), "held", 0).unwrap();
        let reads = Cell::new(0);
        let open = |id: &crate::execution_model::ProcessIdentity| {
            reads.set(reads.get() + 1);
            verified_pidfd(id)
        };
        let mut handles = HeldHandles::default();
        assert!(handles.verify_with(&identity, &open));
        assert!(handles.verify_with(&identity, &open));
        assert!(handles.alive(&identity));
        assert_eq!(reads.get(), 1, "the second check reads no identity");
        let tick = scheduler_tick_ns(None);
        assert!(handles.clock_reading(&identity, 1, tick).is_some());
        let mut other = identity.clone();
        other.start_time += 1;
        assert!(!handles.verify_with(&other, &open));
        assert!(!handles.alive(&other));
        assert!(handles.verify_with(&identity, &open));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!handles.verify_with(&identity, &open));
        assert!(!handles.alive(&identity));
        assert!(handles.clock_reading(&identity, 1, tick).is_none());
        assert!(handles.is_empty());
    }

    /// A `/proc/<pid>/stat` line splits after the last parenthesis, whatever the command holds.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_stat_line_parses_whatever_the_command_holds() {
        let line = "4242 (a (b) c) S 1 4242 4242 0 -1 4194304 10 0 0 0 150 50 0 0 20 0 3 0 98765 \
                    1000000 512 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 2 0 0 0 0 0";
        let fields = processes::parse_stat(line).unwrap();
        assert_eq!(fields.cpu_ticks, 200);
        assert_eq!(fields.start_ticks, 98765);
        assert_eq!(fields.rss_pages, 512);
        assert_eq!(fields.parent, 1);
        assert!(processes::parse_stat("4242 (truncated").is_none());
    }

    /// A supervisor's marker comes only from its own command line, in its place after the spec
    /// path; on a System mode host it counts only inside one of that account's broker units
    /// (006 FR-001, FR-003).
    #[test]
    fn a_supervisor_marker_is_read_from_its_command_line_and_unit() {
        use crate::contention::ContentionMode;
        let marker = "cedegrid-work=v1;cpu=performance;gpu=neutral;arrival=42";
        let cmdline = |args: &[&str]| args.join("\0").into_bytes();
        let read = supervisor_marker(&cmdline(&[
            "/usr/bin/cedegrid",
            "__assignment-supervisor",
            "/home/user/.local/state/cedegrid/attempts/a/supervisor-spec.json",
            marker,
        ]));
        assert_eq!(
            read,
            Some(crate::execution_model::WorkMarker {
                cpu: ContentionMode::Performance,
                gpu: ContentionMode::Neutral,
                arrival_unix_ms: 42,
            })
        );
        for other in [
            cmdline(&["/usr/bin/cedegrid", "agent", "run", marker]),
            cmdline(&["/usr/bin/cedegrid", "__assignment-supervisor", marker]),
            cmdline(&["python3", "__assignment-supervisor", "spec", marker]),
            cmdline(&["/usr/bin/cedegrid", "__assignment-supervisor", "spec"]),
            Vec::new(),
        ] {
            assert_eq!(supervisor_marker(&other), None);
        }
        let unit = "0::/cedegrid.slice/cedegrid-u1001.service/jobs/a1\n";
        assert!(in_broker_unit(unit, 1001));
        assert!(in_broker_unit(
            "0::/user.slice/user-1001.slice/cedegrid-idle-u1001.service/jobs\n",
            1001
        ));
        assert!(!in_broker_unit(unit, 1002));
        assert!(!in_broker_unit(
            "0::/user.slice/user-1001.slice/session-3.scope\n",
            1001
        ));
        assert!(!in_broker_unit(
            "0::/user.slice/user-1001.slice/x-cedegrid-u1001.service\n",
            1001
        ));
        assert!(!in_broker_unit("", 1001));
    }

    /// The total takes the first eight fields of the `cpu` line: user and nice already hold
    /// guest and guest_nice, so adding those counts guest time twice and every process's CPU
    /// reads low on a host running guests.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_global_cpu_total_counts_guest_time_once() {
        let stat = "cpu  100 20 30 800 10 5 5 0 40 10\ncpu0 50 10 15 400 5 3 2 0 20 5\n\
                    cpu1 50 10 15 400 5 2 3 0 20 5\nbtime 1700000000\n";
        assert_eq!(processes::parse_global(stat), Some((970, 2, 1_700_000_000)));
    }

    /// Per-process CPU is computed as sysinfo computes it: CPU ticks over the global ticks per
    /// CPU, at most the CPU count (research R26).
    #[cfg(target_os = "linux")]
    #[test]
    fn process_cpu_is_ticks_over_global_ticks_per_cpu() {
        assert_eq!(processes::cpu_millicores(50, 400, 4), 500);
        assert_eq!(processes::cpu_millicores(1000, 400, 4), 4000);
        assert_eq!(processes::cpu_millicores(0, 400, 4), 0);
        assert_eq!(processes::cpu_millicores(5, 0, 4), 4000);
    }

    /// A process table that cannot be read is no sample, not an empty host: the table keeps
    /// its state, and the next readable sample lists the processes again (FR-077).
    #[cfg(target_os = "linux")]
    #[test]
    fn an_unreadable_process_table_is_not_an_empty_one() {
        let mut table = processes::ProcessTable::new().unwrap();
        assert!(table.sample().is_some_and(|samples| !samples.is_empty()));
        // With no descriptor left, every open fails as on a host out of them.
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        let none = libc::rlimit {
            rlim_cur: 0,
            ..limit
        };
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &none) }, 0);
        let unreadable = table.sample();
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        assert!(unreadable.is_none());
        assert!(table.sample().is_some_and(|samples| !samples.is_empty()));
    }

    /// The process table lists processes, never their threads, reads each process's UID once,
    /// and gives a busy process its CPU from the second sample on (research R26, FR-059).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_process_table_counts_processes_once_and_reads_a_uid_once() {
        let spin = std::thread::spawn(|| std::thread::sleep(Duration::from_millis(800)));
        let mut child = std::process::Command::new("sh")
            .args(["-c", "while :; do :; done"])
            .spawn()
            .unwrap();
        let mut table = processes::ProcessTable::new().unwrap();
        let first = table.sample().unwrap();
        let me = std::process::id();
        let busy = child.id();
        assert!(first.iter().all(|p| p.cpu_millicores == 0));
        let threads: Vec<u32> = std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .filter(|tid| *tid != me)
            .collect();
        assert!(!threads.is_empty(), "the test runs threads");
        assert!(first.iter().any(|p| p.pid == me));
        assert!(first.iter().all(|p| !threads.contains(&p.pid)));
        let pids: BTreeSet<u32> = first.iter().map(|p| p.pid).collect();
        assert_eq!(pids.len(), first.len(), "one entry per process");
        let reads = table.uid_reads();
        std::thread::sleep(Duration::from_millis(500));
        let second = table.sample().unwrap();
        let child_cpu = second
            .iter()
            .find(|p| p.pid == busy)
            .unwrap()
            .cpu_millicores;
        assert!(child_cpu >= 300, "{child_cpu}");
        assert_eq!(
            second.iter().find(|p| p.pid == me).unwrap().uid,
            Some(unsafe { libc::getuid() })
        );
        assert!(
            table.uid_reads() <= reads + 2,
            "a UID is read once per process"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        spin.join().unwrap();
    }

    /// On Linux an own process's tick counters and resident memory are read from `/proc`
    /// directly, and match what sysinfo's refresh gave for the same process.
    #[cfg(target_os = "linux")]
    #[test]
    fn own_process_counters_match_sysinfos() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let collector = ManagedCollector::new(&crate::config::Config::default()).unwrap();
        // A process still loading changes its resident memory between the two reads.
        std::thread::sleep(Duration::from_millis(300));
        let pid = sysinfo::Pid::from_u32(child.id());
        let mut system = System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        let process = system.process(pid).unwrap();
        assert_eq!(collector.resident_bytes(child.id()), Some(process.memory()));
        assert_eq!(
            collector.stat_cpu_ms(child.id()),
            Some(process.accumulated_cpu_time())
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
