//! Own work's usage: live attribution of registered processes beside the host
//! snapshot, with CPU usage bounds.
use super::*;

/// Matching-scope live attribution for explicitly registered single-process jobs.
/// The registry is checked on both sides of sampling to fence PID reuse. Process
/// RSS is not subtracted from system memory: shared/file-backed pages would invent
/// capacity. Linux anonymous proportional memory is the conservative subtraction.
pub struct ManagedCollector {
    pub collector: Collector,
    pub(super) processes: System,
    pub(super) scoped_cpu: bool,
    pub(super) previous: BTreeMap<String, (crate::execution_model::ProcessIdentity, u64, Instant)>,
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
                modes: Default::default(),
            });
        }
        let (snapshot, mut allocations) = self.sample(&augmented)?;
        let child_allocations: Vec<_> = allocations
            .iter()
            .filter(|a| parents.contains_key(&a.id))
            .cloned()
            .collect();
        allocations.retain(|a| !parents.contains_key(&a.id));
        let add = |total: Option<Resources>, usage: Option<Resources>| match (total, usage) {
            (Some(mut total), Some(usage)) => total.add_checked(&usage).ok().map(|()| total),
            _ => None,
        };
        for child in child_allocations {
            if let Some(parent) = allocations.iter_mut().find(|a| a.id == parents[&child.id]) {
                parent.observed_upper = add(parent.observed_upper.take(), child.observed_upper);
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
            let bounds = (|| -> Option<(Resources, Resources)> {
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
                let delta = ticks.checked_sub(old.1)?;
                let quantization = cpu_counter_quantization_ms()?;
                let cpu_millicores = aligned_cpu_usage_lower_bound(
                    delta,
                    old.2,
                    before_ended,
                    cpu_window?,
                    quantization,
                )?;
                let ram_mib = anonymous_memory_mib(id.pid)?;
                let gpu_memory_mib = self.collector.gpu_usage(id.pid, &record.resources)?;
                // Upper bounds: the rate over the process's own interval plus
                // quantization, and resident memory including shared pages.
                let upper = Resources {
                    cpu_millicores: cpu_usage_upper_bound(
                        delta,
                        before_ended.checked_duration_since(old.2)?,
                        quantization,
                    )?
                    .max(cpu_millicores),
                    ram_mib: process.memory().div_ceil(MIB).max(ram_mib),
                    gpu_memory_mib: gpu_memory_mib.clone(),
                };
                Some((
                    Resources {
                        cpu_millicores,
                        ram_mib,
                        gpu_memory_mib,
                    },
                    upper,
                ))
            })();
            let (observed, observed_upper) = bounds.unzip();
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
            let pending = matches!(
                record.phase,
                ExecutionPhase::Reserved | ExecutionPhase::Prepared
            );
            allocations.push(Allocation {
                id: record.assignment_id.clone(),
                class: record.class,
                phase: match record.phase {
                    ExecutionPhase::Reserved | ExecutionPhase::Prepared => AllocationPhase::Pending,
                    ExecutionPhase::Draining => AllocationPhase::Draining,
                    _ => AllocationPhase::Running,
                },
                requested: record.resources.clone(),
                observed: if pending { None } else { observed },
                observed_upper: if pending { None } else { observed_upper },
                modes: record.modes.clone(),
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

pub(super) fn aligned_cpu_usage_lower_bound(
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

pub(super) fn cpu_usage_lower_bound(
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

/// Upper bound over the process's own measurement interval, which lies inside
/// the aggregate window, with the counter quantization added.
pub(super) fn cpu_usage_upper_bound(
    delta_ms: u64,
    process_interval: Duration,
    quantization_ms: u64,
) -> Option<u64> {
    let nanos = process_interval.as_nanos();
    if nanos == 0 {
        return None;
    }
    u64::try_from(u128::from(delta_ms.saturating_add(quantization_ms)) * 1_000_000_000 / nanos).ok()
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
