//! Which journal records the loop reads each iteration (FR-081): unreleased runs, runs a slot
//! or preparation names, and released runs whose spool or outbox still needs work. The whole
//! journal is read once, at start, so a long history costs nothing per iteration.
use crate::{
    execution_model::{ExecutionPhase, ExecutionRecord},
    state::StateStore,
};
use anyhow::{Context, Result};
use std::{collections::BTreeSet, path::Path};

pub(super) struct HistoryScope {
    /// Released runs still followed until `settled`.
    follow: BTreeSet<String>,
    /// The unreleased runs the last iteration saw.
    last_active: BTreeSet<String>,
    /// Whether the upgrade holds table exists; an offline upgrade creates it, never a running
    /// agent's peer.
    holds: bool,
}

impl HistoryScope {
    pub(super) fn new(store: &StateStore, state_dir: &Path) -> Result<Self> {
        let mut follow = BTreeSet::new();
        for record in store.executions()? {
            if record.phase == ExecutionPhase::Released
                && !settled(state_dir, &record.assignment_id)
            {
                follow.insert(record.assignment_id);
            }
        }
        let holds = store.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='upgrade_attempt_holds')",
            [],
            |row| row.get(0),
        )?;
        Ok(Self {
            follow,
            last_active: BTreeSet::new(),
            holds,
        })
    }

    /// This iteration's records: unreleased runs, then the named and followed ones.
    pub(super) fn records<'a>(
        &mut self,
        store: &StateStore,
        named: impl IntoIterator<Item = &'a String>,
    ) -> Result<Vec<ExecutionRecord>> {
        let mut records = store.active_executions()?;
        let active: BTreeSet<String> = records.iter().map(|r| r.assignment_id.clone()).collect();
        // A run that was released since the last iteration is followed until settled.
        self.follow
            .extend(self.last_active.difference(&active).cloned());
        let mut extra: BTreeSet<String> = self.follow.clone();
        extra.extend(named.into_iter().cloned());
        for id in extra.difference(&active) {
            if let Some(record) = store.execution_record(id)? {
                records.push(record);
            }
        }
        self.last_active = active;
        Ok(records)
    }

    /// Stop following released runs whose spool and outbox need nothing more.
    pub(super) fn settle(&mut self, state_dir: &Path) {
        self.follow.retain(|id| !settled(state_dir, id));
    }

    pub(super) fn upgrade_held(&self, store: &StateStore, assignment: &str) -> Result<bool> {
        if !self.holds {
            return Ok(false);
        }
        Ok(store.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM upgrade_attempt_holds WHERE assignment_id=?1)",
            [assignment],
            |row| row.get(0),
        )?)
    }
}

/// Remove the attempt directories of settled released runs outside the newest
/// `REPORTED_RELEASED`, the window heartbeats report (FR-081). A directory without a journal
/// row, an unreleased run's, or one not yet settled stays. Returns how many went.
pub(super) fn prune_attempts(store: &StateStore, state_dir: &Path) -> Result<usize> {
    let keep: BTreeSet<String> = store
        .heartbeat_executions()?
        .into_iter()
        .map(|r| r.assignment_id)
        .collect();
    let attempts = state_dir.join("attempts");
    let entries = match std::fs::read_dir(&attempts) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if id.starts_with('.') || keep.contains(&id) || !entry.file_type()?.is_dir() {
            continue;
        }
        let released = store
            .execution_record(&id)?
            .is_some_and(|record| record.phase == ExecutionPhase::Released);
        if released && settled(state_dir, &id) {
            std::fs::remove_dir_all(entry.path())
                .with_context(|| format!("removing attempt directory {id}"))?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Nothing is left to reclaim or deliver: the attempt's directory is gone, or its spool was
/// reclaimed and its outcome receipted. A path that cannot be read is not settled.
fn settled(state_dir: &Path, id: &str) -> bool {
    let output = state_dir.join("attempts").join(id);
    let present = |path: std::path::PathBuf| matches!(path.try_exists(), Ok(true));
    matches!(output.try_exists(), Ok(false))
        || present(output.join("spool-reclaimed.json"))
            && (present(output.join("result.receipt.json"))
                || present(output.join("failure.receipt.json"))
                || present(
                    state_dir
                        .join("failure-receipts")
                        .join(format!("{id}.json")),
                ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{execution_model::*, model::Resources};
    use std::collections::BTreeMap;

    fn request(task: &str, assignment: &str) -> LaunchRequest {
        LaunchRequest {
            task_id: task.into(),
            assignment_id: assignment.into(),
            argv: vec!["unused".into()],
            cwd: std::env::current_dir().unwrap(),
            env: BTreeMap::new(),
            resources: resources(),
            replay_safe: true,
            class: AllocationClass::Opportunistic,
            no_escape: true,
            single_process: true,
            managed_child_limit: 0,
            max_attempts: None,
            input_artifacts: vec![],
            required_controls: vec![],
            allow_fallback: false,
            contention: None,
        }
    }

    fn resources() -> Resources {
        Resources {
            cpu_millicores: 10,
            ram_mib: 1,
            gpu_memory_mib: BTreeMap::new(),
        }
    }

    fn capacity() -> Resources {
        Resources {
            cpu_millicores: 1_000_000,
            ram_mib: 1_000_000,
            gpu_memory_mib: BTreeMap::new(),
        }
    }

    fn release(store: &StateStore, mut record: ExecutionRecord) {
        record.phase = ExecutionPhase::Released;
        store.transition(&record).unwrap();
    }

    #[test]
    fn released_history_is_read_once_at_start_and_never_per_iteration() {
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let store = StateStore::open(dir.path()).unwrap();
        for i in 0..40 {
            let record = store
                .reserve(&request(&format!("old{i}"), &format!("o{i}")), &capacity())
                .unwrap();
            release(&store, record);
        }
        store.reserve(&request("live", "l1"), &capacity()).unwrap();
        let mut scope = HistoryScope::new(&store, dir.path()).unwrap();
        let named = ["n1".to_string()];
        let records = scope.records(&store, &named).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|r| r.assignment_id.as_str())
                .collect::<Vec<_>>(),
            ["l1"],
            "released runs without an attempt directory are settled; a named run without a row is skipped"
        );
    }

    #[test]
    fn a_run_released_during_the_run_is_followed_until_settled() {
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let store = StateStore::open(dir.path()).unwrap();
        let record = store.reserve(&request("t", "a1"), &capacity()).unwrap();
        let output = dir.path().join("attempts").join("a1");
        std::fs::create_dir_all(&output).unwrap();
        let mut scope = HistoryScope::new(&store, dir.path()).unwrap();
        assert_eq!(scope.records(&store, &[]).unwrap().len(), 1);
        release(&store, record);
        let records = scope.records(&store, &[]).unwrap();
        assert_eq!(records.len(), 1, "a just-released run is still read");
        assert_eq!(records[0].phase, ExecutionPhase::Released);
        scope.settle(dir.path());
        assert_eq!(
            scope.records(&store, &[]).unwrap().len(),
            1,
            "not yet reclaimed"
        );
        std::fs::write(output.join("spool-reclaimed.json"), "{}").unwrap();
        std::fs::write(output.join("result.receipt.json"), "{}").unwrap();
        scope.settle(dir.path());
        assert!(scope.records(&store, &[]).unwrap().is_empty());
        // A restart does not pick a settled run up again.
        let mut restarted = HistoryScope::new(&store, dir.path()).unwrap();
        assert!(restarted.records(&store, &[]).unwrap().is_empty());
    }

    #[test]
    fn settled_attempt_directories_beyond_the_newest_256_are_removed() {
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let store = StateStore::open(dir.path()).unwrap();
        let attempts = dir.path().join("attempts");
        for i in 0..300 {
            let id = format!("r{i:03}");
            let record = store
                .reserve(&request(&format!("t{i}"), &id), &capacity())
                .unwrap();
            release(&store, record);
            let output = attempts.join(&id);
            std::fs::create_dir_all(&output).unwrap();
            std::fs::write(output.join("spool-reclaimed.json"), "{}").unwrap();
            std::fs::write(output.join("result.receipt.json"), "{}").unwrap();
            std::fs::write(output.join("stdout.log"), "log").unwrap();
        }
        // Released but not yet reclaimed, an active run, and a directory with no journal row.
        let unsettled = store
            .reserve(&request("u", "unsettled"), &capacity())
            .unwrap();
        release(&store, unsettled);
        std::fs::create_dir_all(attempts.join("unsettled")).unwrap();
        store
            .reserve(&request("live", "live"), &capacity())
            .unwrap();
        std::fs::create_dir_all(attempts.join("live")).unwrap();
        std::fs::create_dir_all(attempts.join("stray")).unwrap();
        std::fs::create_dir_all(attempts.join(".input-cache")).unwrap();
        assert_eq!(prune_attempts(&store, dir.path()).unwrap(), 45);
        assert!(!attempts.join("r000").exists());
        assert!(!attempts.join("r044").exists());
        assert!(
            attempts.join("r045").exists(),
            "the newest 256 released stay, with the unsettled one"
        );
        assert!(attempts.join("r299").exists());
        for kept in ["unsettled", "live", "stray", ".input-cache"] {
            assert!(attempts.join(kept).exists(), "{kept}");
        }
        assert_eq!(prune_attempts(&store, dir.path()).unwrap(), 0);
    }

    #[test]
    fn the_upgrade_holds_table_is_looked_up_once_per_start() {
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let store = StateStore::open(dir.path()).unwrap();
        let record = store.reserve(&request("t", "a1"), &capacity()).unwrap();
        let scope = HistoryScope::new(&store, dir.path()).unwrap();
        assert!(!scope.upgrade_held(&store, &record.assignment_id).unwrap());
        store
            .connection
            .execute_batch(
                "CREATE TABLE upgrade_attempt_holds(assignment_id TEXT PRIMARY KEY) STRICT;
                 INSERT INTO upgrade_attempt_holds VALUES ('a1');",
            )
            .unwrap();
        assert!(
            !scope.upgrade_held(&store, "a1").unwrap(),
            "decided at start"
        );
        let restarted = HistoryScope::new(&store, dir.path()).unwrap();
        assert!(restarted.upgrade_held(&store, "a1").unwrap());
    }
}
