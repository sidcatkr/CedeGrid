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
    pub(super) previous:
        BTreeMap<String, (crate::execution_model::ProcessIdentity, CpuReading, Instant)>,
    /// One scheduler tick, the precise sources' read lag per running thread.
    tick_ns: u64,
    /// Whether the leaf and clock sources apply; a host with `nohz_full` CPUs
    /// keeps the `/proc` counters.
    precise_cpu: bool,
}

/// The running kernel's build configuration, where the distribution installs it.
fn kernel_config() -> Option<String> {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok()?;
    std::fs::read_to_string(format!("/boot/config-{}", release.trim())).ok()
}

/// The CPUs the agent, and so its workers, may run on.
fn scope_cpus() -> u64 {
    std::thread::available_parallelism().map_or(u64::MAX, |n| n.get() as u64)
}

/// An allocation's verified cgroup leaf, from its launch evidence.
fn leaf_path(record: &crate::execution_model::ExecutionRecord) -> Option<&std::path::Path> {
    if record.backend != "cgroup_v2" {
        return None;
    }
    record
        .evidence
        .iter()
        .find(|evidence| evidence.control == "cgroup.procs" && evidence.applied)
        .map(|evidence| std::path::Path::new(&evidence.scope))
}

impl ManagedCollector {
    pub fn new(config: &crate::config::Config) -> Result<Self> {
        Ok(Self {
            collector: Collector::for_config(config)?,
            processes: System::new(),
            scoped_cpu: config.kernel.enabled,
            previous: BTreeMap::new(),
            tick_ns: scheduler_tick_ns(kernel_config().as_deref()),
            precise_cpu: cfg!(target_os = "linux")
                && !nohz_full_active(
                    std::fs::read_to_string("/sys/devices/system/cpu/nohz_full")
                        .ok()
                        .as_deref(),
                ),
        })
    }
    /// This interval's CPU reading for one record, from the first source that
    /// applies, with the leaf members when the leaf was read.
    fn read_cpu(
        &self,
        record: &crate::execution_model::ExecutionRecord,
        family: &BTreeSet<u32>,
        stat_ms: Option<u64>,
        cpus: u64,
    ) -> (Option<CpuReading>, Option<BTreeSet<u32>>) {
        let Some(identity) = record.identity.as_ref() else {
            return (None, None);
        };
        #[cfg(target_os = "linux")]
        if self.precise_cpu {
            if let Some((reading, members)) = leaf_path(record)
                .and_then(|leaf| leaf_reading(leaf, identity.pid, family, cpus, self.tick_ns))
            {
                return (Some(reading), Some(members));
            }
            if let Some(reading) = clock_reading(identity, cpus, self.tick_ns) {
                return (Some(reading), None);
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (
            identity,
            family,
            cpus,
            leaf_path(record),
            self.tick_ns,
            self.precise_cpu,
        );
        let reading = stat_ms.and_then(|ms| {
            Some(CpuReading {
                source: CpuSource::Stat,
                ns: ms.checked_mul(1_000_000)?,
                quantization_ns: stat_quantization_ns()?,
            })
        });
        (reading, None)
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
        let cpus = scope_cpus();
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
                && !self.previous.contains_key(&record.assignment_id)
                && let (Some(reading), _) = self.read_cpu(
                    record,
                    &BTreeSet::from([identity.pid]),
                    self.processes
                        .process(sysinfo::Pid::from_u32(identity.pid))
                        .map(|p| p.accumulated_cpu_time()),
                    cpus,
                )
            {
                self.previous.insert(
                    record.assignment_id.clone(),
                    (identity.clone(), reading, refresh_started),
                );
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
        let (snapshot, mut allocations) = self.sample_records(&augmented, &parents)?;
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
        self.sample_records(records, &BTreeMap::new())
    }
    /// `parents` maps a managed child's record to its allocation, so that family
    /// members inside the allocation's leaf are counted once, by the leaf.
    fn sample_records(
        &mut self,
        records: &[crate::execution_model::ExecutionRecord],
        parents: &BTreeMap<String, String>,
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
        self.collector.set_managed_identities(
            active
                .iter()
                .filter_map(|record| record.identity.clone())
                .filter(|id| verified.contains(&id.pid))
                .map(|id| (id.pid, id))
                .collect(),
        );
        let owner = |assignment: &String| parents.get(assignment).unwrap_or(assignment).clone();
        let mut families: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
        for record in &active {
            if let Some(id) = record
                .identity
                .as_ref()
                .filter(|id| verified.contains(&id.pid))
            {
                families
                    .entry(owner(&record.assignment_id))
                    .or_default()
                    .insert(id.pid);
            }
        }
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
        let cpus = scope_cpus();
        let end_reads: BTreeMap<String, (Option<CpuReading>, Option<BTreeSet<u32>>)> = active
            .iter()
            .filter_map(|record| {
                let id = record
                    .identity
                    .as_ref()
                    .filter(|id| verified.contains(&id.pid))?;
                let family = families.get(&owner(&record.assignment_id))?;
                let stat_ms = before.get(&sysinfo::Pid::from_u32(id.pid)).copied();
                Some((
                    record.assignment_id.clone(),
                    self.read_cpu(record, family, stat_ms, cpus),
                ))
            })
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
            "Managed CPU is a conservative lower bound over the aggregate CPU sample window, from the allocation's verified cgroup leaf (cpu.stat usage_usec, only while its members are own verified processes), else the process CPU clock read under a pidfd, else /proc tick counters. Process deltas lie inside aggregate read boundaries; Linux uses at least the largest selected-core tick interval as divisor. New workers use the whole aggregate interval; each source's resolution plus one scheduler tick per thread that can be running (the tick counters: two ticks) is subtracted, and reservations remain charged in full. A source change or missing attribution is unknown; this is not hard CPU enforcement."));
        let refresh_started = Instant::now();
        self.processes
            .refresh_processes(sysinfo::ProcessesToUpdate::Some(&pids), true);
        let mut allocations = Vec::new();
        let mut cpu_diagnostics = Vec::new();
        // A member the allocation's leaf already counts adds no CPU of its own.
        let covered: BTreeMap<&String, &BTreeSet<u32>> = end_reads
            .iter()
            .filter_map(|(assignment, (_, members))| Some((assignment, members.as_ref()?)))
            .collect();
        for record in active {
            let old_cpu = self.previous.get(&record.assignment_id).cloned();
            let end = end_reads.get(&record.assignment_id).and_then(|end| end.0);
            let leaf_counted = record.identity.as_ref().is_some_and(|id| {
                counted_by_leaf(parents, &covered, &record.assignment_id, id.pid)
            });
            let baseline = record
                .identity
                .as_ref()
                .filter(|id| verified.contains(&id.pid))
                .and_then(|id| {
                    let family = families.get(&owner(&record.assignment_id))?;
                    let stat_ms = self
                        .processes
                        .process(sysinfo::Pid::from_u32(id.pid))
                        .map(|process| process.accumulated_cpu_time());
                    self.read_cpu(record, family, stat_ms, cpus).0
                });
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
                let Some(baseline) = baseline else {
                    self.previous.remove(&record.assignment_id);
                    return None;
                };
                let old = self.previous.insert(
                    record.assignment_id.clone(),
                    (id.clone(), baseline, refresh_started),
                )?;
                if old.0 != *id {
                    return None;
                }
                let (delta, quantization) = cpu_delta(old.1, end?)?;
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
                let upper_cpu = cpu_usage_upper_bound(
                    delta,
                    before_ended.checked_duration_since(old.2)?,
                    quantization,
                )?
                .max(cpu_millicores);
                let (cpu_millicores, upper_cpu) = if leaf_counted {
                    (0, 0)
                } else {
                    (cpu_millicores, upper_cpu)
                };
                let upper = Resources {
                    cpu_millicores: upper_cpu,
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
                    "cpu_source":end.map(|end|end.source.name()),
                    "leaf_counted":leaf_counted,
                    "previous_cpu_ns":old_cpu.as_ref().map(|old|old.1.ns),
                    "end_cpu_ns":end.map(|end|end.ns),
                    "next_baseline_cpu_ns":self.previous.get(&record.assignment_id).map(|next|next.1.ns),
                    "stat_cpu_ms":before.get(&pid),
                    "process_baseline_after_aggregate_start_ns":old_cpu.as_ref().and_then(|old|cpu_window.and_then(|window|old.2.checked_duration_since(window.started))).map(|duration|duration.as_nanos()),
                    "process_read_before_aggregate_end_ns":cpu_window.and_then(|window|window.ended.checked_duration_since(before_ended)).map(|duration|duration.as_nanos()),
                    "aggregate_wall_ns":cpu_window.map(|window|window.ended.duration_since(window.started).as_nanos()),
                    "aggregate_counter_ns":cpu_window.map(|window|window.counter_interval.as_nanos()),
                    "divisor_ns":cpu_window.map(|window|window.duration().as_nanos()),
                    "quantization_ns":end.map(|end|end.quantization_ns),
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
    delta_ns: u64,
    process_baseline_started: Instant,
    process_read_ended: Instant,
    aggregate: crate::kernel::CpuAccountingWindow,
    quantization_ns: u64,
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
    cpu_usage_lower_bound(delta_ns, aggregate.duration(), quantization_ns)
}

/// CPU time over an enclosing interval in millicores, the read error subtracted.
pub(super) fn cpu_usage_lower_bound(
    delta_ns: u64,
    enclosing_interval: Duration,
    quantization_ns: u64,
) -> Option<u64> {
    let nanos = enclosing_interval.as_nanos();
    if nanos == 0 {
        return None;
    }
    u64::try_from(u128::from(delta_ns.saturating_sub(quantization_ns)) * 1000 / nanos).ok()
}

/// Upper bound over the process's own measurement interval, which lies inside
/// the aggregate window, with the counter quantization added.
pub(super) fn cpu_usage_upper_bound(
    delta_ns: u64,
    process_interval: Duration,
    quantization_ns: u64,
) -> Option<u64> {
    let nanos = process_interval.as_nanos();
    if nanos == 0 {
        return None;
    }
    u64::try_from(u128::from(delta_ns.saturating_add(quantization_ns)) * 1000 / nanos).ok()
}

/// Where an allocation's CPU time came from: its leaf's `cpu.stat`, the process
/// CPU clock, or the `/proc` tick counters.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CpuSource {
    Leaf,
    Clock,
    Stat,
}

impl CpuSource {
    fn name(self) -> &'static str {
        match self {
            CpuSource::Leaf => "leaf_cpu_stat",
            CpuSource::Clock => "process_cpu_clock",
            CpuSource::Stat => "proc_stat_ticks",
        }
    }
}

/// A cumulative CPU time with the error one read of its source may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CpuReading {
    pub(super) source: CpuSource,
    pub(super) ns: u64,
    pub(super) quantization_ns: u64,
}

/// CPU time between two reads of the same source, with the larger of their
/// errors. Another source or a counter that ran backwards gives no delta, as a
/// changed baseline does.
pub(super) fn cpu_delta(old: CpuReading, new: CpuReading) -> Option<(u64, u64)> {
    if old.source != new.source {
        return None;
    }
    Some((
        new.ns.checked_sub(old.ns)?,
        old.quantization_ns.max(new.quantization_ns),
    ))
}

/// The kernel folds a running thread's pending runtime into the group clock and
/// into `cpu.stat` only at its next scheduler tick (`thread_group_cputime` skips
/// other CPUs' pending time; `cpu.stat` takes what `update_se` accounted). So a
/// read can lag by one tick per thread that can be running: the smaller of the
/// threads and the CPUs they may use, and at least the leader.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn running_threads(threads: u64, cpus: u64) -> u64 {
    threads.min(cpus).max(1)
}

/// The leaf's `usage_usec` resolution (1 µs) plus the tick lag.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn leaf_quantization_ns(threads: u64, cpus: u64, tick_ns: u64) -> u64 {
    1_000 + running_threads(threads, cpus) * tick_ns
}

/// The clock's resolution (1 ns) plus the tick lag.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn clock_quantization_ns(threads: u64, cpus: u64, tick_ns: u64) -> u64 {
    1 + running_threads(threads, cpus) * tick_ns
}

pub(super) fn stat_quantization_ns() -> Option<u64> {
    cpu_counter_quantization_ms().map(|ms| ms * 1_000_000)
}

/// One scheduler tick from the kernel's `CONFIG_HZ`, or 10 ms, the tick of the
/// lowest HZ the kernel supports, when the configuration is unreadable.
pub(super) fn scheduler_tick_ns(kernel_config: Option<&str>) -> u64 {
    kernel_config
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.trim().strip_prefix("CONFIG_HZ="))
                .and_then(|hz| hz.trim().parse::<u64>().ok())
        })
        .filter(|hz| *hz > 0)
        .map_or(10_000_000, |hz| 1_000_000_000 / hz)
}

/// CPUs listed in `nohz_full` stop their tick and fold pending runtime in only
/// about once a second, so the precise sources' lag is not bounded by a tick.
pub(super) fn nohz_full_active(list: Option<&str>) -> bool {
    list.map(str::trim)
        .is_some_and(|list| !list.is_empty() && list != "(null)")
}

/// The leaf's CPU time when its members, read before and after `cpu.stat`, are
/// the same set, contain the leader and lie within the family's own verified
/// PIDs. A foreign member would make own usage read high, the unsafe direction.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn leaf_reading_with(
    mut read: impl FnMut(&str) -> Option<String>,
    leader: u32,
    family: &BTreeSet<u32>,
    cpus: u64,
    tick_ns: u64,
    threads: impl Fn(u32) -> Option<u64>,
) -> Option<(CpuReading, BTreeSet<u32>)> {
    let members = |text: String| -> Option<BTreeSet<u32>> {
        text.split_whitespace()
            .map(|pid| pid.parse().ok())
            .collect()
    };
    let before = members(read("cgroup.procs")?)?;
    let stat = read("cpu.stat")?;
    let after = members(read("cgroup.procs")?)?;
    if before != after || !before.contains(&leader) || !before.is_subset(family) {
        return None;
    }
    let usage_usec: u64 = stat
        .lines()
        .find_map(|line| line.strip_prefix("usage_usec "))?
        .trim()
        .parse()
        .ok()?;
    let threads = before
        .iter()
        .map(|pid| threads(*pid))
        .sum::<Option<u64>>()?;
    Some((
        CpuReading {
            source: CpuSource::Leaf,
            ns: usage_usec.checked_mul(1_000)?,
            quantization_ns: leaf_quantization_ns(threads, cpus, tick_ns),
        },
        before,
    ))
}

/// Whether a managed child's CPU is already in its allocation's leaf reading.
pub(super) fn counted_by_leaf(
    parents: &BTreeMap<String, String>,
    covered: &BTreeMap<&String, &BTreeSet<u32>>,
    assignment: &String,
    pid: u32,
) -> bool {
    parents
        .get(assignment)
        .and_then(|parent| covered.get(parent))
        .is_some_and(|members| members.contains(&pid))
}

/// The thread count of a process, from `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
fn process_threads(pid: u32) -> Option<u64> {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()?
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(17)?
        .parse()
        .ok()
}

#[cfg(target_os = "linux")]
fn leaf_reading(
    leaf: &std::path::Path,
    leader: u32,
    family: &BTreeSet<u32>,
    cpus: u64,
    tick_ns: u64,
) -> Option<(CpuReading, BTreeSet<u32>)> {
    leaf_reading_with(
        |file| std::fs::read_to_string(leaf.join(file)).ok(),
        leader,
        family,
        cpus,
        tick_ns,
        process_threads,
    )
}

/// Whether the process a pidfd denotes still runs: the pidfd turns readable at
/// exit. An error is no answer.
#[cfg(target_os = "linux")]
fn pidfd_alive(pidfd: &std::os::fd::OwnedFd) -> Option<bool> {
    use std::os::fd::AsRawFd;
    let mut poll = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd and a zero timeout.
    match unsafe { libc::poll(&mut poll, 1, 0) } {
        0 => Some(true),
        1 if poll.revents & libc::POLLIN != 0 => Some(false),
        _ => None,
    }
}

/// A pidfd that denotes the verified process, not a later one with its PID: it
/// is opened first, then the PID's boot and start identity is checked, and the
/// process still runs.
#[cfg(target_os = "linux")]
pub(super) fn verified_pidfd(
    identity: &crate::execution_model::ProcessIdentity,
) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};
    // SAFETY: pidfd_open only obtains a handle; nothing is signalled.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid, 0u32) };
    if fd < 0 {
        return None;
    }
    // SAFETY: a successful syscall returns a newly owned descriptor.
    let pidfd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let same = crate::supervision::process_identity(
        identity.pid,
        &identity.assignment_id,
        identity.generation,
    )
    .ok()
    .as_ref()
        == Some(identity);
    (same && pidfd_alive(&pidfd)?).then_some(pidfd)
}

/// The whole thread group's CPU time, exited threads included, from the
/// process CPU clock. The clock is keyed by PID number, so it is read only while
/// a pidfd opened first, and checked against the verified identity, shows that
/// process alive before and after the read.
#[cfg(target_os = "linux")]
pub(super) fn clock_reading(
    identity: &crate::execution_model::ProcessIdentity,
    cpus: u64,
    tick_ns: u64,
) -> Option<CpuReading> {
    let pidfd = verified_pidfd(identity)?;
    let threads_before = process_threads(identity.pid)?;
    let mut clock: libc::clockid_t = 0;
    // SAFETY: valid output pointer; the call only names the clock.
    if unsafe { libc::clock_getcpuclockid(identity.pid as libc::pid_t, &mut clock) } != 0 {
        return None;
    }
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid output pointer.
    if unsafe { libc::clock_gettime(clock, &mut time) } != 0 {
        return None;
    }
    let threads_after = process_threads(identity.pid)?;
    // Still alive, so no other process held the PID during the read.
    if !pidfd_alive(&pidfd)? {
        return None;
    }
    let ns = u64::try_from(time.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(time.tv_nsec).ok()?)?;
    Some(CpuReading {
        source: CpuSource::Clock,
        ns,
        quantization_ns: clock_quantization_ns(threads_before.max(threads_after), cpus, tick_ns),
    })
}

pub(super) fn cpu_counter_quantization_ms() -> Option<u64> {
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
