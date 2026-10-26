//! Decision snapshots: every decision and node report the scripted scenarios
//! produce, as JSON lines, so a refactor can show it changes no output. Each test
//! binary that includes this module runs its scenarios under `check`, which is
//! ignored by default. `CEDEGRID_DECISION_SNAPSHOT=<dir>` enables it, and
//! `CEDEGRID_DECISION_SNAPSHOT_WRITE=1` writes `<dir>/<name>.jsonl` instead of
//! comparing with it byte for byte.
#![allow(dead_code)]

use cedegrid::{config::Config, model::*};
use std::cell::RefCell;

thread_local! {
    /// The scenario running and the lines recorded so far, while `check` runs.
    static RECORDING: RefCell<Option<(String, Vec<String>)>> = const { RefCell::new(None) };
}

/// The policy engine, recording each result while `check` runs on this thread.
#[derive(Debug, Default)]
pub struct PolicyEngine(cedegrid::policy::PolicyEngine);

impl PolicyEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn evaluate(
        &mut self,
        config: &Config,
        snapshot: &Snapshot,
        input: &PolicyInput,
        now_monotonic_ms: u64,
    ) -> anyhow::Result<Decision> {
        let result = self.0.evaluate(config, snapshot, input, now_monotonic_ms);
        record(config, &result);
        result
    }
}

impl std::ops::Deref for PolicyEngine {
    type Target = cedegrid::policy::PolicyEngine;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for PolicyEngine {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

fn record(config: &Config, result: &anyhow::Result<Decision>) {
    RECORDING.with(|recording| {
        let mut recording = recording.borrow_mut();
        let Some((scenario, lines)) = recording.as_mut() else {
            return;
        };
        let line = match result {
            Ok(decision) => serde_json::json!({
                "scenario": scenario,
                "decision": decision,
                "report": cedegrid::agent::node_contention_report(config, decision, 4),
            }),
            Err(error) => serde_json::json!({"scenario": scenario, "error": format!("{error:#}")}),
        };
        lines.push(line.to_string());
    });
}

/// A named run whose decisions go into the snapshot.
pub type Scenario = (String, Box<dyn Fn()>);

/// Test functions of `module` as scenarios.
pub fn scripted(module: &str, tests: &[(&str, fn())]) -> Vec<Scenario> {
    tests
        .iter()
        .map(|(name, run)| {
            let run = *run;
            (format!("{module}::{name}"), Box::new(run) as Box<dyn Fn()>)
        })
        .collect()
}

/// Runs each scenario with recording on, then writes or compares
/// `<dir>/<name>.jsonl`.
pub fn check(name: &str, scenarios: Vec<Scenario>) {
    let dir = std::env::var_os("CEDEGRID_DECISION_SNAPSHOT")
        .map(std::path::PathBuf::from)
        .expect("CEDEGRID_DECISION_SNAPSHOT names the snapshot directory");
    let mut lines = Vec::new();
    for (scenario, run) in &scenarios {
        RECORDING.with(|r| *r.borrow_mut() = Some((scenario.clone(), std::mem::take(&mut lines))));
        run();
        lines = RECORDING
            .with(|r| r.borrow_mut().take())
            .map(|(_, recorded)| recorded)
            .unwrap_or_default();
    }
    let text = lines.join("\n") + "\n";
    let path = dir.join(format!("{name}.jsonl"));
    if std::env::var_os("CEDEGRID_DECISION_SNAPSHOT_WRITE").is_some() {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, &text).unwrap();
        eprintln!(
            "wrote {} decisions from {} scenarios to {}",
            lines.len(),
            scenarios.len(),
            path.display()
        );
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    if text != expected {
        let actual: Vec<&str> = text.lines().collect();
        let wanted: Vec<&str> = expected.lines().collect();
        let first = (0..actual.len().max(wanted.len()))
            .find(|i| actual.get(*i) != wanted.get(*i))
            .unwrap_or(0);
        let clip =
            |line: Option<&&str>| line.map_or(String::new(), |l| l.chars().take(600).collect());
        panic!(
            "{name}: {} decisions, snapshot has {}; first difference at line {}:\n  now:  {}\n  then: {}",
            actual.len(),
            wanted.len(),
            first + 1,
            clip(actual.get(first)),
            clip(wanted.get(first))
        );
    }
    eprintln!(
        "{name}: {} decisions from {} scenarios match {}",
        lines.len(),
        scenarios.len(),
        path.display()
    );
}
