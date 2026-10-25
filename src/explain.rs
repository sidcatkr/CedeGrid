//! What `observe` and `doctor` show (US8): one sample as JSON, and the doctor
//! text, both rendered from a decision.
use crate::config::Config;
use crate::contention::{ContentionMode, ResourceKind, YieldStep};
use crate::execution_model::{Actuator, ActuatorSettings, AllocationClass};
use crate::model::{Decision, Resources, Snapshot};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Large numbers with a space between thousands.
pub fn group(value: u64) -> String {
    let digits = value.to_string();
    if value < 10_000 {
        return digits;
    }
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

/// An amount in the resource's units.
pub fn amount(kind: ResourceKind, value: u64) -> String {
    match kind {
        ResourceKind::Cpu => format!("{} m", group(value)),
        ResourceKind::Ram | ResourceKind::GpuMemory => format!("{} MiB", group(value)),
        ResourceKind::GpuCompute => format!("{value}/1000"),
    }
}

/// One `observe` sample, with `actions` and `exceptions` always present.
pub fn observation(snapshot: &Snapshot, decision: &Decision, id: Option<i64>) -> serde_json::Value {
    let mut decision = serde_json::to_value(decision).unwrap_or_default();
    if let Some(object) = decision.as_object_mut() {
        for key in ["actions", "exceptions"] {
            object.entry(key).or_insert_with(|| serde_json::json!([]));
        }
    }
    serde_json::json!({"observation_id": id, "snapshot": snapshot, "decision": decision})
}

/// The steps open to opportunistic work per resource with the configured
/// backend. Hints need a worker that reads resource targets; memory is the only
/// resource drained on its own.
pub fn available_steps(config: &Config) -> BTreeMap<&'static str, Vec<&'static str>> {
    let actuator = crate::actuation::RootlessActuator::new(&ActuatorSettings {
        class: AllocationClass::Opportunistic,
        requested: Resources::default(),
        period: std::time::Duration::from_millis(config.monitor.interval_ms),
    });
    let authorized = |control: &str| {
        config.cgroup.enabled
            && config
                .cgroup
                .authorized_controls
                .iter()
                .any(|c| c == control)
    };
    ResourceKind::ALL
        .into_iter()
        .map(|kind| {
            let unavailable = actuator.unavailable(kind);
            let mut steps = Vec::new();
            if kind == ResourceKind::Cpu && authorized("cpu.weight") {
                steps.push("lower_share");
            }
            for (step, name) in [(YieldStep::Cap, "cap"), (YieldStep::Pause, "pause")] {
                if !unavailable.contains(&step) {
                    steps.push(name);
                }
            }
            if kind != ResourceKind::GpuCompute {
                steps.push("hint");
            }
            if kind.is_memory() {
                steps.push("drain");
            }
            (kind.name(), steps)
        })
        .collect()
}

fn mode_label(mode: ContentionMode) -> &'static str {
    match mode {
        ContentionMode::YieldFirst => "Yield First",
        ContentionMode::Neutral => "Neutral",
        ContentionMode::Performance => "Performance",
    }
}

/// The doctor text: per resource its mode, usage, demand, target share, steps,
/// reasons and what is held back, then floors, caution options, legacy GPU keys
/// and the steps the backend offers.
pub fn doctor_text(
    config: &Config,
    snapshot: &Snapshot,
    decision: &Decision,
    warnings: &[String],
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "CedeGrid doctor");
    let mut host = format!(
        "host: {} CPU, {} RAM",
        amount(ResourceKind::Cpu, snapshot.cpu_capacity_millicores),
        amount(ResourceKind::Ram, snapshot.total_ram_mib)
    );
    for gpu in &snapshot.gpus {
        let _ = write!(
            host,
            ", {} {}",
            gpu.uuid,
            amount(ResourceKind::GpuMemory, gpu.total_memory_mib)
        );
    }
    let _ = writeln!(out, "{host}");
    let _ = writeln!(
        out,
        "modes: default {}, node cap {}",
        mode_label(config.contention.default_mode),
        decision.max_mode.map_or("none", mode_label)
    );
    for (key, status) in &decision.resources {
        let kind = key.kind();
        let _ = writeln!(out);
        let mode = status.mode.map_or("unknown", mode_label);
        let effective = status.effective_mode.map_or("unknown", mode_label);
        let _ = writeln!(out, "{key}: {mode} (effective {effective})");
        let mut usage = Vec::new();
        if let Some((own, upper)) = status.own {
            usage.push(if upper > own {
                format!("own {} to {}", amount(kind, own), amount(kind, upper))
            } else {
                format!("own {}", amount(kind, own))
            });
        }
        if let Some(external) = status.external {
            usage.push(format!("external {}", amount(kind, external)));
        }
        if let Some(unattributed) = status.unattributed {
            usage.push(format!("unattributed {}", amount(kind, unattributed)));
        }
        if let Some(target) = status.target_share {
            usage.push(format!("target share {}", amount(kind, target)));
        }
        if !usage.is_empty() {
            let _ = writeln!(out, "  {}", usage.join(", "));
        }
        if let Some(admission) = &status.admission {
            match admission.code() {
                Some(code) => {
                    let state = if matches!(admission, crate::contention::Admission::Held { .. }) {
                        "held"
                    } else {
                        "blocked"
                    };
                    let _ = writeln!(
                        out,
                        "  admission {state}: {code}: {}",
                        admission.reason().unwrap_or_default()
                    );
                }
                None => {
                    let _ = writeln!(out, "  admission open");
                }
            }
        }
        for action in decision
            .actions
            .iter()
            .filter(|action| &action.resource == key)
        {
            let target = action.target.map_or(String::new(), |target| {
                format!(" to {}", amount(kind, target))
            });
            let cause = serde_json::to_value(action.cause)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "  step {}{target} on {}: {cause}",
                action.code, action.allocation_id
            );
        }
        for reason in &status.reasons {
            let _ = writeln!(out, "  reason {}: {}", reason.code, reason.text);
        }
        if let Some(held) = &status.held_back {
            let causes: Vec<String> = held
                .causes()
                .into_iter()
                .map(|(cause, value)| {
                    let percent = held
                        .percent
                        .get(cause)
                        .map_or(String::new(), |percent| format!(" ({:.1}%)", percent.0));
                    format!("{cause} {}{percent}", amount(kind, value))
                })
                .collect();
            let _ = writeln!(
                out,
                "  held back: {}",
                if causes.is_empty() {
                    "nothing".into()
                } else {
                    causes.join(", ")
                }
            );
            if let Some(path) = &held.compute_path {
                let _ = writeln!(out, "  compute path: {}", path.code());
            }
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "Floors");
    let floors = config.floors(snapshot);
    let floor = |name: &str, status: &crate::config::FloorStatus| {
        format!(
            "  {name}: configured {} MiB, minimum {} MiB, effective {} MiB{}",
            group(status.configured_mib),
            group(status.minimum_mib),
            group(status.effective_mib),
            if status.raised {
                ", raised to twice the minimum"
            } else {
                ""
            }
        )
    };
    let _ = writeln!(out, "{}", floor("RAM floor", &floors.ram));
    for (uuid, status) in &floors.gpus {
        let _ = writeln!(out, "{}", floor(&format!("{uuid} floor"), status));
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "Caution options");
    let details = config.caution_details(snapshot);
    if details.is_empty() {
        let _ = writeln!(out, "  none");
    }
    for detail in details {
        let settings: Vec<String> = detail
            .settings
            .iter()
            .map(|setting| format!("{} = {}", setting.key, setting.value))
            .collect();
        let _ = writeln!(out, "  [caution] {}: {}", detail.name, settings.join(", "));
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "Legacy GPU keys");
    let legacy = config.legacy_gpu();
    let _ = writeln!(
        out,
        "  gpu.execution_mode = {}: {}",
        serde_json::to_value(legacy.execution_mode)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_default(),
        legacy.interpretation
    );
    for warning in &legacy.warnings {
        let _ = writeln!(out, "  {warning}");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "Available steps");
    for (resource, steps) in available_steps(config) {
        let _ = writeln!(out, "  {resource}: {}", steps.join(", "));
    }
    if !decision.reason_codes.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Node reasons");
        for reason in &decision.reason_codes {
            let _ = writeln!(out, "  {}: {}", reason.code, reason.text);
        }
    }
    if !warnings.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Config warnings");
        for warning in warnings {
            let _ = writeln!(out, "  {warning}");
        }
    }
    out
}
