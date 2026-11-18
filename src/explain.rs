//! What `observe` and `doctor` show (US8): one sample as JSON, and the doctor
//! text, both rendered from a decision.
use crate::config::Config;
use crate::contention::{ContentionMode, ResourceKind, YieldStep};
use crate::execution_model::{Actuator, ActuatorSettings, AllocationClass};
use crate::model::{Decision, Resources, Snapshot};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// A mode as people read it; Aggressive carries its caution label (004 FR-007).
fn shown(mode: ContentionMode) -> String {
    match mode {
        ContentionMode::Aggressive => "[Aggressive]".to_owned(),
        mode => mode.label().to_owned(),
    }
}

/// The mode whose rules a resource follows: memory follows Performance under Aggressive (004
/// FR-006).
pub fn behaves_as(kind: ResourceKind, mode: ContentionMode) -> ContentionMode {
    if kind.is_memory() && mode == ContentionMode::Aggressive {
        ContentionMode::Performance
    } else {
        mode
    }
}

/// A resource's mode as doctor heads its block: memory under Aggressive shows the mode it
/// follows, and the effective mode only when it differs.
fn mode_text(kind: ResourceKind, status: &crate::model::ResourceStatus) -> String {
    let follows = |mode: Option<ContentionMode>| mode.map(|mode| behaves_as(kind, mode));
    let (mode, effective) = (follows(status.mode), follows(status.effective_mode));
    let mut text = mode.map_or("unknown".to_owned(), shown);
    if status.mode == Some(ContentionMode::Aggressive) && mode != status.mode {
        text.push_str(" (Aggressive covers compute only)");
    }
    if effective != mode {
        let _ = write!(
            text,
            " (effective {})",
            effective.map_or("unknown".to_owned(), shown)
        );
    }
    text
}

/// One line per resource: mode, admission and the amounts that decide it.
fn overview(decision: &Decision) -> Vec<String> {
    let mut rows = vec![
        [
            "RESOURCE",
            "MODE",
            "ADMISSION",
            "OWN",
            "EXTERNAL",
            "TARGET SHARE",
        ]
        .map(str::to_owned)
        .to_vec(),
    ];
    for (key, status) in &decision.resources {
        let kind = key.kind();
        let value = |value: Option<u64>| value.map_or("-".to_owned(), |value| amount(kind, value));
        let mode = status.mode.map(|mode| behaves_as(kind, mode));
        rows.push(vec![
            key.to_string(),
            mode.map_or("unknown".to_owned(), shown),
            match &status.admission {
                Some(crate::contention::Admission::Open) => "open".to_owned(),
                Some(crate::contention::Admission::Held { .. }) => "held".to_owned(),
                Some(_) => "blocked".to_owned(),
                None => "-".to_owned(),
            },
            value(status.own.map(|(own, _)| own)),
            value(status.external),
            value(status.target_share),
        ]);
    }
    table(&rows, 2)
}

/// Own work's arrival on a compute resource and a row for each other account there: plain or
/// CedeGrid, its arrival, whether it came first, and whether admission holds for it in
/// Performance and Aggressive (006 FR-021). Empty where there is neither.
pub fn principals_text(status: &crate::model::ResourceStatus) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(at) = status.own_arrival_unix_ms {
        lines.push(format!("  own arrival: {at} ms since the epoch"));
    }
    if status.principals.is_empty() {
        return lines;
    }
    let yes = |on: bool| if on { "yes" } else { "no" }.to_owned();
    let mut rows = vec![
        ["UID", "KIND", "ARRIVAL", "FIRST", "PROTECTED", "HELD FOR"]
            .map(str::to_owned)
            .to_vec(),
    ];
    for principal in &status.principals {
        rows.push(vec![
            principal.uid.to_string(),
            if principal.cedegrid {
                "cedegrid"
            } else {
                "plain"
            }
            .to_owned(),
            match (principal.cedegrid, principal.arrival_unix_ms) {
                (false, _) => "-".to_owned(),
                (true, Some(at)) => at.to_string(),
                (true, None) => "unknown".to_owned(),
            },
            yes(principal.first_comer),
            yes(principal.protected),
            yes(principal.held_for),
        ]);
    }
    lines.push("  other accounts:".to_owned());
    lines.extend(table(&rows, 4));
    lines
}

/// Rows in aligned columns, indented; trailing blank cells add no spaces.
pub fn table(rows: &[Vec<String>], indent: usize) -> Vec<String> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    rows.iter()
        .map(|row| {
            let mut line = " ".repeat(indent);
            for (column, cell) in row.iter().enumerate() {
                if column + 1 < row.len() {
                    let _ = write!(line, "{cell:<width$}   ", width = widths[column]);
                } else {
                    line.push_str(cell);
                }
            }
            line.trim_end().to_owned()
        })
        .collect()
}

/// Whether unprotected Aggressive may start on this host, for `doctor`: what is missing and
/// how to fix each (004 FR-014, FR-015, FR-015a).
pub fn unprotected_text(
    gaps: &[crate::system_mode::Gap],
    runs: &[crate::execution_model::UnprotectedRun],
    unreadable: Option<&str>,
) -> String {
    let mut out = String::from("\nUnprotected Aggressive\n");
    if gaps.is_empty() {
        out.push_str("  available: a run that sends both SDK fields may start unprotected\n");
    } else {
        out.push_str("  not available on this host\n");
        for gap in gaps {
            let _ = writeln!(out, "  - {}", gap.what);
            if !gap.fix.is_empty() {
                let _ = writeln!(out, "    fix: {}", gap.fix);
            }
        }
    }
    // Runs that started unprotected keep their grant until they end (004 FR-016).
    if let Some(reason) = unreadable {
        let _ = writeln!(out, "  running unprotected: unknown, {reason}");
    }
    let rows: Vec<Vec<String>> = runs
        .iter()
        .map(|run| {
            vec![
                "UNPROTECTED".to_owned(),
                run.task_id.clone(),
                run.started_unix_ms
                    .map_or("since an earlier start".to_owned(), |at| {
                        format!("since {}", crate::setup::utc_minute(at))
                    }),
            ]
        })
        .collect();
    for line in table(&rows, 2) {
        let _ = writeln!(out, "{line}");
    }
    out
}

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
/// backend. Hints need a worker that reads resource targets, and only memory takes one; CPU and
/// GPU compute take no rate cap; a RAM cap needs a delegated leaf with `memory.high`; memory is
/// the only resource drained on its own.
pub fn available_steps(config: &Config) -> BTreeMap<&'static str, Vec<&'static str>> {
    let actuator = crate::actuation::RootlessActuator::new(&ActuatorSettings {
        class: AllocationClass::Opportunistic,
        requested: Resources::default(),
        period: std::time::Duration::from_millis(config.monitor.interval_ms),
        ram_noise_tolerance_mib: config.ram_cap_check().0,
        ram_calm_window: std::time::Duration::from_millis(config.ram_cap_check().1),
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
            if kind == ResourceKind::Ram && authorized("memory.high") {
                steps.push("cap");
            }
            // CPU and GPU compute are held back by whole tasks, which take no hint (FR-089).
            if kind.is_memory() {
                steps.extend(["hint", "drain"]);
            }
            (kind.name(), steps)
        })
        .collect()
}

/// The CPU pressure line (FR-072): external pressure, the only saturation
/// evidence, beside own and system-wide pressure, which are shown only.
fn psi_line(config: &Config, snapshot: &Snapshot) -> Option<String> {
    use crate::kernel::CpuPressureStatus;
    let pressure = snapshot.kernel.as_ref()?.cpu_pressure.as_ref()?;
    let percent = |value: Option<f64>| value.map_or("n/a".to_string(), |v| format!("{v:.1}%"));
    let shown = format!(
        "own {}, system {}",
        percent(pressure.own_some_avg10),
        percent(pressure.system_some_avg10)
    );
    Some(match pressure.status {
        CpuPressureStatus::Available => format!(
            "psi: external {} over {} cgroups (saturates above {}%), {shown}; a cgroup shared with own work never counts",
            percent(pressure.external_some_avg10),
            pressure.external_cgroups,
            config
                .contention
                .thresholds(ResourceKind::Cpu)
                .psi_some_percent
        ),
        CpuPressureStatus::SystemOnly => {
            format!("psi: system-wide only, no external evidence; {shown}")
        }
        CpuPressureStatus::Unknown => format!("psi: unknown, no evidence; {shown}"),
    })
}

/// Settings derived from others because they were not set, for `doctor` (FR-070); empty when
/// every one is set.
pub fn derived_text(config: &Config) -> String {
    let derived = config.derived_settings();
    if derived.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nDerived settings\n");
    for (key, setting) in derived {
        let _ = writeln!(
            out,
            "  {key} = {} (derived: {})",
            setting.value, setting.rule
        );
    }
    out
}

/// How this host finds a run's descendants, for `doctor` (FR-083, FR-084): `lists` is
/// whether the kernel keeps `children` lists, `None` where descendants are not tracked.
pub fn descendants_text(lists: Option<bool>) -> String {
    let line = match lists {
        Some(true) => "tracked from /proc children lists",
        Some(false) => {
            "tracked by a scan of /proc each interval: this kernel keeps no children lists (CONFIG_PROC_CHILDREN)"
        }
        None => "not tracked on this platform: rootless runs need the single-process contract",
    };
    format!("\nDescendants\n  {line}\n")
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
    let _ = writeln!(out);
    let mut host = format!(
        "{} CPU · {} RAM",
        amount(ResourceKind::Cpu, snapshot.cpu_capacity_millicores),
        amount(ResourceKind::Ram, snapshot.total_ram_mib)
    );
    for gpu in &snapshot.gpus {
        let _ = write!(
            host,
            " · {} {}",
            gpu.uuid,
            amount(ResourceKind::GpuMemory, gpu.total_memory_mib)
        );
    }
    let header = [
        ("Host", host),
        ("Default mode", shown(config.contention.default_mode)),
        (
            "Node cap",
            decision.max_mode.map_or("none".to_owned(), shown),
        ),
    ];
    for line in table(&header.map(|(key, value)| vec![key.to_owned(), value]), 2) {
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(out);
    for line in overview(decision) {
        let _ = writeln!(out, "{line}");
    }
    for (key, status) in &decision.resources {
        let kind = key.kind();
        let _ = writeln!(out);
        let _ = writeln!(out, "{key}: {}", mode_text(kind, status));
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
        if let Some(cedegrid) = status.cedegrid {
            usage.push(format!("in use (CedeGrid) {}", amount(kind, cedegrid)));
        }
        if let Some(target) = status.target_share {
            usage.push(format!("target share {}", amount(kind, target)));
        }
        if !usage.is_empty() {
            let _ = writeln!(out, "  {}", usage.join(", "));
        }
        if *key == crate::contention::ResourceKey::Cpu
            && let Some(line) = psi_line(config, snapshot)
        {
            let _ = writeln!(out, "  {line}");
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
        for line in principals_text(status) {
            let _ = writeln!(out, "{line}");
        }
        if let Some(held) = &status.held_back {
            let share = |cause: &str| {
                held.percent
                    .get(cause)
                    .map_or(String::new(), |percent| format!(" ({:.1}%)", percent.0))
            };
            // An ancestor ceiling limits the scope itself, so it gets its own line.
            if held.ceiling > 0 {
                let _ = writeln!(
                    out,
                    "  ceiling: {}{}",
                    amount(kind, held.ceiling),
                    share("ceiling")
                );
            }
            let causes: Vec<String> = held
                .causes()
                .into_iter()
                .filter(|(cause, _)| *cause != "ceiling")
                .map(|(cause, value)| format!("{cause} {}{}", amount(kind, value), share(cause)))
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
    let floor = |name: String, status: &crate::config::FloorStatus| {
        vec![
            name,
            format!("{} MiB", group(status.configured_mib)),
            format!("{} MiB", group(status.minimum_mib)),
            format!("{} MiB", group(status.effective_mib)),
            if status.raised {
                "raised to twice the minimum".to_owned()
            } else {
                String::new()
            },
        ]
    };
    let mut rows = vec![
        ["", "CONFIGURED", "MINIMUM", "EFFECTIVE", ""]
            .map(str::to_owned)
            .to_vec(),
        floor("RAM floor".into(), &floors.ram),
    ];
    for (uuid, status) in &floors.gpus {
        rows.push(floor(format!("{uuid} floor"), status));
    }
    for line in table(&rows, 2) {
        let _ = writeln!(out, "{line}");
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
