//! Own work's usage: live attribution of registered processes beside the host
//! snapshot, with CPU usage bounds.
use super::*;

/// CedeGrid's own CPU, the agent's process clock and its supervisors', per interval and over
/// the last `CPU_WINDOW_INTERVALS` intervals (FR-069). A supervisor first seen is a baseline
/// and one gone adds nothing, so both values are lower bounds.
#[derive(Debug, Default)]
pub struct CedegridCpu {
    previous: Option<(Instant, u64)>,
    supervisors: BTreeMap<String, u64>,
    window: std::collections::VecDeque<(u64, u64)>,
}

impl CedegridCpu {
    /// The agent's clock and each supervisor's, keyed by assignment, at `at`: millicores over
    /// this interval and over the window, or `None` for the first sample.
    pub fn sample(
        &mut self,
        at: Instant,
        agent_ns: u64,
        supervisors: &[(String, u64)],
    ) -> Option<(u64, u64)> {
        let previous = self.previous.replace((at, agent_ns));
        let mut used = previous.map_or(0, |(_, ns)| agent_ns.saturating_sub(ns));
        for (key, ns) in supervisors {
            if let Some(before) = self.supervisors.get(key) {
                used = used.saturating_add(ns.saturating_sub(*before));
            }
        }
        self.supervisors = supervisors.iter().cloned().collect();
        let (before, _) = previous?;
        let wall = u64::try_from(at.saturating_duration_since(before).as_nanos()).ok()?;
        if wall == 0 {
            return None;
        }
        self.window.push_back((used, wall));
        while self.window.len() > crate::kernel::CPU_WINDOW_INTERVALS {
            self.window.pop_front();
        }
        let (window_used, window_wall) = self
            .window
            .iter()
            .fold((0u64, 0u64), |(u, w), (used, wall)| {
                (u.saturating_add(*used), w.saturating_add(*wall))
            });
        let millicores = |used: u64, wall: u64| {
            u64::try_from(u128::from(used) * 1000 / u128::from(wall.max(1))).unwrap_or(u64::MAX)
        };
        Some((millicores(used, wall), millicores(window_used, window_wall)))
    }
}

/// The calling process's CPU time, all threads, from its process clock.
#[cfg(target_os = "linux")]
pub fn own_process_cpu_ns() -> Option<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid output pointer.
    if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) } != 0 {
        return None;
    }
    u64::try_from(time.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(time.tv_nsec).ok()?)
}

/// A supervisor's CPU time, all threads, from its process clock. An owned supervisor is the
/// agent's unreaped child, so its PID cannot name another process; an attached one is read
/// only while a pidfd and its verified identity show it alive.
#[cfg(target_os = "linux")]
pub fn supervisor_cpu_ns(
    pid: u32,
    attached: Option<&crate::execution_model::ProcessIdentity>,
) -> Option<u64> {
    if let Some(identity) = attached {
        return clock_reading(identity, 1, 1).map(|reading| reading.ns);
    }
    let mut clock: libc::clockid_t = 0;
    // SAFETY: valid output pointer; the call only names the clock.
    if unsafe { libc::clock_getcpuclockid(pid as libc::pid_t, &mut clock) } != 0 {
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
    u64::try_from(time.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(time.tv_nsec).ok()?)
}

/// Pidfds of own processes whose identity was checked through `/proc` once: later intervals
/// re-verify through the handle, which denotes that process and no later one with its PID,
/// instead of re-reading the identity (SC-014). An exited process or another identity goes
/// through the full check again. Other platforms re-read the identity each time.
#[derive(Default)]
pub(super) struct HeldHandles {
    #[cfg(target_os = "linux")]
    handles: BTreeMap<
        u32,
        (
            crate::execution_model::ProcessIdentity,
            std::os::fd::OwnedFd,
        ),
    >,
}

impl HeldHandles {
    /// Whether `identity` names a live own process: through its held pidfd, else a full check
    /// that keeps the pidfd it opens.
    pub(super) fn verify(&mut self, identity: &crate::execution_model::ProcessIdentity) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.verify_with(identity, &verified_pidfd)
        }
        #[cfg(not(target_os = "linux"))]
        {
            crate::supervision::process_identity(
                identity.pid,
                &identity.assignment_id,
                identity.generation,
            )
            .ok()
            .as_ref()
                == Some(identity)
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn verify_with(
        &mut self,
        identity: &crate::execution_model::ProcessIdentity,
        open: &dyn Fn(&crate::execution_model::ProcessIdentity) -> Option<std::os::fd::OwnedFd>,
    ) -> bool {
        match self.handles.get(&identity.pid) {
            Some((held, pidfd)) if held == identity => {
                if pidfd_alive(pidfd) == Some(true) {
                    return true;
                }
                self.handles.remove(&identity.pid);
            }
            Some((_, pidfd)) if pidfd_alive(pidfd) != Some(true) => {
                self.handles.remove(&identity.pid);
            }
            _ => {}
        }
        if self.handles.contains_key(&identity.pid) {
            // A live process with another identity holds this PID.
            return false;
        }
        match open(identity) {
            Some(pidfd) => {
                self.handles.insert(identity.pid, (identity.clone(), pidfd));
                true
            }
            None => false,
        }
    }

    /// Whether the held handle for `identity` still shows it alive; no `/proc` read.
    pub(super) fn alive(&self, identity: &crate::execution_model::ProcessIdentity) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.handles
                .get(&identity.pid)
                .is_some_and(|(held, pidfd)| held == identity && pidfd_alive(pidfd) == Some(true))
        }
        #[cfg(not(target_os = "linux"))]
        {
            crate::supervision::process_identity(
                identity.pid,
                &identity.assignment_id,
                identity.generation,
            )
            .ok()
            .as_ref()
                == Some(identity)
        }
    }

    /// The process clock read under the held pidfd, which shows the process alive before and
    /// after the read.
    #[cfg(target_os = "linux")]
    pub(super) fn clock_reading(
        &self,
        identity: &crate::execution_model::ProcessIdentity,
        cpus: u64,
        tick_ns: u64,
    ) -> Option<CpuReading> {
        let (held, pidfd) = self.handles.get(&identity.pid)?;
        if held != identity || pidfd_alive(pidfd) != Some(true) {
            return None;
        }
        clock_reading_under(pidfd, identity.pid, cpus, tick_ns)
    }

    /// Drops the handles of processes no longer in `keep`.
    pub(super) fn retain(&mut self, keep: &BTreeSet<u32>) {
        #[cfg(target_os = "linux")]
        self.handles.retain(|pid, _| keep.contains(pid));
        #[cfg(not(target_os = "linux"))]
        let _ = keep;
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }
}

/// Matching-scope live attribution for explicitly registered single-process jobs.
/// The registry is checked on both sides of sampling to fence PID reuse. Process
/// RSS is not subtracted from system memory: shared/file-backed pages would invent
/// capacity. Linux anonymous proportional memory is the conservative subtraction.
pub struct ManagedCollector {
    pub collector: Collector,
    pub(super) processes: System,
    pub(super) scoped_cpu: bool,
    /// Each allocation's baseline: its identity, the reading, when it was read, and whether it
    /// was taken at launch, so the next delta covers only part of an aggregate interval.
    pub(super) previous: BTreeMap<String, Baseline>,
    /// Each allocation's baselines for the window, oldest first: one per aggregate sample.
    window_baselines: BTreeMap<String, std::collections::VecDeque<Baseline>>,
    /// One scheduler tick, the precise sources' read lag per running thread.
    tick_ns: u64,
    /// Whether the leaf and clock sources apply; a host with `nohz_full` CPUs
    /// keeps the `/proc` counters.
    precise_cpu: bool,
    /// The cgroup backend's delegated root, whose `cpu.stat` gives own CPU in Full.
    delegated_root: Option<std::path::PathBuf>,
    /// Pidfds of own processes verified once, for each interval's re-verification.
    pub(super) handles: HeldHandles,
    /// The monitor interval, for the PSS period's budget.
    interval: Duration,
    /// Each Tracked family's PSS schedule and its members this interval, by run (FR-085).
    family_pss: BTreeMap<String, FamilyPss>,
    /// Diagnostic only: a single-process run's PSS period, from
    /// `CEDEGRID_DIAGNOSTIC_SINGLE_PSS_EVERY`; `None` reads it every interval.
    single_pss_every: Option<u32>,
    /// Diagnostic only: per-task reads skipped, from `CEDEGRID_DIAGNOSTIC_TASK_READS`.
    task_reads: TaskReadSkips,
    /// The first RSS read of each task, carried when the RSS read is skipped.
    first_rss: std::cell::RefCell<BTreeMap<u32, u64>>,
    family_members: BTreeMap<String, BTreeSet<Member>>,
    /// The runs whose RAM this interval is an estimate between passes.
    ram_estimated: BTreeSet<String>,
}

/// A CPU reading taken as the start of an allocation's next delta.
#[derive(Clone)]
pub(super) struct Baseline {
    pub(super) identity: crate::execution_model::ProcessIdentity,
    pub(super) reading: CpuReading,
    pub(super) at: Instant,
    /// Taken at launch rather than right after an aggregate sample.
    pub(super) first: bool,
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
            window_baselines: BTreeMap::new(),
            handles: HeldHandles::default(),
            interval: Duration::from_millis(config.monitor.interval_ms),
            family_pss: BTreeMap::new(),
            single_pss_every: diagnostic_single_pss_every(
                std::env::var("CEDEGRID_DIAGNOSTIC_SINGLE_PSS_EVERY")
                    .ok()
                    .as_deref(),
            ),
            task_reads: TaskReadSkips::parse(
                std::env::var("CEDEGRID_DIAGNOSTIC_TASK_READS")
                    .ok()
                    .as_deref(),
            ),
            first_rss: std::cell::RefCell::new(BTreeMap::new()),
            family_members: BTreeMap::new(),
            ram_estimated: BTreeSet::new(),
            delegated_root: config
                .cgroup
                .enabled
                .then(|| config.cgroup.delegated_root.clone())
                .flatten(),
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
        cpus: u64,
    ) -> (Option<CpuReading>, Option<BTreeSet<u32>>) {
        let Some(identity) = record.identity.as_ref() else {
            return (None, None);
        };
        #[cfg(target_os = "linux")]
        if self.precise_cpu && !self.task_reads.clock {
            if let Some((reading, members)) = leaf_path(record)
                .and_then(|leaf| leaf_reading(leaf, identity.pid, family, cpus, self.tick_ns))
            {
                return (Some(reading), Some(members));
            }
            if let Some(reading) = self.handles.clock_reading(identity, cpus, self.tick_ns) {
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
        let reading = self.stat_cpu_ms(identity.pid).and_then(|ms| {
            Some(CpuReading {
                source: CpuSource::Stat,
                ns: ms.checked_mul(1_000_000)?,
                quantization_ns: stat_quantization_ns()?,
                threads: 0,
            })
        });
        (reading, None)
    }
    /// A process's CPU time in milliseconds from its `/proc` tick counters, the last source.
    pub(super) fn stat_cpu_ms(&self, pid: u32) -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            let line = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let ticks = super::processes::parse_stat(&line)?.cpu_ticks;
            // SAFETY: sysconf reads a constant.
            let hz = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?;
            ticks.checked_mul(1000)?.checked_div(hz)
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.processes
                .process(sysinfo::Pid::from_u32(pid))
                .map(|process| process.accumulated_cpu_time())
        }
    }
    /// A process's resident memory in bytes, as `/proc/<pid>/statm` counts it.
    pub(super) fn resident_bytes(&self, pid: u32) -> Option<u64> {
        // Diagnostic: after the first read, carry it.
        if self.task_reads.rss
            && let Some(rss) = self.first_rss.borrow().get(&pid)
        {
            return Some(*rss);
        }
        let rss = self.read_resident_bytes(pid)?;
        if self.task_reads.rss {
            self.first_rss.borrow_mut().insert(pid, rss);
        }
        Some(rss)
    }
    fn read_resident_bytes(&self, pid: u32) -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
            let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
            // SAFETY: sysconf reads a constant.
            let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
            pages.checked_mul(page)
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.processes
                .process(sysinfo::Pid::from_u32(pid))
                .map(|process| process.memory())
        }
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
        // Linux reads the counters it needs from `/proc` directly.
        #[cfg(not(target_os = "linux"))]
        self.processes
            .refresh_processes(sysinfo::ProcessesToUpdate::Some(&ids), true);
        #[cfg(target_os = "linux")]
        let _ = ids;
        let cpus = scope_cpus();
        for record in records {
            if let Some(identity) = &record.identity
                && self.handles.verify(identity)
                && !self.previous.contains_key(&record.assignment_id)
                && let (Some(reading), _) =
                    self.read_cpu(record, &BTreeSet::from([identity.pid]), cpus)
            {
                let baseline = Baseline {
                    identity: identity.clone(),
                    reading,
                    at: refresh_started,
                    first: true,
                };
                self.window_baselines
                    .insert(record.assignment_id.clone(), [baseline.clone()].into());
                self.previous.insert(record.assignment_id.clone(), baseline);
            }
        }
    }
    /// Each member's RAM this interval for Tracked families of more than their leader: a timed
    /// PSS pass every K intervals, else each member's last PSS with the family's RSS growth on
    /// the leader (FR-085, R45). A member missing from the result is read as before.
    fn family_ram(
        &mut self,
        active: &[&crate::execution_model::ExecutionRecord],
        parents: &BTreeMap<String, String>,
        verified: &BTreeSet<u32>,
    ) -> BTreeMap<u32, u64> {
        let mut families: BTreeMap<String, BTreeSet<Member>> = BTreeMap::new();
        let mut leaders: BTreeMap<String, u32> = BTreeMap::new();
        for record in active {
            let Some(id) = record
                .identity
                .as_ref()
                .filter(|id| verified.contains(&id.pid))
            else {
                continue;
            };
            let run = parents
                .get(&record.assignment_id)
                .unwrap_or(&record.assignment_id)
                .clone();
            if !parents.contains_key(&record.assignment_id) {
                if leaf_path(record).is_some() {
                    continue;
                }
                leaders.insert(run.clone(), id.pid);
            }
            families
                .entry(run)
                .or_default()
                .insert((id.pid, id.start_time));
        }
        let single = self.single_pss_every;
        families.retain(|run, members| {
            leaders.contains_key(run)
                && (family_pss_applies(members.len(), false)
                    || single_pss_applies(members.len(), false, single))
        });
        self.family_pss.retain(|run, _| families.contains_key(run));
        self.ram_estimated.clear();
        let scope_cpu = self
            .interval
            .saturating_mul(u32::try_from(scope_cpus()).unwrap_or(1));
        let mut ram = BTreeMap::new();
        for (run, members) in &families {
            let rss: Option<BTreeMap<Member, u64>> = members
                .iter()
                .map(|member| Some((*member, self.resident_bytes(member.0)?)))
                .collect();
            let Some(rss) = rss else {
                continue;
            };
            if self.family_pss.entry(run.clone()).or_default().due(members) {
                let started = Instant::now();
                let read: Option<BTreeMap<Member, (u64, u64)>> = members
                    .iter()
                    .map(|member| Some((*member, (own_memory_mib(member.0)?, rss[member]))))
                    .collect();
                let Some(read) = read else {
                    continue;
                };
                let schedule = self.family_pss.get_mut(run).expect("entry above");
                schedule.passed(read.clone(), started.elapsed(), scope_cpu);
                if let Some(every) = single.filter(|_| members.len() == 1) {
                    schedule.period = every;
                }
                ram.extend(read.into_iter().map(|((pid, _), (pss, _))| (pid, pss)));
            } else {
                let schedule = self.family_pss.get_mut(run).expect("entry above");
                let (held, growth) = schedule.estimate(&rss);
                ram.extend(held.into_iter().map(|((pid, _), pss)| (pid, pss)));
                if let Some(leader) = ram.get_mut(&leaders[run]) {
                    *leader = leader.saturating_add(growth);
                }
                self.ram_estimated.insert(run.clone());
            }
        }
        self.family_members = families;
        ram
    }
    /// The runs whose RAM this interval is an estimate between PSS passes (FR-085).
    pub fn ram_estimated(&self) -> &BTreeSet<String> {
        &self.ram_estimated
    }
    /// Reads these runs' family PSS now, as a pass, and returns each one's RAM in MiB; none for
    /// a run whose members could not all be read, which is then unknown.
    pub fn fresh_family_ram(&mut self, runs: &BTreeSet<String>) -> BTreeMap<String, Option<u64>> {
        let scope_cpu = self
            .interval
            .saturating_mul(u32::try_from(scope_cpus()).unwrap_or(1));
        let mut out = BTreeMap::new();
        for run in runs {
            let Some(members) = self.family_members.get(run).cloned() else {
                continue;
            };
            let started = Instant::now();
            let read: Option<BTreeMap<Member, (u64, u64)>> = members
                .iter()
                .map(|member| {
                    Some((
                        *member,
                        (own_memory_mib(member.0)?, self.resident_bytes(member.0)?),
                    ))
                })
                .collect();
            let total = read
                .as_ref()
                .map(|read| read.values().map(|(pss, _)| pss).sum());
            if let Some(read) = read {
                self.family_pss.entry(run.clone()).or_default().passed(
                    read,
                    started.elapsed(),
                    scope_cpu,
                );
            }
            out.insert(run.clone(), total);
        }
        out
    }
    /// Records for a run's verified descendants, each keyed apart and mapped to its run, so
    /// they are measured as the run's own work (FR-085).
    fn descendant_records(
        records: &[crate::execution_model::ExecutionRecord],
        descendants: &[(String, crate::execution_model::ProcessIdentity)],
    ) -> Vec<(crate::execution_model::ExecutionRecord, String)> {
        use crate::execution_model::{ExecutionPhase, ExecutionRecord};
        descendants
            .iter()
            .filter_map(|(assignment, identity)| {
                let run = records.iter().find(|r| {
                    r.assignment_id == *assignment && r.phase != ExecutionPhase::Released
                })?;
                Some((
                    ExecutionRecord {
                        task_id: run.task_id.clone(),
                        assignment_id: format!(
                            "descendant:{assignment}:{}:{}",
                            identity.pid, identity.start_time
                        ),
                        generation: run.generation,
                        class: run.class,
                        phase: ExecutionPhase::Running,
                        resources: crate::model::Resources::default(),
                        identity: Some(identity.clone()),
                        backend: "descendant".into(),
                        evidence: vec![],
                        detail: String::new(),
                        modes: Default::default(),
                    },
                    assignment.clone(),
                ))
            })
            .collect()
    }
    /// Take launch baselines for descendants that just joined, so their first interval has a
    /// delta instead of leaving their run's usage unknown.
    pub fn prime_descendants(
        &mut self,
        records: &[crate::execution_model::ExecutionRecord],
        descendants: &[(String, crate::execution_model::ProcessIdentity)],
    ) {
        let augmented: Vec<_> = Self::descendant_records(records, descendants)
            .into_iter()
            .map(|(record, _)| record)
            .collect();
        self.prime(&augmented);
    }
    /// Aggregate explicitly registered family members into exactly one allocation.
    /// A pending/uncertain child makes family usage unknown, preserving reservation.
    pub fn sample_with_children(
        &mut self,
        records: &[crate::execution_model::ExecutionRecord],
        children: &[crate::managed_children::ManagedChildRecord],
    ) -> Result<(Snapshot, Vec<crate::model::Allocation>)> {
        self.sample_with_descendants(records, children, &[])
    }
    /// As `sample_with_children`, with each run's verified descendants counted as its own
    /// work: CPU summed over members, memory by their summed PSS, GPU per process (FR-085).
    pub fn sample_with_descendants(
        &mut self,
        records: &[crate::execution_model::ExecutionRecord],
        children: &[crate::managed_children::ManagedChildRecord],
        descendants: &[(String, crate::execution_model::ProcessIdentity)],
    ) -> Result<(Snapshot, Vec<crate::model::Allocation>)> {
        use crate::{
            execution_model::{ExecutionPhase, ExecutionRecord},
            managed_children::ManagedChildPhase,
            model::Resources,
        };
        let mut augmented = records.to_vec();
        let mut parents = BTreeMap::new();
        for (record, run) in Self::descendant_records(records, descendants) {
            parents.insert(record.assignment_id.clone(), run);
            augmented.push(record);
        }
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
        for mut child in child_allocations {
            // A descendant that exited since its last sample, or joined too late for a
            // baseline, adds nothing: own usage stays a lower bound and others read no less.
            if child.id.starts_with("descendant:") && child.observed.is_none() {
                child.observed = Some(Resources::default());
                child.observed_upper = None;
            }
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
        self.window_baselines.retain(|assignment, _| {
            active
                .iter()
                .any(|record| record.assignment_id == *assignment)
        });
        // Each own process is checked through `/proc` once, then through its held pidfd.
        let identities: Vec<_> = active.iter().filter_map(|r| r.identity.as_ref()).collect();
        self.handles
            .retain(&identities.iter().map(|id| id.pid).collect::<BTreeSet<_>>());
        let verified: BTreeSet<_> = identities
            .into_iter()
            .filter(|id| self.handles.verify(id))
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
        let family_ram = self.family_ram(&active, parents, &verified);
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
        #[cfg(not(target_os = "linux"))]
        self.processes.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&pids),
            true,
            sysinfo::ProcessRefreshKind::nothing().with_cpu(),
        );
        let cpus = scope_cpus();
        type EndRead = (Option<CpuReading>, Option<BTreeSet<u32>>, Instant);
        let end_reads: BTreeMap<String, EndRead> = active
            .iter()
            .filter_map(|record| {
                record
                    .identity
                    .as_ref()
                    .filter(|id| verified.contains(&id.pid))?;
                let family = families.get(&owner(&record.assignment_id))?;
                let at = Instant::now();
                let (reading, members) = self.read_cpu(record, family, cpus);
                Some((record.assignment_id.clone(), (reading, members, at)))
            })
            .collect();
        let mut snapshot = self.collector.sample()?;
        let cpu_window = if self.scoped_cpu && cfg!(target_os = "linux") {
            self.collector.kernel.cpu_accounting_window()
        } else {
            self.collector.cpu_window
        };
        // The busy-time window, when the scoped collector holds one (FR-073).
        let busy_window = (self.scoped_cpu && cfg!(target_os = "linux"))
            .then(|| self.collector.kernel.cpu_window_accounting())
            .flatten()
            .map(|(window, _)| window);
        snapshot.capabilities.insert("managed_memory_attribution".into(),capability(
            if cfg!(target_os="linux"){CapabilityStatus::Available}else{CapabilityStatus::Unsupported},
            "Managed RAM subtraction uses anonymous proportional physical memory on Linux, excluding reclaimable file mappings. Other platforms subtract a conservative zero lower bound and retain declared reservations; this is not a measurement of zero workload RAM and not hard memory enforcement."));
        snapshot.capabilities.insert("managed_cpu_attribution".into(),capability(
            if cpu_counter_quantization_ms().is_some(){CapabilityStatus::Available}else{CapabilityStatus::Unsupported},
            "Managed CPU is a conservative lower bound from the allocation's verified cgroup leaf (cpu.stat usage_usec, only while its members are own verified processes), else the process CPU clock read under a pidfd, else /proc tick counters. Process deltas lie inside aggregate read boundaries; a process that ran the whole interval is divided by its own read interval, and a new worker by the whole aggregate interval. Each source's resolution plus one scheduler tick per thread that can be running (the tick counters: two ticks) is subtracted per allocation, and one tick per scope CPU from the sum; reservations remain charged in full. A source change or missing attribution is unknown; this is not hard CPU enforcement."));
        #[cfg(not(target_os = "linux"))]
        self.processes
            .refresh_processes(sysinfo::ProcessesToUpdate::Some(&pids), true);
        let mut allocations = Vec::new();
        let mut cpu_diagnostics = Vec::new();
        let mut parts = Vec::new();
        let mut window_parts = Vec::new();
        let mut window_complete = busy_window.is_some();
        // A member the allocation's leaf already counts adds no CPU of its own.
        let covered: BTreeMap<&String, &BTreeSet<u32>> = end_reads
            .iter()
            .filter_map(|(assignment, (_, members, _))| Some((assignment, members.as_ref()?)))
            .collect();
        for record in active {
            let old_cpu = self.previous.get(&record.assignment_id).cloned();
            let end = end_reads.get(&record.assignment_id).and_then(|end| end.0);
            let end_at = end_reads.get(&record.assignment_id).map(|end| end.2);
            let leaf_counted = record.identity.as_ref().is_some_and(|id| {
                counted_by_leaf(parents, &covered, &record.assignment_id, id.pid)
            });
            let baseline = record
                .identity
                .as_ref()
                .filter(|id| verified.contains(&id.pid))
                .and_then(|id| {
                    let family = families.get(&owner(&record.assignment_id))?;
                    let at = Instant::now();
                    let reading = self.read_cpu(record, family, cpus).0?;
                    Some(Baseline {
                        identity: id.clone(),
                        reading,
                        at,
                        first: false,
                    })
                });
            let mut window_cpu = None;
            // Each check that can leave the allocation unmeasured names itself for the record
            // (FR-018).
            let bounds = (|| -> std::result::Result<(Resources, Resources), &'static str> {
                let id = record.identity.as_ref().ok_or("no_identity")?;
                if !verified.contains(&id.pid) {
                    return Err("not_verified");
                }
                if !self.handles.alive(id) {
                    return Err("not_alive");
                }
                // Diagnostic: no per-task usage at all, or no affinity recheck.
                if self.task_reads.all {
                    return Err("diagnostic_skipped");
                }
                if !self.task_reads.affinity
                    && !process_scope_matches(id.pid, &snapshot, self.scoped_cpu)
                {
                    return Err("scope_changed");
                }
                let resident = self.resident_bytes(id.pid).ok_or("resident_unreadable")?;
                let Some(baseline) = baseline.clone() else {
                    self.previous.remove(&record.assignment_id);
                    self.window_baselines.remove(&record.assignment_id);
                    return Err("no_baseline");
                };
                let old = self
                    .previous
                    .insert(record.assignment_id.clone(), baseline)
                    .ok_or("first_interval")?;
                if old.identity != *id {
                    self.window_baselines.remove(&record.assignment_id);
                    return Err("identity_changed");
                }
                let end = end.ok_or("no_end_reading")?;
                let end_at = end_at.ok_or("no_end_reading")?;
                let (delta, quantization) =
                    cpu_delta(old.reading, end).ok_or("counter_went_back")?;
                let (cpu_millicores, divisor) = own_cpu_lower_bound(
                    delta,
                    old.at,
                    end_at,
                    cpu_window.ok_or("no_cpu_window")?,
                    quantization,
                    old.first,
                )
                .ok_or("outside_cpu_window")?;
                // Own RAM: the leaf's anonymous and shared memory where the leaf was read with only
                // own members, children it holds included, else the process's anonymous and shared
                // pages; a child the leaf already counts adds none.
                let leaf_read = end_reads
                    .get(&record.assignment_id)
                    .is_some_and(|end| end.1.is_some());
                let ram_mib = if leaf_counted {
                    0
                } else {
                    match leaf_path(record).filter(|_| leaf_read) {
                        Some(leaf) => leaf_memory_mib(leaf).ok_or("ram_unreadable")?,
                        None => match family_ram.get(&id.pid) {
                            Some(ram) => *ram,
                            None => own_memory_mib(id.pid).ok_or("ram_unreadable")?,
                        },
                    }
                };
                let gpu_memory_mib = self.collector.gpu_usage(id.pid, &record.resources);
                // Upper bounds: the rate over the process's own interval plus
                // quantization, and resident memory including shared pages.
                let upper_cpu = end_at
                    .checked_duration_since(old.at)
                    .and_then(|interval| cpu_usage_upper_bound(delta, interval, quantization))
                    .ok_or("outside_cpu_window")?
                    .max(cpu_millicores);
                let precise = end
                    .resolution_ns()
                    .map(|resolution| (resolution, old.reading.threads.max(end.threads)));
                // Over the window: from the first baseline taken after the window began, divided
                // by the process's own read interval when an older baseline shows it ran the whole
                // window, else by the whole window (FR-073).
                if let Some(window) = busy_window
                    && let Some(history) = self.window_baselines.get(&record.assignment_id)
                    && let Some(start) = history.iter().position(|b| b.at >= window.started)
                    && history[start].identity == *id
                    && let Some((window_delta, window_quantization)) =
                        cpu_delta(history[start].reading, end)
                {
                    let interval = if start > 0 && !history[start].first {
                        end_at.checked_duration_since(history[start].at)
                    } else {
                        Some(window.duration())
                    };
                    if let Some(interval) = interval {
                        window_cpu =
                            cpu_usage_lower_bound(window_delta, interval, window_quantization);
                        if !leaf_counted {
                            window_parts.push(CpuPart {
                                delta_ns: window_delta,
                                quantization_ns: window_quantization,
                                precise,
                                interval,
                            });
                        }
                    }
                }
                let (cpu_millicores, upper_cpu) = if leaf_counted {
                    window_cpu = window_cpu.map(|_| 0);
                    (0, 0)
                } else {
                    parts.push(CpuPart {
                        delta_ns: delta,
                        quantization_ns: quantization,
                        precise,
                        interval: divisor,
                    });
                    (cpu_millicores, upper_cpu)
                };
                let upper = Resources {
                    cpu_millicores: upper_cpu,
                    ram_mib: resident.div_ceil(MIB).max(ram_mib),
                    gpu_memory_mib: gpu_memory_mib.clone(),
                };
                Ok((
                    Resources {
                        cpu_millicores,
                        ram_mib,
                        gpu_memory_mib,
                    },
                    upper,
                ))
            })();
            let usage_unknown = bounds.as_ref().err().map(|check| (*check).to_owned());
            let (observed, observed_upper) = bounds.ok().unzip();
            if let Some(baseline) = baseline {
                let history = self
                    .window_baselines
                    .entry(record.assignment_id.clone())
                    .or_default();
                history.push_back(baseline);
                while history.len() > crate::kernel::CPU_WINDOW_INTERVALS + 2 {
                    history.pop_front();
                }
            }
            if observed.is_some() && window_cpu.is_none() {
                window_complete = false;
            }
            // Bounded read-only evidence makes a rejected subtraction
            // diagnosable even if the workload drains before the next heartbeat.
            if cpu_diagnostics.len() < 64
                && let Some(id) = record.identity.as_ref()
            {
                cpu_diagnostics.push(serde_json::json!({
                    "assignment_id":record.assignment_id,"pid":id.pid,
                    "cpu_source":end.map(|end|end.source.name()),
                    "leaf_counted":leaf_counted,
                    "previous_cpu_ns":old_cpu.as_ref().map(|old|old.reading.ns),
                    "end_cpu_ns":end.map(|end|end.ns),
                    "next_baseline_cpu_ns":self.previous.get(&record.assignment_id).map(|next|next.reading.ns),
                    "stat_cpu_ms":(!self.precise_cpu).then(|| self.stat_cpu_ms(id.pid)).flatten(),
                    "process_baseline_after_aggregate_start_ns":old_cpu.as_ref().and_then(|old|cpu_window.and_then(|window|old.at.checked_duration_since(window.started))).map(|duration|duration.as_nanos()),
                    "process_read_before_aggregate_end_ns":cpu_window.zip(end_at).and_then(|(window,at)|window.ended.checked_duration_since(at)).map(|duration|duration.as_nanos()),
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
                observed_window_cpu: if pending { None } else { window_cpu },
                usage_unknown: if pending { None } else { usage_unknown },
                modes: record.modes.clone(),
            });
        }
        if cpu_window.is_some() && !parts.is_empty() {
            snapshot.managed_cpu_lower_millicores = summed_lower_bound(&parts, cpus, self.tick_ns);
        }
        // Full: every own allocation is a leaf directly under the delegated root, read through
        // its verified leaf; then the root's one read, beside the host counters, is own CPU.
        let mut full = false;
        if let (Some(root), Some(window), true) =
            (&self.delegated_root, cpu_window, self.scoped_cpu)
        {
            let mut leaves = BTreeSet::new();
            let mut threads = 0u64;
            let mut every_leaf = true;
            for record in records
                .iter()
                .filter(|r| r.phase != ExecutionPhase::Released)
            {
                match leaf_path(record).filter(|leaf| leaf.parent() == Some(root.as_path())) {
                    Some(leaf) => {
                        leaves.insert(leaf.file_name().unwrap_or_default().to_owned());
                        match end_reads.get(&record.assignment_id) {
                            Some((Some(reading), Some(_), _)) => threads += reading.threads,
                            _ if matches!(
                                record.phase,
                                ExecutionPhase::Reserved | ExecutionPhase::Prepared
                            ) => {}
                            _ => every_leaf = false,
                        }
                    }
                    None => every_leaf = false,
                }
            }
            let children: BTreeSet<_> = std::fs::read_dir(root)
                .map(|entries| {
                    entries
                        .filter_map(|entry| entry.ok())
                        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                        .map(|entry| entry.file_name())
                        .collect()
                })
                .unwrap_or_default();
            let procs = std::fs::read_to_string(root.join("cgroup.procs")).ok();
            if every_leaf
                && let Some(procs) = procs
                && full_parent_applies(&children, &procs, &leaves)
            {
                let (interval, over_window) = self.collector.kernel.delegated_cpu_usage();
                if let Some(delta) = interval {
                    snapshot.managed_cpu_lower_millicores =
                        parent_lower_bound(delta, threads, cpus, self.tick_ns, window.duration());
                }
                if let (Some(delta), Some(span)) = (over_window, busy_window) {
                    snapshot.managed_cpu_window_lower_millicores =
                        parent_lower_bound(delta, threads, cpus, self.tick_ns, span.duration());
                }
                full = true;
                if let Some(evidence) = snapshot.capabilities.get_mut("managed_cpu_attribution") {
                    evidence.detail.push_str(" Own CPU in total: the delegated root's cpu.stat, read beside the host counters, while its children are exactly the own leaves.");
                }
            }
        }
        // Own CPU over the window, only when every measured allocation has its window value.
        if !full && window_complete && window_parts.len() == parts.len() {
            snapshot.managed_cpu_window_lower_millicores = if window_parts.is_empty() {
                Some(0)
            } else {
                summed_lower_bound(&window_parts, cpus, self.tick_ns)
            };
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

/// One allocation's CPU lower bound and the interval it is divided by. A process whose
/// baseline was read right after the previous aggregate sample ran that whole interval, so its
/// delta is divided by its own read interval; one whose baseline was taken at launch keeps the
/// aggregate interval, so a partial first delta is never scaled to a full rate. A baseline
/// before the aggregate interval, or an end read after it, stays unknown (FR-073).
pub(super) fn own_cpu_lower_bound(
    delta_ns: u64,
    baseline_at: Instant,
    end_at: Instant,
    aggregate: crate::kernel::CpuAccountingWindow,
    quantization_ns: u64,
    first: bool,
) -> Option<(u64, Duration)> {
    if first {
        let divisor = aggregate.duration();
        return aligned_cpu_usage_lower_bound(
            delta_ns,
            baseline_at,
            end_at,
            aggregate,
            quantization_ns,
        )
        .map(|millicores| (millicores, divisor));
    }
    if baseline_at < aggregate.started || end_at > aggregate.ended || end_at <= baseline_at {
        return None;
    }
    let divisor = end_at - baseline_at;
    cpu_usage_lower_bound(delta_ns, divisor, quantization_ns)
        .map(|millicores| (millicores, divisor))
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
    /// Threads read with a precise source, for the sum over own work.
    pub(super) threads: u64,
}

impl CpuReading {
    /// The source's resolution, when it is a precise one.
    fn resolution_ns(self) -> Option<u64> {
        match self.source {
            CpuSource::Leaf => Some(1_000),
            CpuSource::Clock => Some(1),
            CpuSource::Stat => None,
        }
    }
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

/// One allocation's CPU delta, for the sum over own work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CpuPart {
    pub(super) delta_ns: u64,
    /// The allocation's own bound, as in its evidence.
    pub(super) quantization_ns: u64,
    /// For the leaf and the clock: the resolution and the threads read.
    pub(super) precise: Option<(u64, u64)>,
    /// What the delta is divided by: the process's own read interval, or the aggregate
    /// interval for a delta that covers only part of it.
    pub(super) interval: Duration,
}

/// Own work's CPU lower bound in millicores, summed over allocations, each divided by its own
/// interval. Pending runtime exists only on a task running at the read, so across all own
/// processes it is taken as one tick per scope CPU, over the shortest interval; runtime pending
/// at one read is folded and counted at a later one (research R23). The tick counters keep their
/// rounding in full. The result is never below the sum of the per-allocation bounds, nor above
/// the raw delta.
pub(super) fn summed_lower_bound(parts: &[CpuPart], cpus: u64, tick_ns: u64) -> Option<u64> {
    // Nano-millicores keep the sum of many per-part rates exact to a millicore.
    const SCALE: u128 = 1_000_000_000;
    let per_part = parts
        .iter()
        .map(|part| cpu_usage_lower_bound(part.delta_ns, part.interval, part.quantization_ns))
        .sum::<Option<u64>>()?;
    let shortest = parts.iter().map(|part| part.interval.as_nanos()).min()?;
    if shortest == 0 {
        return None;
    }
    let mut rate = 0u128;
    let mut threads = 0u64;
    for part in parts {
        let subtract = match part.precise {
            Some((resolution, part_threads)) => {
                threads = threads.checked_add(part_threads.max(1))?;
                resolution
            }
            None => part.quantization_ns,
        };
        rate += u128::from(part.delta_ns.saturating_sub(subtract)) * 1000 * SCALE
            / part.interval.as_nanos().max(1);
    }
    let pending = u128::from(threads.min(cpus).checked_mul(tick_ns)?) * 1000 * SCALE / shortest;
    let summed = u64::try_from(rate.saturating_sub(pending) / SCALE).ok()?;
    Some(summed.max(per_part))
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
    let ns = parse_usage_usec(&stat)?;
    let threads = before
        .iter()
        .map(|pid| threads(*pid))
        .sum::<Option<u64>>()?;
    Some((
        CpuReading {
            source: CpuSource::Leaf,
            ns,
            quantization_ns: leaf_quantization_ns(threads, cpus, tick_ns),
            threads,
        },
        before,
    ))
}

pub(super) use crate::kernel::parse_usage_usec;

/// Whether the delegated root's CPU time is own work's alone: it holds no process and its
/// children are exactly the own leaves, so no other process and none of CedeGrid's own are in it
/// (FR-073).
pub(super) fn full_parent_applies(
    children: &BTreeSet<std::ffi::OsString>,
    procs: &str,
    leaves: &BTreeSet<std::ffi::OsString>,
) -> bool {
    !leaves.is_empty() && procs.trim().is_empty() && children == leaves
}

/// Own CPU from the delegated root over `interval`: its 1 µs resolution and one tick per own
/// thread that can run, at most one per scope CPU, subtracted; one read, so no read-pass term.
pub(super) fn parent_lower_bound(
    delta_ns: u64,
    threads: u64,
    cpus: u64,
    tick_ns: u64,
    interval: Duration,
) -> Option<u64> {
    cpu_usage_lower_bound(
        delta_ns,
        interval,
        1_000 + running_threads(threads, cpus) * tick_ns,
    )
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
    clock_reading_under(&pidfd, identity.pid, cpus, tick_ns)
}

/// The process clock of `pid`, read while `pidfd`, which denotes that process, shows it alive
/// before and after the read.
#[cfg(target_os = "linux")]
fn clock_reading_under(
    pidfd: &std::os::fd::OwnedFd,
    pid: u32,
    cpus: u64,
    tick_ns: u64,
) -> Option<CpuReading> {
    if !pidfd_alive(pidfd)? {
        return None;
    }
    let threads_before = process_threads(pid)?;
    let mut clock: libc::clockid_t = 0;
    // SAFETY: valid output pointer; the call only names the clock.
    if unsafe { libc::clock_getcpuclockid(pid as libc::pid_t, &mut clock) } != 0 {
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
    let threads_after = process_threads(pid)?;
    // Still alive, so no other process held the PID during the read.
    if !pidfd_alive(pidfd)? {
        return None;
    }
    let ns = u64::try_from(time.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(time.tv_nsec).ok()?)?;
    let threads = threads_before.max(threads_after);
    Some(CpuReading {
        source: CpuSource::Clock,
        ns,
        quantization_ns: clock_quantization_ns(threads, cpus, tick_ns),
        threads,
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

/// A process's own memory from its `smaps_rollup`: its proportional share of anonymous and
/// shared memory pages (shared memory, tmpfs and `/dev/shm` mappings), in MiB rounded down so
/// the subtracted managed usage never exceeds the evidence.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn own_memory_from_rollup(text: &str) -> Option<u64> {
    let field = |name: &str| -> Option<u64> {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|value| value.split_whitespace().next())?
            .parse::<u64>()
            .ok()
    };
    // Kernels before 5.7 list no Pss_Shmem; their shared pages are unattributed.
    let kib = field("Pss_Anon:")?.checked_add(field("Pss_Shmem:").unwrap_or(0))?;
    Some(kib / 1024)
}

/// A leaf's own memory from its `memory.stat`: anonymous and shared memory (`anon`, `shmem`), in
/// MiB rounded down. Unlike `memory.current` it leaves out reclaimable page cache, which host
/// used memory leaves out too.
pub(super) fn leaf_memory_mib(leaf: &std::path::Path) -> Option<u64> {
    let stat = std::fs::read_to_string(leaf.join("memory.stat")).ok()?;
    let field = |name: &str| -> Option<u64> {
        stat.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))?
            .trim()
            .parse::<u64>()
            .ok()
    };
    Some(field("anon")?.checked_add(field("shmem")?)? / MIB)
}

#[cfg(target_os = "linux")]
fn own_memory_mib(pid: u32) -> Option<u64> {
    own_memory_from_rollup(&std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?)
}

/// A family member, by PID and start time.
pub(super) type Member = (u32, u64);

/// The longest period between a Tracked family's PSS passes, in intervals.
const PSS_PERIOD_MAX: u32 = 10;

/// The intervals between PSS passes so that a pass costing `cost` averages at most 0.5% of the
/// scope's CPU, `scope_cpu` per interval: from 1 to 10 (FR-085, R45).
pub(super) fn pss_period(cost: Duration, scope_cpu: Duration) -> u32 {
    let budget = scope_cpu.as_nanos() / 200;
    if budget == 0 {
        return PSS_PERIOD_MAX;
    }
    u32::try_from(cost.as_nanos().div_ceil(budget))
        .unwrap_or(PSS_PERIOD_MAX)
        .clamp(1, PSS_PERIOD_MAX)
}

/// Per-task reads a gate's diagnostic skips, from comma-separated words in
/// `CEDEGRID_DIAGNOSTIC_TASK_READS`: `affinity` (the per-interval affinity recheck), `rss` (statm
/// after the first read), `clock` (the precise CPU clock, so /proc ticks are read instead) and
/// `all` (no per-task usage: every allocation unmeasured). Not an operator setting: it exists to
/// find which read of a running task costs it (006). Unknown words are ignored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct TaskReadSkips {
    pub(super) affinity: bool,
    pub(super) rss: bool,
    pub(super) clock: bool,
    pub(super) all: bool,
}

impl TaskReadSkips {
    pub(super) fn parse(value: Option<&str>) -> Self {
        let mut skips = Self::default();
        for word in value.unwrap_or_default().split(',').map(str::trim) {
            match word {
                "affinity" => skips.affinity = true,
                "rss" => skips.rss = true,
                "clock" => skips.clock = true,
                "all" => skips.all = true,
                _ => {}
            }
        }
        skips
    }
}

/// A gate's diagnostic period for a single-process run's PSS: a whole number from 2 to 64 in
/// `CEDEGRID_DIAGNOSTIC_SINGLE_PSS_EVERY`, else none. Not an operator setting: it exists to
/// measure what reading a running task's `smaps_rollup` every interval costs it (006).
pub(super) fn diagnostic_single_pss_every(value: Option<&str>) -> Option<u32> {
    value
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|every| (2..=64).contains(every))
}

/// Whether the diagnostic period applies: a Tracked family of one, with the diagnostic set.
pub(super) fn single_pss_applies(members: usize, full: bool, every: Option<u32>) -> bool {
    members == 1 && !full && every.is_some()
}

/// A family's memory takes the PSS period only in Tracked with more than its leader; a family
/// of one and a Full leaf keep their readings.
pub(super) fn family_pss_applies(members: usize, full: bool) -> bool {
    members > 1 && !full
}

/// When a Tracked family's PSS is read, and what was read at the last pass.
#[derive(Debug, Clone, Default)]
pub(super) struct FamilyPss {
    /// Each member's PSS in MiB and RSS in bytes at the last pass.
    members: BTreeMap<Member, (u64, u64)>,
    /// Intervals since the last pass, and the period after it; 0 before the first.
    since: u32,
    period: u32,
}

impl FamilyPss {
    #[cfg(test)]
    pub(super) fn period(&self) -> u32 {
        self.period
    }
    /// Whether this interval reads PSS: the first, the period's last, or any change in members.
    pub(super) fn due(&self, members: &BTreeSet<Member>) -> bool {
        self.period == 0 || self.since + 1 >= self.period || !members.iter().eq(self.members.keys())
    }
    /// A pass read `readings` (PSS in MiB, RSS in bytes) at `cost`.
    pub(super) fn passed(
        &mut self,
        readings: BTreeMap<Member, (u64, u64)>,
        cost: Duration,
        scope_cpu: Duration,
    ) {
        self.members = readings;
        self.since = 0;
        self.period = pss_period(cost, scope_cpu);
    }
    /// Between passes: each member's last PSS, and the family's RSS growth since that pass in
    /// MiB, rounded up, never a fall, since new private pages raise RSS and PSS alike.
    pub(super) fn estimate(&mut self, rss: &BTreeMap<Member, u64>) -> (BTreeMap<Member, u64>, u64) {
        self.since = self.since.saturating_add(1);
        let then: u64 = self.members.values().map(|(_, rss)| rss).sum();
        let now: u64 = rss.values().sum();
        let held = self
            .members
            .iter()
            .map(|(member, (pss, _))| (*member, *pss))
            .collect();
        (held, now.saturating_sub(then).div_ceil(MIB))
    }
}

#[cfg(not(target_os = "linux"))]
fn own_memory_mib(_pid: u32) -> Option<u64> {
    // No matching attribution on this backend. Zero is a known safe lower bound
    // for subtraction, not a claim that the process uses no RAM.
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
