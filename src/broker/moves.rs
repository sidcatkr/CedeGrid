//! Moving a job between a caller's two units (FR-018, research R9, T039).
//!
//! The job's leaf is frozen, every process in it is checked against the caller's IDs and the
//! leaf, each is written into the empty leaf the agent made in the other unit and read back, and
//! the source is thawed. On any failure the job stays where it was.

use super::ModeName;
use super::oom::{HeldProcess, ProcessAccess, parse_leaf};
use super::request::{JobId, Refusal, RefusalCode};
use super::units::Caller;
use std::time::{Duration, Instant};

/// How long the broker waits for a leaf to report itself frozen.
pub const FREEZE_TIMEOUT: Duration = Duration::from_secs(1);

/// The cgroup files a move reads and writes, by cgroup path relative to the cgroup root.
pub trait CgroupFs {
    fn read(&self, cgroup: &str, file: &str) -> anyhow::Result<String>;
    fn write(&self, cgroup: &str, file: &str, value: &str) -> anyhow::Result<()>;
    /// The names of the cgroups directly below `cgroup`.
    fn children(&self, cgroup: &str) -> anyhow::Result<Vec<String>>;
}

/// The cgroup v2 file system under `root` (normally `/sys/fs/cgroup`).
#[derive(Clone, Debug)]
pub struct HostCgroupFs {
    pub root: std::path::PathBuf,
}

impl HostCgroupFs {
    fn path(&self, cgroup: &str) -> std::path::PathBuf {
        self.root.join(cgroup.trim_start_matches('/'))
    }
}

impl CgroupFs for HostCgroupFs {
    fn read(&self, cgroup: &str, file: &str) -> anyhow::Result<String> {
        Ok(std::fs::read_to_string(self.path(cgroup).join(file))?)
    }

    fn write(&self, cgroup: &str, file: &str, value: &str) -> anyhow::Result<()> {
        use std::io::Write;
        let mut handle = std::fs::OpenOptions::new()
            .write(true)
            .open(self.path(cgroup).join(file))?;
        handle.write_all(value.as_bytes())?;
        Ok(())
    }

    fn children(&self, cgroup: &str) -> anyhow::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(self.path(cgroup))? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Ok(name) = entry.file_name().into_string()
            {
                names.push(name);
            }
        }
        Ok(names)
    }
}

/// The cgroup paths of a caller's two units, read from systemd after their markers were checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallerUnits {
    pub ordinary: String,
    pub idle: String,
}

/// What a completed move did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Moved {
    pub from: String,
    pub to: String,
    pub pids: Vec<u32>,
}

fn refused(message: impl Into<String>) -> Refusal {
    Refusal::new(RefusalCode::MoveRefused, message)
}

fn wait_frozen(fs: &dyn CgroupFs, leaf: &str) -> bool {
    let deadline = Instant::now() + FREEZE_TIMEOUT;
    loop {
        let frozen = fs
            .read(leaf, "cgroup.events")
            .is_ok_and(|events| events.lines().any(|line| line.trim() == "frozen 1"));
        if frozen {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn pids_in(fs: &dyn CgroupFs, leaf: &str) -> anyhow::Result<Vec<u32>> {
    Ok(fs
        .read(leaf, "cgroup.procs")?
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect())
}

/// Moves the caller's `job` to the unit for `to` (FR-018).
pub fn move_job(
    fs: &dyn CgroupFs,
    access: &dyn ProcessAccess,
    caller: Caller,
    units: &CallerUnits,
    job: &JobId,
    to: ModeName,
) -> Result<Moved, Refusal> {
    let (source_unit, target_unit) = if to == ModeName::YieldFirst {
        (&units.ordinary, &units.idle)
    } else {
        (&units.idle, &units.ordinary)
    };
    let matches: Vec<String> = fs
        .children(source_unit)
        .map_err(|_| refused("cannot read the source unit's cgroups"))?
        .into_iter()
        .filter(|name| parse_leaf(name).is_some_and(|(_, id)| id == job.as_str()))
        .collect();
    let source = match matches.as_slice() {
        [] => {
            return Err(Refusal::new(
                RefusalCode::JobNotFound,
                format!("job {} is not in the unit it would move from", job.as_str()),
            ));
        }
        [one] => format!("{source_unit}/{one}"),
        _ => return Err(refused("more than one cgroup carries this job's ID")),
    };
    let targets: Vec<String> = fs
        .children(target_unit)
        .unwrap_or_default()
        .into_iter()
        .filter(|name| parse_leaf(name) == Some((to, job.as_str())))
        .collect();
    let target = match targets.as_slice() {
        [one] => format!("{target_unit}/{one}"),
        _ => {
            return Err(refused(format!(
                "no single empty destination cgroup for job {} in mode {}; the agent creates it before a move",
                job.as_str(),
                to.name()
            )));
        }
    };
    let target_ready = fs.children(&target).is_ok_and(|names| names.is_empty())
        && pids_in(fs, &target).is_ok_and(|pids| pids.is_empty());
    if !target_ready {
        return Err(refused(format!(
            "destination cgroup {target} is not empty; the agent creates an empty one before a move"
        )));
    }
    if !fs.children(&source).is_ok_and(|names| names.is_empty()) {
        return Err(refused(
            "the job's cgroup has child cgroups; only a single leaf can be moved",
        ));
    }
    if fs.write(&source, "cgroup.freeze", "1").is_err() {
        return Err(refused("cannot freeze the job's cgroup"));
    }
    let thaw = || fs.write(&source, "cgroup.freeze", "0").is_ok();
    if !wait_frozen(fs, &source) {
        let _thawed = thaw();
        return Err(refused("the job's cgroup did not freeze in time"));
    }
    let result = move_frozen(fs, access, caller, &source, &target);
    // The source is thawed whatever happened; when everything moved, it is empty.
    if !thaw() && result.is_ok() {
        return Err(refused(
            "the job moved but its old cgroup could not be thawed",
        ));
    }
    result.map(|pids| Moved {
        from: source,
        to: target,
        pids,
    })
}

fn move_frozen(
    fs: &dyn CgroupFs,
    access: &dyn ProcessAccess,
    caller: Caller,
    source: &str,
    target: &str,
) -> Result<Vec<u32>, Refusal> {
    let pids = pids_in(fs, source).map_err(|_| refused("cannot list the job's processes"))?;
    if pids.is_empty() {
        return Err(refused("the job has no processes to move"));
    }
    let mut held: Vec<(u32, Box<dyn HeldProcess>)> = Vec::with_capacity(pids.len());
    for &pid in &pids {
        let process = access
            .open(pid, None)
            .map_err(|_| refused(format!("process {pid} could not be held")))?;
        let in_source = process.cgroup().is_ok_and(|cgroup| cgroup == source);
        let ids_match = process
            .ids()
            .is_ok_and(|ids| ids.all_are(caller.uid, caller.primary_gid));
        if !in_source || !ids_match {
            return Err(refused(format!(
                "process {pid} is not the caller's own work in the job's cgroup"
            )));
        }
        held.push((pid, process));
    }
    let mut moved: Vec<u32> = Vec::with_capacity(held.len());
    for (pid, process) in &held {
        let landed = fs.write(target, "cgroup.procs", &pid.to_string()).is_ok()
            && process.cgroup().is_ok_and(|cgroup| cgroup == target);
        if !landed {
            // Put back what already moved, so the job stays whole in its unit.
            for back in &moved {
                let _restored = fs.write(source, "cgroup.procs", &back.to_string());
            }
            return Err(refused(format!("process {pid} could not be moved")));
        }
        moved.push(*pid);
    }
    Ok(moved)
}
