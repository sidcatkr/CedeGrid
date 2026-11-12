//! Every process's CPU, resident memory and start time from one read of its `/proc/<pid>/stat`
//! per interval, and its user ID once, for the principals and the external memory processes
//! (research R26). Threads are not processes; CPU is computed as sysinfo computes it. Linux
//! only; other platforms keep sysinfo.
use super::*;

/// The fields a principal sample needs from one `/proc/<pid>/stat` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StatFields {
    /// utime plus stime, in clock ticks.
    pub(super) cpu_ticks: u64,
    /// Start time after boot, in clock ticks.
    pub(super) start_ticks: u64,
    /// Resident pages, as `statm` counts them.
    pub(super) rss_pages: u64,
}

/// Splits after the last `)`, so a command holding spaces or parentheses cannot shift fields.
pub(super) fn parse_stat(line: &str) -> Option<StatFields> {
    let fields: Vec<&str> = line.rsplit_once(')')?.1.split_whitespace().collect();
    // The first field after ')' is field 3 (state): utime 14, stime 15, starttime 22, rss 24.
    let field = |n: usize| fields.get(n - 3)?.parse::<u64>().ok();
    Some(StatFields {
        cpu_ticks: field(14)?.saturating_add(field(15)?),
        start_ticks: field(22)?,
        rss_pages: field(24)?,
    })
}

/// A process's CPU in millicores, as sysinfo computes it: its ticks over the global ticks per
/// CPU, at most the CPU count.
pub(super) fn cpu_millicores(delta_ticks: u64, delta_total: u64, cpus: u64) -> u64 {
    let cpus = cpus.max(1);
    let per_cpu = delta_total.max(1) as f64 / cpus as f64;
    let percent = (delta_ticks as f64 / per_cpu * 100.0).min(cpus as f64 * 100.0);
    (percent * 10.0).round() as u64
}

/// The global CPU ticks and the online CPU count from `/proc/stat`, and the boot time.
fn global_ticks() -> Option<(u64, u64, u64)> {
    parse_global(&std::fs::read_to_string("/proc/stat").ok()?)
}

/// The total CPU ticks, the online CPU count and the boot time of a `/proc/stat` text.
pub(super) fn parse_global(stat: &str) -> Option<(u64, u64, u64)> {
    let mut total = None;
    let mut cpus = 0u64;
    let mut boot = None;
    for line in stat.lines() {
        if let Some(rest) = line.strip_prefix("cpu ") {
            // User through steal: guest and guest_nice are already inside user and nice.
            total = Some(
                rest.split_whitespace()
                    .take(8)
                    .filter_map(|v| v.parse::<u64>().ok())
                    .fold(0u64, u64::saturating_add),
            );
        } else if line.starts_with("cpu") && line.as_bytes().get(3).is_some_and(u8::is_ascii_digit)
        {
            cpus += 1;
        } else if let Some(rest) = line.strip_prefix("btime ") {
            boot = rest.trim().parse().ok();
        }
    }
    Some((total?, cpus, boot?))
}

/// The real user ID from `/proc/<pid>/status`.
fn status_uid(pid: u32) -> Option<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

pub(super) struct ProcessTable {
    /// Each process's start ticks and CPU ticks at the last sample.
    previous: BTreeMap<u32, (u64, u64)>,
    /// Real user IDs, read once per PID and start time.
    uids: BTreeMap<(u32, u64), Option<u32>>,
    last_total: Option<u64>,
    /// Whether the last sample listed an MPS control daemon or server, by command name, as
    /// sysinfo's process names gave it.
    pub(super) mps: bool,
    clock_hz: u64,
    page_bytes: u64,
    uid_reads: u64,
}

impl ProcessTable {
    pub(super) fn new() -> Option<Self> {
        // SAFETY: sysconf reads constants.
        let (hz, page) = unsafe {
            (
                libc::sysconf(libc::_SC_CLK_TCK),
                libc::sysconf(libc::_SC_PAGESIZE),
            )
        };
        Some(Self {
            previous: BTreeMap::new(),
            uids: BTreeMap::new(),
            last_total: None,
            mps: false,
            clock_hz: u64::try_from(hz).ok().filter(|hz| *hz > 0)?,
            page_bytes: u64::try_from(page).ok().filter(|page| *page > 0)?,
            uid_reads: 0,
        })
    }

    /// One entry per readable `/proc/<pid>` directory. A process first seen, or one whose start
    /// time changed, has no CPU yet. `None` when the table itself cannot be read: `/proc/stat`,
    /// the `/proc` listing, or a process's `stat` for want of a descriptor. The table then
    /// keeps its state, the MPS flag included, so the next sample's CPU spans the gap (TEL-9).
    pub(super) fn sample(&mut self) -> Option<Vec<ProcessSample>> {
        let (total, cpus, boot) = global_ticks()?;
        let delta_total = self.last_total.map(|last| total.saturating_sub(last));
        let entries = std::fs::read_dir("/proc").ok()?;
        let mut next = BTreeMap::new();
        let mut samples = Vec::new();
        let mut mps = false;
        for entry in entries {
            let entry = entry.ok()?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let line = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(line) => line,
                // Out of descriptors: the rest of the table would read as gone.
                Err(error) if matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE)) => {
                    return None;
                }
                // The process exited, or its entry is hidden from this account.
                Err(_) => continue,
            };
            let Some(fields) = parse_stat(&line) else {
                continue;
            };
            mps |= line
                .split_once('(')
                .and_then(|(_, rest)| rest.rsplit_once(')'))
                .is_some_and(|(command, _)| is_mps_process(command));
            let cpu = match (self.previous.get(&pid), delta_total) {
                (Some((start, ticks)), Some(delta)) if *start == fields.start_ticks => {
                    cpu_millicores(fields.cpu_ticks.saturating_sub(*ticks), delta, cpus)
                }
                _ => 0,
            };
            next.insert(pid, (fields.start_ticks, fields.cpu_ticks));
            let key = (pid, fields.start_ticks);
            let uid = match self.uids.get(&key) {
                Some(uid) => *uid,
                None => {
                    self.uid_reads += 1;
                    let uid = status_uid(pid);
                    self.uids.insert(key, uid);
                    uid
                }
            };
            samples.push(ProcessSample {
                pid,
                uid,
                cpu_millicores: cpu,
                ram_mib: fields.rss_pages.saturating_mul(self.page_bytes) / MIB,
                start_time: boot.saturating_add(fields.start_ticks / self.clock_hz),
            });
        }
        self.uids
            .retain(|(pid, start), _| next.get(pid).is_some_and(|(s, _)| s == start));
        self.previous = next;
        self.last_total = Some(total);
        self.mps = mps;
        Some(samples)
    }

    #[cfg(test)]
    pub(super) fn uid_reads(&self) -> u64 {
        self.uid_reads
    }
}
