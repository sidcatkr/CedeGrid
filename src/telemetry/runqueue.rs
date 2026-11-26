//! Run-queue wait by user and the CPUs a waiting user's processes may use (003 FR-050, FR-056),
//! read only from `/proc`: each thread's `schedstat` run delay, as a rate between two reads, and
//! a waiting process's `Cpus_allowed_list`. A scan reads a bounded number of threads and rotates
//! through the rest over later intervals, so its cost stays bounded (research R7).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// A thread's last reading.
#[derive(Debug, Clone, Copy)]
struct Thread {
    delay_ns: u64,
    at_ns: u64,
    /// Its wait over the last two readings, in millicores.
    rate: u64,
}

#[derive(Debug)]
pub(super) struct RunQueue {
    root: PathBuf,
    /// Each thread by process ID, process start and thread ID, so a reused ID starts again.
    threads: BTreeMap<(u32, u64, u32), Thread>,
    /// Where the next scan starts, as a position in process ID order.
    cursor: usize,
}

impl RunQueue {
    pub(super) fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            threads: BTreeMap::new(),
            cursor: 0,
        }
    }

    /// One scan of `processes`, each as process ID, start and user ID, reading at most
    /// `max_threads` threads at `now_ns` (monotonic). A thread first read, or whose run delay
    /// went back, has no wait yet; a thread no longer listed is dropped. Every user with a
    /// process gets an entry; its wait is None while none of its threads could be read.
    pub(super) fn sample(
        &mut self,
        processes: &[(u32, u64, u32)],
        max_threads: usize,
        now_ns: u64,
    ) -> BTreeMap<u32, (Option<u64>, Option<BTreeSet<u32>>)> {
        let mut processes = processes.to_vec();
        processes.sort_unstable();
        processes.dedup();
        let live: BTreeSet<(u32, u64)> = processes
            .iter()
            .map(|(pid, start, _)| (*pid, *start))
            .collect();
        self.threads
            .retain(|(pid, start, _), _| live.contains(&(*pid, *start)));
        let mut unreadable: BTreeSet<u32> = BTreeSet::new();
        let count = processes.len();
        let mut read = 0usize;
        let mut visited = 0usize;
        let first = if count == 0 { 0 } else { self.cursor % count };
        while visited < count && read < max_threads {
            let (pid, start, uid) = processes[(first + visited) % count];
            visited += 1;
            let Ok(entries) = std::fs::read_dir(self.root.join(pid.to_string()).join("task"))
            else {
                unreadable.insert(uid);
                continue;
            };
            let mut listed = BTreeSet::new();
            for entry in entries.flatten() {
                let Some(tid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|n| n.parse::<u32>().ok())
                else {
                    continue;
                };
                listed.insert(tid);
                let Some(delay_ns) = std::fs::read_to_string(entry.path().join("schedstat"))
                    .ok()
                    .and_then(|text| text.split_whitespace().nth(1)?.parse::<u64>().ok())
                else {
                    continue;
                };
                read += 1;
                let key = (pid, start, tid);
                let rate = match self.threads.get(&key) {
                    Some(last) if delay_ns >= last.delay_ns && now_ns > last.at_ns => {
                        (delay_ns - last.delay_ns).saturating_mul(1000) / (now_ns - last.at_ns)
                    }
                    _ => 0,
                };
                self.threads.insert(
                    key,
                    Thread {
                        delay_ns,
                        at_ns: now_ns,
                        rate,
                    },
                );
            }
            // A thread the process no longer lists has exited.
            self.threads
                .retain(|(p, s, tid), _| (*p, *s) != (pid, start) || listed.contains(tid));
        }
        self.cursor = if count == 0 {
            0
        } else {
            (first + visited) % count
        };
        let uid_of: BTreeMap<(u32, u64), u32> = processes
            .iter()
            .map(|(pid, start, uid)| ((*pid, *start), *uid))
            .collect();
        let mut wait: BTreeMap<u32, u64> = BTreeMap::new();
        let mut waiting: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        for ((pid, start, _), thread) in &self.threads {
            let Some(uid) = uid_of.get(&(*pid, *start)) else {
                continue;
            };
            let sum = wait.entry(*uid).or_default();
            *sum = sum.saturating_add(thread.rate);
            if thread.rate > 0 {
                waiting.entry(*uid).or_default().insert(*pid);
            }
        }
        let mut result = BTreeMap::new();
        for (_, _, uid) in &processes {
            let millicores = wait
                .get(uid)
                .copied()
                .or_else(|| (!unreadable.contains(uid)).then_some(0));
            let allowed = waiting.get(uid).map_or(Some(BTreeSet::new()), |pids| {
                pids.iter().try_fold(BTreeSet::new(), |mut cpus, pid| {
                    cpus.extend(self.allowed_cpus(*pid)?);
                    Some(cpus)
                })
            });
            result.insert(*uid, (millicores, allowed));
        }
        result
    }

    /// A process's `Cpus_allowed_list`, read only.
    fn allowed_cpus(&self, pid: u32) -> Option<BTreeSet<u32>> {
        let status =
            std::fs::read_to_string(self.root.join(pid.to_string()).join("status")).ok()?;
        parse_cpu_list(
            status
                .lines()
                .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))?
                .trim(),
        )
    }
}

/// A kernel CPU list such as `0-3,8,10-11`.
pub(super) fn parse_cpu_list(text: &str) -> Option<BTreeSet<u32>> {
    let mut cpus = BTreeSet::new();
    for part in text
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        match part.split_once('-') {
            Some((first, last)) => {
                let (first, last): (u32, u32) = (first.parse().ok()?, last.parse().ok()?);
                if first > last || last - first > 65_535 {
                    return None;
                }
                cpus.extend(first..=last);
            }
            None => {
                cpus.insert(part.parse().ok()?);
            }
        }
    }
    Some(cpus)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn thread(root: &std::path::Path, pid: u32, tid: u32, delay_ns: u64) {
        let dir = root
            .join(pid.to_string())
            .join("task")
            .join(tid.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("schedstat"), format!("1000 {delay_ns} 5\n")).unwrap();
    }

    fn allowed(root: &std::path::Path, pid: u32, list: &str) {
        fs::write(
            root.join(pid.to_string()).join("status"),
            format!("Name:\tx\nCpus_allowed_list:\t{list}\n"),
        )
        .unwrap();
    }

    /// Wait is each thread's run delay over the time between two reads, summed by user; a new
    /// thread and one whose delay went back count nothing yet, and an exited one is dropped
    /// (003 FR-050).
    #[test]
    fn wait_is_summed_by_user_from_thread_deltas() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        thread(root, 10, 10, 0);
        thread(root, 10, 11, 0);
        thread(root, 20, 20, 5_000_000);
        allowed(root, 10, "0-1");
        allowed(root, 20, "2");
        let processes = [(10, 1, 1500), (20, 7, 1600)];
        let mut queue = RunQueue::new(root);
        let first = queue.sample(&processes, 100, 1_000_000_000);
        assert_eq!(first[&1500], (Some(0), Some(BTreeSet::new())));
        // A second later: 500 ms and 250 ms of wait for the first user, none for the second.
        thread(root, 10, 10, 500_000_000);
        thread(root, 10, 11, 250_000_000);
        thread(root, 20, 20, 5_000_000);
        let second = queue.sample(&processes, 100, 2_000_000_000);
        assert_eq!(second[&1500], (Some(750), Some(BTreeSet::from([0, 1]))));
        assert_eq!(second[&1600], (Some(0), Some(BTreeSet::new())));
        // Thread 11 exits and thread 10's delay goes back (a reused ID): no negative wait.
        fs::remove_dir_all(root.join("10/task/11")).unwrap();
        thread(root, 10, 10, 100);
        let third = queue.sample(&processes, 100, 3_000_000_000);
        assert_eq!(third[&1500].0, Some(0));
    }

    /// The scan reads at most the bound and rotates, so every process is read over later
    /// intervals; a process whose threads cannot be listed leaves its user's wait unknown.
    #[test]
    fn the_scan_is_bounded_and_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for pid in [10, 20, 30] {
            thread(root, pid, pid, 0);
        }
        let processes = [(10, 1, 1500), (20, 1, 1600), (30, 1, 1700), (40, 1, 1800)];
        let mut queue = RunQueue::new(root);
        let mut read = BTreeSet::new();
        for second in 1..=4u64 {
            queue.sample(&processes, 1, second * 1_000_000_000);
            read.extend(queue.threads.keys().map(|(pid, _, _)| *pid));
        }
        assert_eq!(read, BTreeSet::from([10, 20, 30]));
        // Process 40 lists no task directory: its user's wait is unknown.
        let result = queue.sample(&processes, 100, 5_000_000_000);
        assert_eq!(result[&1800].0, None);
    }

    #[test]
    fn cpu_lists_parse() {
        assert_eq!(
            parse_cpu_list("0-3,8,10-11"),
            Some(BTreeSet::from([0, 1, 2, 3, 8, 10, 11]))
        );
        assert_eq!(parse_cpu_list(""), Some(BTreeSet::new()));
        assert_eq!(parse_cpu_list("3-1"), None);
        assert_eq!(parse_cpu_list("x"), None);
    }
}
