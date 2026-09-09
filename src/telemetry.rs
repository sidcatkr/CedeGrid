//! Read-only, local telemetry. No workload execution or resource controls live here.
//!
//! CPU quantities use logical millicores. GPU utilization is kernel busy time,
//! never a measure of spare compute capacity or proof of interference.
use crate::model::{
    Capability, CapabilityStatus, ComputeActivity, ExternalGpuProcessIdentity,
    GpuExecutionCapability, GpuObservation, GpuSnapshot, SCHEMA_VERSION, Snapshot,
};
use anyhow::{Result, ensure};
use nvml_wrapper::{Nvml, error::NvmlError, struct_wrappers::device::ProcessUtilizationSample};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{CpuRefreshKind, System};

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
    gpu_sample_max_age: Duration,
    gpu_config: crate::config::GpuConfig,
    platform_capabilities: BTreeMap<String, Capability>,
    kernel: crate::kernel::KernelCollector,
    managed_pids: BTreeSet<u32>,
    managed_gpu_usage: BTreeMap<u32, BTreeMap<String, u64>>,
    gpu_accounting_known: BTreeSet<String>,
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
            gpu_sample_max_age: Duration::from_millis(
                crate::config::GpuConfig::default().process_sample_max_age_ms,
            ),
            gpu_config: crate::config::GpuConfig::default(),
            platform_capabilities: platform_capabilities(),
            kernel: crate::kernel::KernelCollector::new(crate::kernel::KernelConfig::default()),
            managed_pids: BTreeSet::new(),
            managed_gpu_usage: BTreeMap::new(),
            gpu_accounting_known: BTreeSet::new(),
        })
    }

    pub fn with_kernel(node_id: &str, config: crate::kernel::KernelConfig) -> Result<Self> {
        let mut collector = Self::new(node_id)?;
        collector.kernel = crate::kernel::KernelCollector::new(config);
        Ok(collector)
    }

    pub fn for_config(config: &crate::config::Config) -> Result<Self> {
        let mut collector = Self::with_kernel(&config.node_id, config.kernel.clone())?;
        collector.gpu_sample_max_age = Duration::from_millis(config.gpu.process_sample_max_age_ms);
        collector.gpu_config = config.gpu.clone();
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
    }
    pub fn gpu_usage(
        &self,
        pid: u32,
        requested: &crate::model::Resources,
    ) -> Option<BTreeMap<String, u64>> {
        if requested
            .gpu_memory_mib
            .keys()
            .any(|id| !self.gpu_accounting_known.contains(id))
        {
            return None;
        }
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
        Some(usage)
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
        let (gpu_inventory, gpus) = self.sample_gpus(&mut capabilities, observed_at_unix_ms);
        Ok(Snapshot {
            schema_version: SCHEMA_VERSION,
            node_id: self.node_id.clone(),
            observed_at_unix_ms,
            cpu_capacity_millicores: capacity,
            physical_cores: self.physical_cores,
            cpu_busy_millicores: cpu_busy,
            total_ram_mib: total_ram,
            available_ram_mib: available_ram,
            gpu_inventory,
            gpus,
            capabilities,
            kernel: Some(self.kernel.sample()),
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
            let memory = device.memory_info();
            let normalized_memory = memory
                .as_ref()
                .ok()
                .and_then(|memory| gpu_memory_mib(memory.total, memory.used));
            let (total_memory_mib, used_memory_mib) = match normalized_memory {
                Some((total, used)) => {
                    capabilities.insert(format!("gpu:{uuid}:memory"), capability(CapabilityStatus::Available, "Device-wide bytes; all occupied memory is external or unattributed in observe-only mode."));
                    (total, Some(used))
                }
                None => {
                    capabilities.insert(
                        format!("gpu:{uuid}:memory"),
                        capability(
                            CapabilityStatus::Unavailable,
                            format!("Memory unavailable or inconsistent: {memory:?}"),
                        ),
                    );
                    (0, None)
                }
            };
            let mut accounting_known = normalized_memory.is_some();
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
            let external_process_ids =
                merge_process_lists(compute.as_deref().ok(), graphics.as_deref().ok()).map(
                    |pids| {
                        pids.into_iter()
                            .filter(|p| !self.managed_pids.contains(p))
                            .collect()
                    },
                );
            if let (Ok(compute), Ok(graphics)) = (&compute_info, &graphics_info) {
                for process in compute.iter().chain(graphics) {
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
            }
            capabilities.insert(format!("gpu:{uuid}:processes"), capability(
                if external_process_ids.is_some() { CapabilityStatus::Available } else { CapabilityStatus::Unknown },
                format!("Compute and graphics enumeration must both succeed; Only registry-verified managed identities are excluded; all other PIDs remain external. Compute error: {:?}; graphics error: {:?}", compute.err(), graphics.err()),
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
            // Include query latency in the freshness bound. Preserve the actual
            // submitted cursor in diagnostics even if it expires during the call.
            let baseline_age = baseline.map(|(_, at)| at.elapsed());
            let activity_baseline = fresh_cursor(previous, baseline_age, self.gpu_sample_max_age);
            let (external_compute, compute_sample_id, next_cursor) = classify_activity(
                external_process_ids.as_deref(),
                activity_baseline,
                samples.as_deref().ok(),
            );
            if let Some(cursor) = next_cursor {
                // An unchanged timestamp never refreshes the age of the baseline.
                if previous.is_none() || baseline.is_none_or(|(old, _)| cursor > old) {
                    self.gpu_cursors
                        .insert(uuid.clone(), (cursor, Instant::now()));
                }
            }
            let mut observation = describe_gpu_observation(
                normalized_memory.is_some() && external_process_ids.is_some(),
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
            });
        }
        // A failed per-device query withholds that device's capacity only. Device
        // UUIDs remain stable keys; an unidentifiable index is never schedulable.
        capabilities.insert("gpu_inventory".into(), capability(inventory.clone(), "Stable UUID observations for NVML devices. Individual failures remain per-device diagnostics and never disable other qualified devices. Other vendor adapters are not implemented."));
        (inventory, gpus)
    }
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

fn gpu_memory_mib(total: u64, used: u64) -> Option<(u64, u64)> {
    let total_mib = total / MIB;
    (total_mib > 0 && used <= total).then(|| (total_mib, used.div_ceil(MIB).min(total_mib)))
}

fn available_ram_mib(total: u64, available: u64) -> Option<u64> {
    (total >= MIB && available <= total).then_some(available / MIB)
}

fn merge_process_lists(compute: Option<&[u32]>, graphics: Option<&[u32]>) -> Option<Vec<u32>> {
    let processes: Vec<u32> = compute?
        .iter()
        .chain(graphics?)
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    (!processes.contains(&0)).then_some(processes)
}

/// Native identity for an independently tracked external Linux process. Requiring
/// all four UID slots to agree prevents a credential transition from retaining a
/// stale authorization. Other platforms cannot establish this contract yet.
pub fn external_gpu_process_identity(pid: u32) -> Option<ExternalGpuProcessIdentity> {
    #[cfg(target_os = "linux")]
    {
        if pid == 0 {
            return None;
        }
        let native = crate::supervision::process_identity(pid, "external-gpu-identity", 0).ok()?;
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let uids: Vec<u32> = status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))?
            .split_whitespace()
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()
            .ok()?;
        if uids.len() != 4
            || uids.iter().any(|uid| *uid != uids[0])
            || native.start_time == 0
            || native.boot_id.trim().is_empty()
        {
            return None;
        }
        Some(ExternalGpuProcessIdentity {
            pid,
            boot_id: native.boot_id,
            start_ticks: native.start_time,
            uid: uids[0],
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

fn verify_external_gpu_inventory(
    external: Option<&[u32]>,
    authorized: &[ExternalGpuProcessIdentity],
    before: &[ExternalGpuProcessIdentity],
    mut read_after: impl FnMut(u32) -> Option<ExternalGpuProcessIdentity>,
) -> Option<Vec<ExternalGpuProcessIdentity>> {
    let external = external?;
    if authorized.is_empty() {
        return None;
    }
    let unique: BTreeSet<_> = external.iter().copied().collect();
    if unique.len() != external.len() || unique.contains(&0) {
        return None;
    }
    unique
        .into_iter()
        .map(|pid| {
            let identity = read_after(pid)?;
            (identity.pid == pid
                && identity.start_ticks > 0
                && !identity.boot_id.trim().is_empty()
                && authorized.contains(&identity)
                && before.contains(&identity))
            .then_some(identity)
        })
        .collect()
}

fn fresh_cursor(previous: Option<u64>, age: Option<Duration>, max_age: Duration) -> Option<u64> {
    previous.filter(|_| age.is_some_and(|age| age <= max_age))
}

fn describe_gpu_observation(
    basic_known: bool,
    previous: Option<u64>,
    baseline_age: Option<Duration>,
    samples: &std::result::Result<Vec<ProcessUtilizationSample>, NvmlError>,
    fresh_id: Option<u64>,
    observed_at_unix_ms: u64,
    max_age: Duration,
) -> GpuObservation {
    let newest = samples
        .as_ref()
        .ok()
        .and_then(|s| s.iter().map(|s| s.timestamp).max());
    let device_error = matches!(
        samples,
        Err(NvmlError::GpuLost
            | NvmlError::ResetRequired
            | NvmlError::DriverNotLoaded
            | NvmlError::Uninitialized
            | NvmlError::LibRmVersionMismatch
            | NvmlError::OperatingSystem)
    );
    let capability = if !basic_known || device_error {
        GpuExecutionCapability::InsufficientObservability
    } else if fresh_id.is_some() {
        GpuExecutionCapability::ContentionAware
    } else {
        GpuExecutionCapability::ConservativeNonSharing
    };
    let freshness_decision = if fresh_id.is_some() {
        "new_driver_timestamp_after_recent_baseline"
    } else if baseline_age.is_some_and(|age| age > max_age) {
        "baseline_expired_before_query_completion_buffered_history_not_activity"
    } else if previous.is_none() {
        "no_recent_baseline_buffered_history_not_activity"
    } else {
        "no_new_driver_timestamp_not_evidence_of_idleness"
    };
    GpuObservation {
        capability,
        best_effort_external_processes: None,
        activity_api: "nvmlDeviceGetProcessUtilization".into(),
        activity_api_status: match samples {
            Ok(_) => "NVML_SUCCESS".into(),
            Err(error) => format!("{error:?}"),
        },
        query_cursor_us: previous.unwrap_or(0),
        newest_sample_timestamp_us: newest,
        samples_returned: samples.as_ref().map_or(0, Vec::len),
        observed_at_unix_ms,
        baseline_age_ms: baseline_age.map(|v| v.as_millis().min(u128::from(u64::MAX)) as u64),
        sample_max_age_ms: max_age.as_millis().min(u128::from(u64::MAX)) as u64,
        fresh_after_baseline: fresh_id.is_some(),
        freshness_decision: freshness_decision.into(),
    }
}

fn classify_activity(
    processes: Option<&[u32]>,
    previous: Option<u64>,
    samples: Option<&[ProcessUtilizationSample]>,
) -> (ComputeActivity, Option<u64>, Option<u64>) {
    let newest = samples.and_then(|samples| samples.iter().map(|s| s.timestamp).max());
    let next = previous.into_iter().chain(newest).max();
    // Discard all buffered samples on the first successful read. A repeated
    // timestamp (including overlapping polling windows) is not new evidence.
    let fresh_id = previous.and_then(|old| newest.filter(|new| *new > old));
    let activity = match processes {
        Some([]) => ComputeActivity::Idle,
        Some(pids)
            if fresh_id.is_some()
                && samples.is_some_and(|samples| {
                    samples.iter().any(|sample| {
                        sample.timestamp > previous.unwrap_or(u64::MAX)
                            && sample.sm_util > 0
                            && sample.sm_util <= 100
                            && pids.contains(&sample.pid)
                    })
                }) =>
        {
            ComputeActivity::Active
        }
        _ => ComputeActivity::Unknown,
    };
    (activity, fresh_id, next)
}

fn error_status(error: &NvmlError) -> CapabilityStatus {
    match error {
        NvmlError::NotSupported | NvmlError::FunctionNotFound => CapabilityStatus::Unsupported,
        NvmlError::NoData | NvmlError::NotFound => CapabilityStatus::Unknown,
        _ => CapabilityStatus::Unavailable,
    }
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

/// GPU admission requires complete device/process APIs before launch. Release is
/// a fresh successful absence check for the verified managed PID after OS exit;
/// it is not a promise about unrelated allocations or a hard VRAM partition.
pub struct GpuReleaseGuard {
    nvml: Option<Nvml>,
    uuids: Vec<String>,
}
impl GpuReleaseGuard {
    pub fn prepare(resources: &crate::model::Resources) -> Result<Self> {
        if resources.gpu_memory_mib.is_empty() {
            return Ok(Self {
                nvml: None,
                uuids: Vec::new(),
            });
        }
        ensure!(
            cfg!(target_os = "linux"),
            "GPU execution release qualification is Linux-only"
        );
        let nvml = Nvml::init()?;
        for (uuid, amount) in &resources.gpu_memory_mib {
            ensure!(*amount > 0, "GPU reservation must be positive");
            let device = nvml.device_by_uuid(uuid.as_str())?;
            ensure!(
                device.memory_info()?.total / MIB >= *amount,
                "GPU reservation exceeds device memory"
            );
            let compute = device.running_compute_processes()?;
            let graphics = device.running_graphics_processes()?;
            ensure!(
                compute.iter().chain(&graphics).all(|p| p.pid > 0),
                "GPU process inventory contains invalid identity"
            );
        }
        Ok(Self {
            nvml: Some(nvml),
            uuids: resources.gpu_memory_mib.keys().cloned().collect(),
        })
    }
    /// One fresh observation, never a sleep; family supervisors poll every member
    /// so a delayed GPU context cannot stall another member's termination clock.
    pub fn is_released(&self, pid: u32) -> Result<bool> {
        let Some(nvml) = &self.nvml else {
            return Ok(true);
        };
        for uuid in &self.uuids {
            let device = nvml.device_by_uuid(uuid.as_str())?;
            let compute = device.running_compute_processes()?;
            let graphics = device.running_graphics_processes()?;
            ensure!(
                compute.iter().chain(&graphics).all(|p| p.pid > 0),
                "GPU process inventory contains invalid identity"
            );
            if compute.iter().chain(&graphics).any(|p| p.pid == pid) {
                return Ok(false);
            }
        }
        Ok(true)
    }
    pub fn confirm(&self, pid: u32, timeout: Duration) -> Result<()> {
        let start = Instant::now();
        loop {
            if self.is_released(pid)? {
                return Ok(());
            }
            ensure!(
                start.elapsed() < timeout,
                "managed GPU context remains visible after verified process exit"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Matching-scope live attribution for explicitly registered single-process jobs.
/// The registry is checked on both sides of sampling to fence PID reuse. Process
/// RSS is not subtracted from system memory: shared/file-backed pages would invent
/// capacity. Linux anonymous proportional memory is the conservative subtraction.
pub struct ManagedCollector {
    pub collector: Collector,
    processes: System,
    scoped_cpu: bool,
    previous: BTreeMap<String, (crate::execution_model::ProcessIdentity, u64, Instant)>,
}
impl ManagedCollector {
    pub fn new(config: &crate::config::Config) -> Result<Self> {
        Ok(Self {
            collector: Collector::for_config(config)?,
            processes: System::new(),
            scoped_cpu: config.kernel.enabled,
            previous: BTreeMap::new(),
        })
    }
    pub(crate) fn next_cpu_sample_at(&self) -> Instant {
        self.collector.next_cpu_sample_at()
    }
    pub fn prime(&mut self, records: &[crate::execution_model::ExecutionRecord]) {
        let ids: Vec<_> = records
            .iter()
            .filter_map(|r| r.identity.as_ref().map(|p| sysinfo::Pid::from_u32(p.pid)))
            .collect();
        let refresh_started = Instant::now();
        self.processes
            .refresh_processes(sysinfo::ProcessesToUpdate::Some(&ids), true);
        for record in records {
            if let Some(identity) = &record.identity
                && crate::supervision::process_identity(
                    identity.pid,
                    &identity.assignment_id,
                    identity.generation,
                )
                .ok()
                .as_ref()
                    == Some(identity)
                && let Some(p) = self.processes.process(sysinfo::Pid::from_u32(identity.pid))
            {
                self.previous
                    .entry(record.assignment_id.clone())
                    .or_insert((identity.clone(), p.accumulated_cpu_time(), refresh_started));
            }
        }
    }
    /// Aggregate explicitly registered family members into exactly one allocation.
    /// A pending/uncertain child makes family usage unknown, preserving reservation.
    pub fn sample_with_children(
        &mut self,
        records: &[crate::execution_model::ExecutionRecord],
        children: &[crate::managed_children::ManagedChildRecord],
    ) -> Result<(Snapshot, Vec<crate::model::Allocation>)> {
        use crate::{
            execution_model::{ExecutionPhase, ExecutionRecord},
            managed_children::ManagedChildPhase,
            model::Resources,
        };
        let mut augmented = records.to_vec();
        let mut parents = BTreeMap::new();
        for child in children
            .iter()
            .filter(|c| c.phase != ManagedChildPhase::Released)
        {
            if !records.iter().any(|r| {
                r.assignment_id == child.assignment_id
                    && r.generation == child.generation
                    && r.phase != ExecutionPhase::Released
            }) {
                continue;
            }
            let key = format!("managed-child:{}", child.child_id);
            parents.insert(key.clone(), child.assignment_id.clone());
            augmented.push(ExecutionRecord {
                task_id: child.request.task_id.clone(),
                assignment_id: key,
                generation: child.generation,
                class: child.request.class,
                phase: match child.phase {
                    ManagedChildPhase::Reserved => ExecutionPhase::Reserved,
                    ManagedChildPhase::Prepared => ExecutionPhase::Prepared,
                    ManagedChildPhase::Draining => ExecutionPhase::Draining,
                    ManagedChildPhase::NeedsReconciliation => ExecutionPhase::NeedsReconciliation,
                    _ => ExecutionPhase::Running,
                },
                resources: Resources::default(),
                identity: child.identity.clone(),
                backend: "mediated_child".into(),
                evidence: vec![],
                detail: String::new(),
            });
        }
        let (snapshot, mut allocations) = self.sample(&augmented)?;
        let child_allocations: Vec<_> = allocations
            .iter()
            .filter(|a| parents.contains_key(&a.id))
            .cloned()
            .collect();
        allocations.retain(|a| !parents.contains_key(&a.id));
        for child in child_allocations {
            if let Some(parent) = allocations.iter_mut().find(|a| a.id == parents[&child.id]) {
                parent.observed = match (parent.observed.take(), child.observed) {
                    (Some(mut total), Some(usage)) => (|| -> Option<Resources> {
                        total.cpu_millicores =
                            total.cpu_millicores.checked_add(usage.cpu_millicores)?;
                        total.ram_mib = total.ram_mib.checked_add(usage.ram_mib)?;
                        for (id, n) in usage.gpu_memory_mib {
                            let old = total.gpu_memory_mib.get(&id).copied().unwrap_or(0);
                            total.gpu_memory_mib.insert(id, old.checked_add(n)?);
                        }
                        Some(total)
                    })(),
                    _ => None,
                };
            }
        }
        Ok((snapshot, allocations))
    }
    pub fn sample(
        &mut self,
        records: &[crate::execution_model::ExecutionRecord],
    ) -> Result<(Snapshot, Vec<crate::model::Allocation>)> {
        use crate::{
            execution_model::ExecutionPhase,
            model::{Allocation, AllocationPhase, Resources},
        };
        let active: Vec<_> = records
            .iter()
            .filter(|r| r.phase != ExecutionPhase::Released)
            .collect();
        self.previous.retain(|assignment, _| {
            active
                .iter()
                .any(|record| record.assignment_id == *assignment)
        });
        let verified: BTreeSet<_> = active
            .iter()
            .filter_map(|r| r.identity.as_ref())
            .filter(|id| {
                crate::supervision::process_identity(id.pid, &id.assignment_id, id.generation)
                    .ok()
                    .as_ref()
                    == Some(id)
            })
            .map(|id| id.pid)
            .collect();
        self.collector.set_managed_pids(verified.clone());
        let pids: Vec<_> = verified
            .iter()
            .map(|p| sysinfo::Pid::from_u32(*p))
            .collect();
        // Only retain handles for currently verified managed identities.
        if self
            .processes
            .processes()
            .keys()
            .any(|pid| !pids.contains(pid))
        {
            self.processes = System::new();
        }
        // A process delta must sit INSIDE the aggregate CPU interval. The
        // previous baseline was read after its aggregate sample; this endpoint
        // is read before the next aggregate sample, including its GPU/kernel
        // collection cost. A worker launched midway through the interval is
        // divided by the aggregate interval, never its shorter lifetime.
        self.processes.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&pids),
            true,
            sysinfo::ProcessRefreshKind::nothing().with_cpu(),
        );
        let before: BTreeMap<_, _> = self
            .processes
            .processes()
            .iter()
            .map(|(pid, process)| (*pid, process.accumulated_cpu_time()))
            .collect();
        let before_ended = Instant::now();
        let mut snapshot = self.collector.sample()?;
        let cpu_window = if self.scoped_cpu && cfg!(target_os = "linux") {
            self.collector.kernel.cpu_accounting_window()
        } else {
            self.collector.cpu_window
        };
        snapshot.capabilities.insert("managed_memory_attribution".into(),capability(
            if cfg!(target_os="linux"){CapabilityStatus::Available}else{CapabilityStatus::Unsupported},
            "Managed RAM subtraction uses anonymous proportional physical memory on Linux, excluding reclaimable file mappings. Other platforms subtract a conservative zero lower bound and retain declared reservations; this is not a measurement of zero workload RAM and not hard memory enforcement."));
        snapshot.capabilities.insert("managed_cpu_attribution".into(),capability(
            if cpu_counter_quantization_ms().is_some(){CapabilityStatus::Available}else{CapabilityStatus::Unsupported},
            "Managed CPU is a conservative counter-derived lower bound over the aggregate CPU sample window. Process deltas lie inside aggregate read boundaries; Linux uses at least the largest selected-core tick interval as divisor. New workers use the whole aggregate interval, counter quantization is subtracted, and reservations remain charged in full. Missing attribution is unknown; this is not hard CPU enforcement."));
        let refresh_started = Instant::now();
        self.processes
            .refresh_processes(sysinfo::ProcessesToUpdate::Some(&pids), true);
        let mut allocations = Vec::new();
        let mut cpu_diagnostics = Vec::new();
        for record in active {
            let old_cpu = self.previous.get(&record.assignment_id).cloned();
            let observed = (|| -> Option<Resources> {
                let id = record.identity.as_ref()?;
                if !verified.contains(&id.pid)
                    || crate::supervision::process_identity(
                        id.pid,
                        &id.assignment_id,
                        id.generation,
                    )
                    .ok()
                    .as_ref()
                        != Some(id)
                {
                    return None;
                }
                if !process_scope_matches(id.pid, &snapshot, self.scoped_cpu) {
                    return None;
                }
                let process = self.processes.process(sysinfo::Pid::from_u32(id.pid))?;
                let ticks = before.get(&sysinfo::Pid::from_u32(id.pid))?;
                let old = self.previous.insert(
                    record.assignment_id.clone(),
                    (id.clone(), process.accumulated_cpu_time(), refresh_started),
                )?;
                if old.0 != *id {
                    return None;
                }
                let cpu_millicores = aligned_cpu_usage_lower_bound(
                    ticks.checked_sub(old.1)?,
                    old.2,
                    before_ended,
                    cpu_window?,
                    cpu_counter_quantization_ms()?,
                )?;
                let ram_mib = anonymous_memory_mib(id.pid)?;
                Some(Resources {
                    cpu_millicores,
                    ram_mib,
                    gpu_memory_mib: self.collector.gpu_usage(id.pid, &record.resources)?,
                })
            })();
            // Bounded read-only evidence makes a rejected subtraction
            // diagnosable even if the workload drains before the next heartbeat.
            if cpu_diagnostics.len() < 64
                && let Some(id) = record.identity.as_ref()
            {
                let pid = sysinfo::Pid::from_u32(id.pid);
                cpu_diagnostics.push(serde_json::json!({
                    "assignment_id":record.assignment_id,"pid":id.pid,
                    "previous_cpu_ms":old_cpu.as_ref().map(|old|old.1),
                    "before_cpu_ms":before.get(&pid),
                    "next_baseline_cpu_ms":self.processes.process(pid).map(|process|process.accumulated_cpu_time()),
                    "process_baseline_after_aggregate_start_ns":old_cpu.as_ref().and_then(|old|cpu_window.and_then(|window|old.2.checked_duration_since(window.started))).map(|duration|duration.as_nanos()),
                    "process_read_before_aggregate_end_ns":cpu_window.and_then(|window|window.ended.checked_duration_since(before_ended)).map(|duration|duration.as_nanos()),
                    "aggregate_wall_ns":cpu_window.map(|window|window.ended.duration_since(window.started).as_nanos()),
                    "aggregate_counter_ns":cpu_window.map(|window|window.counter_interval.as_nanos()),
                    "divisor_ns":cpu_window.map(|window|window.duration().as_nanos()),
                    "quantization_ms":cpu_counter_quantization_ms(),
                    "observed_cpu_millicores":observed.as_ref().map(|value|value.cpu_millicores)
                }));
            }
            allocations.push(Allocation {
                id: record.assignment_id.clone(),
                class: record.class,
                phase: match record.phase {
                    ExecutionPhase::Reserved | ExecutionPhase::Prepared => AllocationPhase::Pending,
                    ExecutionPhase::Draining => AllocationPhase::Draining,
                    _ => AllocationPhase::Running,
                },
                requested: record.resources.clone(),
                observed: if matches!(
                    record.phase,
                    ExecutionPhase::Reserved | ExecutionPhase::Prepared
                ) {
                    None
                } else {
                    observed
                },
            });
        }
        if !cpu_diagnostics.is_empty()
            && let Some(evidence) = snapshot.capabilities.get_mut("managed_cpu_attribution")
        {
            evidence
                .detail
                .push_str(" Counter/window evidence (first 64 registered identities): ");
            evidence
                .detail
                .push_str(&serde_json::to_string(&cpu_diagnostics)?);
        }
        Ok((snapshot, allocations))
    }
}

fn aligned_cpu_usage_lower_bound(
    delta_ms: u64,
    process_baseline_started: Instant,
    process_read_ended: Instant,
    aggregate: crate::kernel::CpuAccountingWindow,
    quantization_ms: u64,
) -> Option<u64> {
    // No subtraction when a process baseline precedes this aggregate interval:
    // it could include execution that the aggregate did not observe. A missing
    // or changed baseline remains unknown rather than being represented as idle.
    if process_baseline_started < aggregate.started
        || process_read_ended > aggregate.ended
        || process_read_ended < process_baseline_started
    {
        return None;
    }
    cpu_usage_lower_bound(delta_ms, aggregate.duration(), quantization_ms)
}

fn cpu_usage_lower_bound(
    delta_ms: u64,
    enclosing_interval: Duration,
    quantization_ms: u64,
) -> Option<u64> {
    let nanos = enclosing_interval.as_nanos();
    if nanos == 0 {
        return None;
    }
    u64::try_from(u128::from(delta_ms.saturating_sub(quantization_ms)) * 1_000_000_000 / nanos).ok()
}

fn cpu_counter_quantization_ms() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        // /proc/<pid>/stat utime/stime use _SC_CLK_TCK; account for both
        // counters' tick rounding plus the library's millisecond conversion.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if hz <= 0 {
            return None;
        }
        Some(2 * 1000u64.div_ceil(hz as u64) + 2)
    }
    #[cfg(target_os = "macos")]
    {
        // Recount publishes task CPU counters at context switches and user/kernel
        // transitions. Integral-millisecond conversion alone does not bound the
        // short-interval refresh jitter of another running task. Include two
        // runtime accounting ticks, without changing the configured CPU budget.
        #[repr(C)]
        struct ClockInfo {
            hz: i32,
            tick: i32,
            tickadj: i32,
            stathz: i32,
            profhz: i32,
        }
        let mut clock: ClockInfo = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<ClockInfo>();
        let result = unsafe {
            libc::sysctlbyname(
                c"kern.clockrate".as_ptr(),
                (&mut clock as *mut ClockInfo).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0
            || size != std::mem::size_of::<ClockInfo>()
            || clock.hz <= 0
            || clock.tick <= 0
        {
            return None;
        }
        Some(2 * (clock.tick as u64).div_ceil(1000) + 2)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}
#[cfg(target_os = "linux")]
fn anonymous_memory_mib(pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
    let anon = text
        .lines()
        .find_map(|l| l.strip_prefix("Pss_Anon:"))
        .and_then(|v| v.split_whitespace().next())?
        .parse::<u64>()
        .ok()?;
    // Floor ensures the subtracted managed usage does not exceed the evidence.
    Some(anon / 1024)
}
#[cfg(not(target_os = "linux"))]
fn anonymous_memory_mib(_pid: u32) -> Option<u64> {
    // No matching anonymous attribution on this backend. Zero is a known safe
    // lower bound for subtraction, not a claim that the process uses no RAM.
    Some(0)
}
#[cfg(target_os = "linux")]
fn process_scope_matches(pid: u32, snapshot: &Snapshot, scoped: bool) -> bool {
    if !scoped {
        return snapshot.cpu_capacity_millicores > 0;
    }
    let Some(allowed) = snapshot
        .kernel
        .as_ref()
        .and_then(|k| k.cpu.effective_cpu_ids.as_ref())
    else {
        return false;
    };
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return false;
    };
    let Some(list) = status
        .lines()
        .find_map(|s| s.strip_prefix("Cpus_allowed_list:"))
    else {
        return false;
    };
    crate::kernel::parse_cpu_list(list.trim())
        .is_ok_and(|ids| ids.iter().all(|id| allowed.contains(id)))
}
#[cfg(not(target_os = "linux"))]
fn process_scope_matches(_pid: u32, _snapshot: &Snapshot, _scoped: bool) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(cpu_usage_lower_bound(501, Duration::from_micros(500_500), 2).unwrap() <= 1000);
        // Full nanoseconds and enclosing refresh endpoints avoid truncated-wall
        // and variable collector-cost bias. Actual extra-thread demand survives.
        assert_eq!(
            cpu_usage_lower_bound(1100, Duration::from_secs(1), 2),
            Some(1098)
        );
        assert!(cpu_usage_lower_bound(525, Duration::from_millis(525), 22).unwrap() <= 1000);
        assert_eq!(cpu_usage_lower_bound(1, Duration::from_secs(1), 2), Some(0));
        assert_eq!(cpu_usage_lower_bound(1, Duration::ZERO, 2), None);
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
        let old_wrong_rate = cpu_usage_lower_bound(170, Duration::from_millis(300), 22).unwrap();
        let aligned = aligned_cpu_usage_lower_bound(
            170,
            started + Duration::from_millis(200),
            started + Duration::from_millis(500),
            window,
            22,
        )
        .unwrap();
        assert!(old_wrong_rate > 322);
        assert_eq!(aligned, 290);
        assert!(aligned <= 322);
        // Neither a missing baseline nor execution outside aggregate endpoints
        // is converted into a measurement of zero CPU.
        assert_eq!(
            aligned_cpu_usage_lower_bound(
                170,
                started - Duration::from_millis(1),
                window.ended,
                window,
                22
            ),
            None
        );
        assert_eq!(
            aligned_cpu_usage_lower_bound(
                170,
                started,
                window.ended + Duration::from_millis(1),
                window,
                22
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
                300,
                started + Duration::from_millis(50),
                started + Duration::from_millis(450),
                window,
                22,
            ),
            Some(463)
        );
        // Actual extra-thread excess remains observable; the envelope and
        // rounding allowance were not increased to hide the native failure.
        assert!(
            aligned_cpu_usage_lower_bound(900, started, window.ended, window, 22).unwrap() > 1000
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
        };
        collector.prime(&[record]);
        assert_eq!(collector.previous.len(), 1);
        assert_eq!(collector.processes.processes().len(), 1);
        collector.sample(&[]).unwrap();
        assert!(collector.previous.is_empty());
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
                if expected == "GpuLost" {
                    GpuExecutionCapability::InsufficientObservability
                } else {
                    GpuExecutionCapability::ConservativeNonSharing
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
        assert_eq!(collector.gpu_usage(42, &good).unwrap()["GPU-good"], 123);
        assert!(collector.gpu_usage(42, &bad).is_none());
        assert!(
            collector
                .gpu_usage(7, &crate::model::Resources::default())
                .unwrap()
                .is_empty()
        );
        // Known but undeclared contexts still reach the envelope checker.
        assert_eq!(
            collector
                .gpu_usage(42, &crate::model::Resources::default())
                .unwrap()["GPU-good"],
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
        assert_eq!(
            observation.capability,
            GpuExecutionCapability::ConservativeNonSharing
        );
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
            assert_eq!(
                observation.capability,
                GpuExecutionCapability::ConservativeNonSharing
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
}
