//! Node setup: choices, risk markers, flags and the terminal interface.
//!
//! Flags, the terminal interface and the line prompts all reduce to one
//! [`Choices`], and one writer applies it, editing a key only when the choice
//! differs from the current value. Equal choices therefore give byte-identical
//! files (SC-011).
use crate::config::{ACKNOWLEDGEMENTS, CONFIG_ERROR, CONFIG_VERSION, CautionDetail, Config};
use crate::contention::{ContentionMode, ResourceKind, ResourceThresholds};
use crate::model::{ExceptionNotice, RiskMarker};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table};

/// A setup failure with its exit status and stable code (contracts/setup-cli.md).
#[derive(Debug)]
pub struct SetupError {
    pub exit: i32,
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for SetupError {}

fn fail(exit: i32, code: &'static str, message: impl Into<String>) -> anyhow::Error {
    SetupError {
        exit,
        code,
        message: message.into(),
    }
    .into()
}

fn missing(message: impl Into<String>) -> anyhow::Error {
    fail(2, "ERR_CEDEGRID_SETUP_MISSING_CHOICE", message)
}

fn argument(message: impl Into<String>) -> anyhow::Error {
    fail(2, "ERR_CEDEGRID_ARGUMENT", message)
}

fn unacknowledged(message: impl Into<String>) -> anyhow::Error {
    fail(3, "ERR_CEDEGRID_CAUTION_UNACKNOWLEDGED", message)
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    fail(4, CONFIG_ERROR, message)
}

fn cancelled() -> anyhow::Error {
    fail(
        130,
        "ERR_CEDEGRID_SETUP_CANCELLED",
        "setup was cancelled; nothing was written",
    )
}

/// A configuration load failure, as loading reports it.
fn load_error(error: anyhow::Error) -> anyhow::Error {
    let text = format!("{error:#}");
    let text = text
        .strip_prefix(&format!("{CONFIG_ERROR}: "))
        .unwrap_or(&text)
        .to_string();
    match text.strip_prefix("ERR_CEDEGRID_CAUTION_UNACKNOWLEDGED: ") {
        Some(rest) => unacknowledged(rest),
        None => invalid(text),
    }
}

/// Which configuration file setup writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Node,
    Coordinator,
    Client,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Node => "node",
            Role::Coordinator => "coordinator",
            Role::Client => "client",
        }
    }
}

/// The `setup` flags (contracts/setup-cli.md). Values are parsed here, so errors
/// keep the JSON shape and exit statuses of the contract.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct SetupArgs {
    /// Use only flags and the existing configuration, even on a terminal.
    #[arg(long)]
    pub non_interactive: bool,
    /// Print the result as JSON.
    #[arg(long)]
    pub json: bool,
    #[arg(long, value_enum)]
    pub role: Option<Role>,
    /// yield_first, neutral or performance.
    #[arg(long, value_name = "MODE")]
    pub default_mode: Option<String>,
    /// RESOURCE=MODE; RESOURCE is cpu, ram, gpu_compute or gpu_memory, and MODE
    /// `default` follows the default mode.
    #[arg(long = "mode", value_name = "RESOURCE=MODE")]
    pub modes: Vec<String>,
    /// The most aggressive mode a job may request; `default` removes the cap.
    #[arg(long, value_name = "MODE")]
    pub max_mode: Option<String>,
    #[arg(long, value_name = "N")]
    pub ram_floor_mib: Option<u64>,
    #[arg(long, value_name = "N")]
    pub gpu_floor_mib: Option<u64>,
    /// RESOURCE.MODE=PERCENT, validated as yield_first >= neutral >= performance.
    #[arg(long = "margin", value_name = "RESOURCE.MODE=PERCENT")]
    pub margins: Vec<String>,
    #[arg(long, value_name = "N")]
    pub guaranteed_hint_deadline_s: Option<u64>,
    /// RESOURCE.KEY=VALUE for a tuned threshold, e.g. gpu_memory.noise_tolerance_mib=512.
    #[arg(long = "set", value_name = "RESOURCE.KEY=VALUE")]
    pub sets: Vec<String>,
    /// small_floor, reduced_margin or relaxed_detection.
    #[arg(long = "acknowledge-caution", value_name = "OPTION")]
    pub acknowledge: Vec<String>,
    /// Client: the coordinator's HTTPS origin.
    #[arg(long, value_name = "URL")]
    pub endpoint: Option<String>,
    /// Coordinator: the listen address.
    #[arg(long, value_name = "ADDRESS")]
    pub listen: Option<String>,
    /// Coordinator: the state directory.
    #[arg(long, value_name = "PATH")]
    pub state_dir: Option<String>,
    /// Coordinator: the SHA256 fingerprint of an operator certificate to authorize.
    #[arg(long, value_name = "HEX")]
    pub operator_fingerprint: Option<String>,
    /// Client and coordinator: TLS files.
    #[arg(long, value_name = "PATH")]
    pub ca_cert: Option<String>,
    #[arg(long, value_name = "PATH")]
    pub certificate: Option<String>,
    #[arg(long, value_name = "PATH")]
    pub private_key: Option<String>,
    /// Allow unprotected on this host: on a terminal only, after the host's numbers
    /// and the fixed phrase (004 FR-013).
    #[arg(long, conflicts_with = "disallow_unprotected")]
    pub allow_unprotected: bool,
    /// Turn that allowance off; no confirmation.
    #[arg(long)]
    pub disallow_unprotected: bool,
}

/// What an operator chose. Unset fields keep the current configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Choices {
    pub role: Option<Role>,
    pub default_mode: Option<ContentionMode>,
    /// `None` follows the default mode.
    pub modes: BTreeMap<ResourceKind, Option<ContentionMode>>,
    /// `Some(None)` removes the cap.
    pub max_mode: Option<Option<ContentionMode>>,
    pub ram_floor_mib: Option<u64>,
    pub gpu_floor_mib: Option<u64>,
    pub margins: BTreeMap<(ResourceKind, ContentionMode), u8>,
    pub guaranteed_hint_deadline_s: Option<u64>,
    /// Tuned thresholds as TOML literals.
    pub sets: BTreeMap<(ResourceKind, String), String>,
    pub acknowledge: BTreeSet<String>,
    pub connection: BTreeMap<ConnectionKey, String>,
}

/// Client and coordinator settings setup manages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConnectionKey {
    Endpoint,
    Listen,
    StateDir,
    OperatorFingerprint,
    CaCert,
    Certificate,
    PrivateKey,
}

impl ConnectionKey {
    fn flag(self) -> &'static str {
        match self {
            ConnectionKey::Endpoint => "--endpoint",
            ConnectionKey::Listen => "--listen",
            ConnectionKey::StateDir => "--state-dir",
            ConnectionKey::OperatorFingerprint => "--operator-fingerprint",
            ConnectionKey::CaCert => "--ca-cert",
            ConnectionKey::Certificate => "--certificate",
            ConnectionKey::PrivateKey => "--private-key",
        }
    }

    fn path(self) -> (&'static [&'static str], &'static str) {
        match self {
            ConnectionKey::Endpoint => (&[], "endpoint"),
            ConnectionKey::Listen => (&[], "listen"),
            ConnectionKey::StateDir => (&[], "state_dir"),
            // Written as `[clients."<fingerprint>"] role = "operator"`.
            ConnectionKey::OperatorFingerprint => (&["clients"], ""),
            ConnectionKey::CaCert => (&["tls"], "ca_cert"),
            ConnectionKey::Certificate => (&["tls"], "certificate"),
            ConnectionKey::PrivateKey => (&["tls"], "private_key"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            ConnectionKey::Endpoint => "Coordinator endpoint (https://host:port)",
            ConnectionKey::Listen => "Listen address (host:port)",
            ConnectionKey::StateDir => "State directory",
            ConnectionKey::OperatorFingerprint => "Operator certificate SHA256 fingerprint",
            ConnectionKey::CaCert => "CA certificate",
            ConnectionKey::Certificate => "Certificate",
            ConnectionKey::PrivateKey => "Private key",
        }
    }

    /// The settings each role needs, in prompt order.
    fn for_role(role: Role) -> &'static [ConnectionKey] {
        use ConnectionKey::*;
        match role {
            Role::Node => &[],
            Role::Client => &[Endpoint, CaCert, Certificate, PrivateKey],
            Role::Coordinator => &[
                Listen,
                StateDir,
                OperatorFingerprint,
                CaCert,
                Certificate,
                PrivateKey,
            ],
        }
    }
}

fn resource_label(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Cpu => "CPU",
        ResourceKind::Ram => "RAM",
        ResourceKind::GpuCompute => "GPU compute",
        ResourceKind::GpuMemory => "GPU memory",
    }
}

fn parse_mode(text: &str, flag: &str) -> Result<ContentionMode> {
    match text {
        "yield_first" => Ok(ContentionMode::YieldFirst),
        "neutral" => Ok(ContentionMode::Neutral),
        "performance" => Ok(ContentionMode::Performance),
        "aggressive" => Ok(ContentionMode::Aggressive),
        _ => Err(argument(format!(
            "{flag} takes yield_first, neutral, performance or aggressive, not {text:?}"
        ))),
    }
}

fn parse_resource(text: &str, flag: &str) -> Result<ResourceKind> {
    ResourceKind::ALL
        .into_iter()
        .find(|kind| kind.name() == text)
        .ok_or_else(|| {
            argument(format!(
                "{flag} takes cpu, ram, gpu_compute or gpu_memory, not {text:?}"
            ))
        })
}

fn split_pair<'a>(text: &'a str, flag: &str, shape: &str) -> Result<(&'a str, &'a str)> {
    text.split_once('=')
        .ok_or_else(|| argument(format!("{flag} takes {shape}, not {text:?}")))
}

impl Choices {
    pub fn from_args(args: &SetupArgs) -> Result<Self> {
        let mut choices = Choices {
            role: args.role,
            ..Choices::default()
        };
        if let Some(mode) = &args.default_mode {
            choices.default_mode = Some(parse_mode(mode, "--default-mode")?);
        }
        for pair in &args.modes {
            let (resource, mode) = split_pair(pair, "--mode", "RESOURCE=MODE")?;
            let kind = parse_resource(resource, "--mode")?;
            let mode = match mode {
                "default" => None,
                mode => Some(parse_mode(mode, "--mode")?),
            };
            choices.modes.insert(kind, mode);
        }
        if let Some(mode) = &args.max_mode {
            choices.max_mode = Some(match mode.as_str() {
                "default" => None,
                mode => Some(parse_mode(mode, "--max-mode")?),
            });
        }
        choices.ram_floor_mib = args.ram_floor_mib;
        choices.gpu_floor_mib = args.gpu_floor_mib;
        for pair in &args.margins {
            let (key, percent) = split_pair(pair, "--margin", "RESOURCE.MODE=PERCENT")?;
            let (resource, mode) = key.split_once('.').ok_or_else(|| {
                argument(format!(
                    "--margin takes RESOURCE.MODE=PERCENT, not {pair:?}"
                ))
            })?;
            let percent: u8 = percent.parse().map_err(|_| {
                argument(format!(
                    "--margin takes a whole percentage, not {percent:?}"
                ))
            })?;
            let (kind, mode) = (
                parse_resource(resource, "--margin")?,
                parse_mode(mode, "--margin")?,
            );
            if kind.is_memory() && mode != ContentionMode::YieldFirst {
                return Err(argument(format!(
                    "--margin {key} does not apply: {}",
                    crate::config::MEMORY_FLOOR_ONLY
                )));
            }
            choices.margins.insert((kind, mode), percent);
        }
        choices.guaranteed_hint_deadline_s = args.guaranteed_hint_deadline_s;
        for pair in &args.sets {
            let (key, literal) = split_pair(pair, "--set", "RESOURCE.KEY=VALUE")?;
            let (resource, key) = key
                .split_once('.')
                .ok_or_else(|| argument(format!("--set takes RESOURCE.KEY=VALUE, not {pair:?}")))?;
            literal.parse::<toml_edit::Value>().map_err(|_| {
                argument(format!("--set {key} takes a TOML value, not {literal:?}"))
            })?;
            choices.sets.insert(
                (parse_resource(resource, "--set")?, key.to_string()),
                literal.to_string(),
            );
        }
        for option in &args.acknowledge {
            if !ACKNOWLEDGEMENTS.contains(&option.as_str()) {
                return Err(argument(format!(
                    "--acknowledge-caution takes {}, not {option:?}",
                    ACKNOWLEDGEMENTS.join(", ")
                )));
            }
            choices.acknowledge.insert(option.clone());
        }
        for (key, value) in [
            (ConnectionKey::Endpoint, &args.endpoint),
            (ConnectionKey::Listen, &args.listen),
            (ConnectionKey::StateDir, &args.state_dir),
            (
                ConnectionKey::OperatorFingerprint,
                &args.operator_fingerprint,
            ),
            (ConnectionKey::CaCert, &args.ca_cert),
            (ConnectionKey::Certificate, &args.certificate),
            (ConnectionKey::PrivateKey, &args.private_key),
        ] {
            if let Some(value) = value {
                choices.connection.insert(key, value.clone());
            }
        }
        Ok(choices)
    }

    /// The node flags this run sets, for errors that name them.
    fn node_flags(&self) -> Vec<&'static str> {
        [
            ("--default-mode", self.default_mode.is_some()),
            ("--mode", !self.modes.is_empty()),
            ("--max-mode", self.max_mode.is_some()),
            ("--ram-floor-mib", self.ram_floor_mib.is_some()),
            ("--gpu-floor-mib", self.gpu_floor_mib.is_some()),
            ("--margin", !self.margins.is_empty()),
            (
                "--guaranteed-hint-deadline-s",
                self.guaranteed_hint_deadline_s.is_some(),
            ),
            ("--set", !self.sets.is_empty()),
        ]
        .into_iter()
        .filter(|(_, set)| *set)
        .map(|(flag, _)| flag)
        .collect()
    }
}

/// What setup knows of this host, for consequence lines and floors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub cpu_millicores: u64,
    pub physical_cores: Option<u32>,
    pub ram_mib: u64,
    pub gpus: Vec<(String, u64)>,
}

impl Host {
    pub fn from_snapshot(snapshot: &crate::model::Snapshot) -> Self {
        Self {
            cpu_millicores: snapshot.cpu_capacity_millicores,
            physical_cores: snapshot.physical_cores,
            ram_mib: snapshot.total_ram_mib,
            gpus: snapshot
                .gpus
                .iter()
                .map(|gpu| (gpu.uuid.clone(), gpu.total_memory_mib))
                .collect(),
        }
    }

    /// One read-only sample; `None` when the host cannot be read.
    pub fn sample(config: &Config) -> Option<Self> {
        let mut collector = crate::telemetry::Collector::for_config(config).ok()?;
        let snapshot = collector.sample().ok()?;
        Some(Self::from_snapshot(&snapshot))
    }

    fn gpu_totals(&self) -> Vec<u64> {
        self.gpus.iter().map(|(_, total)| *total).collect()
    }
}

fn caution_needed(config: &Config, host: Option<&Host>) -> Vec<CautionDetail> {
    match host {
        Some(host) => config.caution_needed(Some(host.ram_mib), &host.gpu_totals()),
        None => config.caution_needed(None, &[]),
    }
}

use crate::explain::group;

/// The configuration file and what it holds now.
#[derive(Debug, Clone)]
pub struct Target {
    pub path: PathBuf,
    pub original: Option<String>,
    pub role: Option<Role>,
}

/// The absolute file location, its text and the role it already has.
pub fn locate(path: &Path) -> Result<Target> {
    if path.exists() {
        let path = path
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", path.display()))?;
        let original = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let role = Some(detect_role(&original)?);
        return Ok(Target {
            path,
            original: Some(original),
            role,
        });
    }
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let directory = parent.canonicalize().map_err(|_| {
        invalid(format!(
            "the directory of {} does not exist; nothing was written",
            path.display()
        ))
    })?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid(format!("{} names no file", path.display())))?;
    Ok(Target {
        path: directory.join(name),
        original: None,
        role: None,
    })
}

fn detect_role(text: &str) -> Result<Role> {
    let document: DocumentMut = text
        .parse()
        .map_err(|error| invalid(format!("the current configuration is not TOML: {error}")))?;
    if document.contains_key("endpoint") {
        Ok(Role::Client)
    } else if document.contains_key("listen") {
        Ok(Role::Coordinator)
    } else if document.contains_key("coordinator_url") {
        Err(argument(
            "setup manages node, coordinator and client configurations, not agent ones",
        ))
    } else {
        Ok(Role::Node)
    }
}

/// The role this run writes, after checking it against the file and the flags.
pub fn resolve_role(target: &Target, choices: &Choices) -> Result<Role> {
    let role = match (target.role, choices.role) {
        (Some(file), Some(chosen)) if file != chosen => {
            return Err(argument(format!(
                "{} is a {} configuration, not a {} one",
                target.path.display(),
                file.name(),
                chosen.name()
            )));
        }
        (Some(role), _) | (None, Some(role)) => role,
        (None, None) => {
            return Err(missing(
                "a new configuration needs --role node, coordinator or client",
            ));
        }
    };
    if cfg!(windows) && role != Role::Client {
        return Err(fail(
            5,
            "ERR_CEDEGRID_UNSUPPORTED_PLATFORM",
            "on Windows setup configures client settings only; node and coordinator choices need Linux or macOS",
        ));
    }
    let node_flags = choices.node_flags();
    if role != Role::Node && (!node_flags.is_empty() || !choices.acknowledge.is_empty()) {
        let mut flags = node_flags;
        if !choices.acknowledge.is_empty() {
            flags.push("--acknowledge-caution");
        }
        return Err(argument(format!(
            "{} apply to node configurations only",
            flags.join(", ")
        )));
    }
    let allowed = ConnectionKey::for_role(role);
    if let Some(key) = choices.connection.keys().find(|key| !allowed.contains(key)) {
        return Err(argument(format!(
            "{} does not apply to a {} configuration",
            key.flag(),
            role.name()
        )));
    }
    Ok(role)
}

/// The file setup would write, with what changes and what it reports.
#[derive(Debug, Clone)]
pub struct Plan {
    pub role: Role,
    pub path: PathBuf,
    pub original: Option<String>,
    pub text: String,
    /// Each edited key, dotted, and its new value; `null` for a removed key.
    pub changes: BTreeMap<String, serde_json::Value>,
    pub exceptions: Vec<ExceptionNotice>,
    pub cautions: Vec<CautionDetail>,
    pub config: Option<Config>,
}

impl Plan {
    pub fn changed(&self) -> bool {
        self.original.as_deref() != Some(self.text.as_str())
    }
}

pub fn plan(path: &Path, choices: &Choices, host: Option<&Host>) -> Result<Plan> {
    let target = locate(path)?;
    let role = resolve_role(&target, choices)?;
    match role {
        Role::Node => plan_node(target, choices, host),
        role => plan_connection(target, role, choices),
    }
}

fn new_file() -> String {
    format!("config_version = {CONFIG_VERSION}\n")
}

/// The current configuration a node plan starts from.
fn node_base(target: &Target) -> Result<(String, Config)> {
    let text = target.original.clone().unwrap_or_else(new_file);
    let config = Config::parse_reporting(&text, &target.path)
        .map_err(|error| {
            invalid(format!(
                "the current configuration does not load, so setup cannot start from it: {}",
                load_error(error)
            ))
        })?
        .config;
    Ok((text, config))
}

fn plan_node(target: Target, choices: &Choices, host: Option<&Host>) -> Result<Plan> {
    let (text, base) = node_base(&target)?;
    let mut document: DocumentMut = text
        .parse()
        .map_err(|error| invalid(format!("the current configuration is not TOML: {error}")))?;
    let mut changes = BTreeMap::new();
    edit_node(&mut document, &base, choices, &mut changes)?;
    // Which caution options the result calls for, judged with every
    // acknowledgement present so validation reaches the other settings.
    let mut probe = document.clone();
    let all: BTreeSet<String> = ACKNOWLEDGEMENTS.iter().map(|s| s.to_string()).collect();
    set_acknowledgements(&mut probe, &BTreeSet::new(), &all, &mut BTreeMap::new())?;
    let candidate = Config::parse_reporting(&probe.to_string(), &target.path)
        .map_err(load_error)?
        .config;
    let needed = caution_needed(&candidate, host);
    let before = caution_needed(&base, host);
    let held = &base.contention.acknowledge_caution;
    // An option needs a fresh acknowledgement when this run brings it or changes
    // what it covers; one the file already carried unacknowledged stays so.
    let fresh: Vec<&CautionDetail> = needed
        .iter()
        .filter(|detail| !before.contains(detail) && !held.contains(&detail.name))
        .collect();
    let lacking: Vec<String> = fresh
        .iter()
        .filter(|detail| !choices.acknowledge.contains(&detail.name))
        .map(|detail| {
            let settings: Vec<String> = detail
                .settings
                .iter()
                .map(|setting| format!("{} = {}", setting.key, setting.value))
                .collect();
            format!(
                "{} needs --acknowledge-caution {}",
                settings.join(", "),
                detail.name
            )
        })
        .collect();
    if !lacking.is_empty() {
        return Err(unacknowledged(lacking.join("; ")));
    }
    let wanted: BTreeSet<String> = needed
        .iter()
        .map(|detail| detail.name.clone())
        .filter(|name| held.contains(name) || choices.acknowledge.contains(name))
        .collect();
    set_acknowledgements(&mut document, held, &wanted, &mut changes)?;
    let text = document.to_string();
    let config = Config::parse_reporting(&text, &target.path)
        .map_err(load_error)?
        .config;
    let cautions: Vec<CautionDetail> = needed
        .into_iter()
        .filter(|detail| wanted.contains(&detail.name))
        .collect();
    let exceptions = cautions
        .iter()
        .map(|detail| ExceptionNotice {
            name: detail.name.clone(),
            marker: RiskMarker::Caution,
        })
        .collect();
    Ok(Plan {
        role: Role::Node,
        path: target.path,
        original: target.original,
        text,
        changes,
        exceptions,
        cautions,
        config: Some(config),
    })
}

/// The standard table at `path`, created implicit when missing.
fn table_at<'a>(document: &'a mut DocumentMut, path: &[&str]) -> Result<&'a mut Table> {
    let mut table = document.as_table_mut();
    for (depth, segment) in path.iter().enumerate() {
        let item = table.entry(segment).or_insert_with(|| {
            let mut created = Table::new();
            created.set_implicit(true);
            Item::Table(created)
        });
        table = item.as_table_mut().ok_or_else(|| {
            invalid(format!(
                "setup edits [{}] only as a standard table",
                path[..=depth].join(".")
            ))
        })?;
    }
    Ok(table)
}

fn dotted(path: &[&str], key: &str) -> String {
    path.iter()
        .copied()
        .chain(std::iter::once(key))
        .collect::<Vec<_>>()
        .join(".")
}

fn json_of(value: &toml_edit::Value) -> serde_json::Value {
    match value {
        toml_edit::Value::String(text) => serde_json::Value::String(text.value().clone()),
        toml_edit::Value::Integer(number) => serde_json::Value::from(*number.value()),
        toml_edit::Value::Float(number) => serde_json::Value::from(*number.value()),
        toml_edit::Value::Boolean(flag) => serde_json::Value::Bool(*flag.value()),
        toml_edit::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(json_of).collect())
        }
        other => serde_json::Value::String(other.to_string().trim().to_string()),
    }
}

/// Set `key`, keeping the key's comments and the value's inline comment.
fn set(
    document: &mut DocumentMut,
    path: &[&str],
    key: &str,
    mut value: toml_edit::Value,
    changes: &mut BTreeMap<String, serde_json::Value>,
) -> Result<()> {
    changes.insert(dotted(path, key), json_of(&value));
    let table = table_at(document, path)?;
    match table.get_mut(key) {
        Some(item) => {
            if let Some(old) = item.as_value() {
                *value.decor_mut() = old.decor().clone();
            }
            *item = Item::Value(value);
        }
        None => {
            table.insert(key, Item::Value(value));
        }
    }
    Ok(())
}

fn remove(
    document: &mut DocumentMut,
    path: &[&str],
    key: &str,
    changes: &mut BTreeMap<String, serde_json::Value>,
) -> Result<()> {
    changes.insert(dotted(path, key), serde_json::Value::Null);
    table_at(document, path)?.remove(key);
    Ok(())
}

fn integer(value: u64) -> Result<toml_edit::Value> {
    Ok(toml_edit::Value::from(
        i64::try_from(value).map_err(|_| argument(format!("{value} is too large")))?,
    ))
}

fn margin_of(thresholds: &ResourceThresholds, mode: ContentionMode) -> (&'static str, u8) {
    let key = match mode {
        ContentionMode::YieldFirst => "yield_first_margin_percent",
        ContentionMode::Neutral => "neutral_margin_percent",
        ContentionMode::Performance => "performance_margin_percent",
        // Never written: callers iterate `ContentionMode::MARGINED`; the margin is fixed at 0%.
        ContentionMode::Aggressive => "aggressive_margin_percent",
    };
    (key, thresholds.margin_percent(mode))
}

/// A resource setting as the configuration resolves it now.
fn effective(base: &Config, kind: ResourceKind, key: &str) -> Option<serde_json::Value> {
    let written = serde_json::to_value(base.contention.resource(kind)).ok()?;
    if let Some(value) = written.get(key) {
        return Some(value.clone());
    }
    serde_json::to_value(base.contention.thresholds(kind))
        .ok()?
        .get(key)
        .cloned()
}

/// Apply node choices in one fixed order, each only where it differs from the
/// current value.
fn edit_node(
    document: &mut DocumentMut,
    base: &Config,
    choices: &Choices,
    changes: &mut BTreeMap<String, serde_json::Value>,
) -> Result<()> {
    let contention = base.contention.clone();
    if let Some(mode) = choices.default_mode
        && mode != contention.default_mode
    {
        set(
            document,
            &["contention"],
            "default_mode",
            mode.name().into(),
            changes,
        )?;
    }
    if let Some(cap) = choices.max_mode
        && cap != contention.max_mode
    {
        match cap {
            Some(mode) => set(
                document,
                &["contention"],
                "max_mode",
                mode.name().into(),
                changes,
            )?,
            None => remove(document, &["contention"], "max_mode", changes)?,
        }
    }
    if let Some(seconds) = choices.guaranteed_hint_deadline_s
        && seconds != contention.guaranteed_hint_deadline_s
    {
        set(
            document,
            &["contention"],
            "guaranteed_hint_deadline_s",
            integer(seconds)?,
            changes,
        )?;
    }
    for kind in ResourceKind::ALL {
        let section = ["contention", kind.name()];
        if let Some(mode) = choices.modes.get(&kind)
            && *mode != contention.resource(kind).mode
        {
            match mode {
                Some(mode) => set(document, &section, "mode", mode.name().into(), changes)?,
                None => remove(document, &section, "mode", changes)?,
            }
        }
        let thresholds = contention.thresholds(kind);
        for mode in ContentionMode::MARGINED {
            let (key, current) = margin_of(&thresholds, mode);
            if let Some(percent) = choices.margins.get(&(kind, mode))
                && *percent != current
            {
                set(
                    document,
                    &section,
                    key,
                    integer(u64::from(*percent))?,
                    changes,
                )?;
            }
        }
        for ((set_kind, key), literal) in &choices.sets {
            if *set_kind != kind {
                continue;
            }
            let value: toml_edit::Value = literal
                .parse()
                .map_err(|_| argument(format!("--set {key} takes a TOML value")))?;
            if effective(base, kind, key).as_ref() != Some(&json_of(&value)) {
                set(document, &section, key, value, changes)?;
            }
        }
    }
    if let Some(mib) = choices.ram_floor_mib
        && mib != base.ram.reserve_mib
    {
        set(document, &["ram"], "reserve_mib", integer(mib)?, changes)?;
    }
    if let Some(mib) = choices.gpu_floor_mib
        && mib != base.gpu.reserve_vram_mib
    {
        set(
            document,
            &["gpu"],
            "reserve_vram_mib",
            integer(mib)?,
            changes,
        )?;
    }
    Ok(())
}

fn set_acknowledgements(
    document: &mut DocumentMut,
    held: &BTreeSet<String>,
    wanted: &BTreeSet<String>,
    changes: &mut BTreeMap<String, serde_json::Value>,
) -> Result<()> {
    if held == wanted {
        return Ok(());
    }
    if wanted.is_empty() {
        return remove(document, &["contention"], "acknowledge_caution", changes);
    }
    let mut array: toml_edit::Array = wanted.iter().map(String::as_str).collect();
    array.fmt();
    set(
        document,
        &["contention"],
        "acknowledge_caution",
        toml_edit::Value::Array(array),
        changes,
    )
}

fn plan_connection(target: Target, role: Role, choices: &Choices) -> Result<Plan> {
    let fresh = target.original.is_none();
    if fresh
        && let Some(key) = ConnectionKey::for_role(role)
            .iter()
            .find(|key| !choices.connection.contains_key(key))
    {
        return Err(missing(format!(
            "a new {} configuration needs {}",
            role.name(),
            key.flag()
        )));
    }
    let text = target.original.clone().unwrap_or_else(new_file);
    let mut document: DocumentMut = text
        .parse()
        .map_err(|error| invalid(format!("the current configuration is not TOML: {error}")))?;
    let mut changes = BTreeMap::new();
    for key in ConnectionKey::for_role(role) {
        let Some(value) = choices.connection.get(key) else {
            continue;
        };
        if *key == ConnectionKey::OperatorFingerprint {
            // An existing entry for the fingerprint is kept as it is.
            let fingerprint = value.to_ascii_lowercase();
            if !table_at(&mut document, &["clients"])?.contains_key(&fingerprint) {
                set(
                    &mut document,
                    &["clients", &fingerprint],
                    "role",
                    "operator".into(),
                    &mut changes,
                )?;
                table_at(&mut document, &["clients", &fingerprint])?.set_implicit(false);
            }
            continue;
        }
        let (path, name) = key.path();
        let current = table_at(&mut document, path)?
            .get(name)
            .and_then(Item::as_str)
            .map(str::to_string);
        if current.as_deref() != Some(value.as_str()) {
            set(
                &mut document,
                path,
                name,
                value.as_str().into(),
                &mut changes,
            )?;
        }
    }
    let text = document.to_string();
    let kind = match role {
        Role::Client => crate::config::RuntimeConfigKind::Client,
        _ => crate::config::RuntimeConfigKind::Coordinator,
    };
    crate::config::parse_runtime::<serde_json::Value>(&text, &target.path, kind)
        .map_err(load_error)?;
    Ok(Plan {
        role,
        path: target.path,
        original: target.original,
        text,
        changes,
        exceptions: Vec::new(),
        cautions: Vec::new(),
        config: None,
    })
}

/// Write `text` next to `path`, fsync it, and rename it over `path`.
pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let directory = path.parent().context("configuration has no directory")?;
    let name = path
        .file_name()
        .context("configuration names no file")?
        .to_string_lossy();
    let temporary = directory.join(format!(".{name}.setup-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("cannot create {}", temporary.display()))?;
        if let Ok(metadata) = std::fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
            .with_context(|| format!("cannot replace {}", path.display()))?;
        #[cfg(unix)]
        std::fs::File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        // Only the temporary file this run created is removed.
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// What a run reports (contracts/setup-cli.md).
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub written: bool,
    pub path: PathBuf,
    pub choices: BTreeMap<String, serde_json::Value>,
    pub exceptions: Vec<ExceptionNotice>,
}

/// Run setup and print its result; returns the exit status.
pub fn main(config: &Path, args: &SetupArgs) -> i32 {
    if args.allow_unprotected || args.disallow_unprotected {
        return match allowance(config, args) {
            Ok(change) => {
                if args.json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "unprotected": if change.on() { "on" } else { "off" },
                            "changed": change.changed(),
                        })
                    );
                } else if let Some(text) = change.text() {
                    let style = Style::detect();
                    println!("{}", text.0);
                    if let Some(note) = text.1 {
                        println!("{}", style.dim(&note));
                    }
                }
                0
            }
            Err(error) => report_error(&error, args.json),
        };
    }
    match run(config, args) {
        Ok(report) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string(&report).unwrap_or_else(|_| "{}".into())
                );
            } else {
                print_report(&report);
            }
            0
        }
        Err(error) => report_error(&error, args.json),
    }
}

/// Prints a setup error, as JSON on standard output or for people on standard error, and
/// returns its exit status.
fn report_error(error: &anyhow::Error, json: bool) -> i32 {
    let (exit, code, message) = match error.downcast_ref::<SetupError>() {
        Some(error) => (error.exit, error.code, error.message.clone()),
        None => (1, "ERR_CEDEGRID_SETUP", format!("{error:#}")),
    };
    if json {
        println!(
            "{}",
            serde_json::json!({"error": {"code": code, "message": message}})
        );
    } else {
        for line in error_lines(code, &message, Style::detect_stderr()) {
            eprintln!("{line}");
        }
    }
    exit
}

fn print_report(report: &Report) {
    if report.written {
        println!("wrote {}", report.path.display());
    } else {
        println!("{} unchanged", report.path.display());
    }
    for (key, value) in &report.choices {
        println!("  {key} = {value}");
    }
    for exception in &report.exceptions {
        println!("  [caution] {}", exception.name);
    }
}

pub fn run(config: &Path, args: &SetupArgs) -> Result<Report> {
    let mut choices = Choices::from_args(args)?;
    let interactive =
        !args.non_interactive && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let target = locate(config)?;
    let base = match (target.role, &target.original) {
        (Some(Role::Node), Some(_)) => node_base(&target).ok().map(|(_, config)| config),
        _ => None,
    };
    let host = if cfg!(windows) {
        None
    } else {
        Host::sample(&base.unwrap_or_default())
    };
    if interactive {
        choices = interface::run(&target, choices, host.as_ref())?.ok_or_else(cancelled)?;
    }
    let plan = plan(config, &choices, host.as_ref())?;
    let written = plan.changed();
    if written {
        write_atomic(&plan.path, &plan.text)?;
    }
    Ok(Report {
        written,
        path: plan.path,
        choices: plan.changes,
        exceptions: plan.exceptions,
    })
}

/// The fixed phrase that turns the allowance on (004 R6).
pub const ALLOWANCE_PHRASE: &str = "slow other users' compute on this host";

/// What the allowance's numbers screen shows (004 FR-013, R11).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllowanceNumbers {
    /// Other principals at or above 1% of the host's CPU; `None` when telemetry is unavailable.
    pub other_principals: Option<usize>,
    pub other_cpu_millicores: u64,
    pub capacity_millicores: u64,
    /// How unprotected can raise own CPU share here (006 FR-016).
    pub raise: crate::actuation::Raise,
}

impl AllowanceNumbers {
    pub fn from_snapshot(snapshot: &crate::model::Snapshot) -> Self {
        let floor = (snapshot.cpu_capacity_millicores / 100).max(1);
        let others: Vec<u64> = snapshot
            .principals
            .iter()
            .flatten()
            .filter(|principal| !principal.is_owner && principal.cpu_millicores >= floor)
            .map(|principal| principal.cpu_millicores)
            .collect();
        Self {
            other_principals: snapshot.principals.as_ref().map(|_| others.len()),
            other_cpu_millicores: others.iter().sum(),
            capacity_millicores: snapshot.cpu_capacity_millicores,
            raise: crate::actuation::Raise::current(),
        }
    }
}

/// What `--allow-unprotected` or `--disallow-unprotected` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllowanceChange {
    Allowed,
    AlreadyAllowed {
        since_unix_ms: u64,
    },
    Disallowed,
    AlreadyOff,
    /// The dialog was declined; it already said so.
    Cancelled,
}

impl AllowanceChange {
    fn on(self) -> bool {
        matches!(self, Self::Allowed | Self::AlreadyAllowed { .. })
    }

    fn changed(self) -> bool {
        matches!(self, Self::Allowed | Self::Disallowed)
    }

    /// The result line and an optional dim note; nothing after a cancel, which printed its own.
    fn text(self) -> Option<(String, Option<String>)> {
        let off = "Turn it off with: cedegrid setup --disallow-unprotected";
        Some(match self {
            Self::Allowed => (
                "Unprotected is now allowed on this host.".into(),
                Some(format!(
                    "  A run still needs both SDK fields to use it. {off}"
                )),
            ),
            Self::AlreadyAllowed { since_unix_ms } => (
                format!(
                    "Unprotected is already allowed on this host (since {}).",
                    utc_minute(since_unix_ms)
                ),
                Some(format!("  {off}")),
            ),
            Self::Disallowed => (
                "Unprotected is no longer allowed on this host.".into(),
                Some("  Runs already started keep their grant until they end.".into()),
            ),
            Self::AlreadyOff => (
                "Unprotected is not allowed on this host; nothing changed.".into(),
                None,
            ),
            Self::Cancelled => return None,
        })
    }
}

/// A Unix time in milliseconds as `YYYY-MM-DD HH:MM UTC`.
pub fn utc_minute(unix_ms: u64) -> String {
    let minutes = unix_ms / 60_000;
    let (days, minute) = (minutes / 1440, minutes % 1440);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        minute / 60,
        minute % 60
    )
}

/// The two steps on the given streams: the numbers and a yes/no question with no default, then
/// the fixed phrase. Returns whether both passed; anything else cancels with one line.
pub fn allowance_dialog(
    numbers: &AllowanceNumbers,
    style: Style,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<bool> {
    writeln!(
        output,
        "{}  {}",
        style.bold("Unprotected"),
        style.refusal(HIGHEST_WARNING)
    )?;
    writeln!(output)?;
    // No slowdown figure: the kernel's split depends on the run's threads and this host's cgroup
    // layout, which setup cannot know (004 FR-013).
    for line in wrap(
        "A run that asks for it starts compute work beside every other user, including CedeGrid \
         users that arrived first and yield, and raises its CPU share above other users' where \
         this host gives a way. Other users' compute can slow down until the run ends. It never \
         does either beside a CedeGrid user that arrived first in Performance. Memory floors and \
         OOM handling do not change.",
        76,
    ) {
        writeln!(output, "  {line}")?;
    }
    writeln!(output)?;
    let mut rows = match numbers.other_principals {
        Some(count) => vec![
            ("Other users active now", count.to_string()),
            (
                "Their CPU use",
                format!(
                    "{} m of {} m",
                    group(numbers.other_cpu_millicores),
                    group(numbers.capacity_millicores)
                ),
            ),
        ],
        None => vec![(
            "Other users active now",
            "unknown: no per-user telemetry on this host".to_string(),
        )],
    };
    rows.push(("Raise on this host", numbers.raise.describe()));
    for line in key_values(&rows, 2) {
        writeln!(output, "{line}")?;
    }
    writeln!(output)?;
    write!(output, "Allow unprotected on this host? [y/n] ")?;
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "yes") {
        writeln!(output, "Cancelled. The allowance stays off.")?;
        return Ok(false);
    }
    write!(output, "Type \"{ALLOWANCE_PHRASE}\" to confirm: ")?;
    output.flush()?;
    let mut phrase = String::new();
    input.read_line(&mut phrase)?;
    if phrase.trim_end_matches(['\r', '\n']) != ALLOWANCE_PHRASE {
        writeln!(
            output,
            "Cancelled: the phrase did not match. The allowance stays off."
        )?;
        return Ok(false);
    }
    Ok(true)
}

/// `setup --allow-unprotected` or `--disallow-unprotected`. Turning it on
/// needs a terminal; turning it off needs nothing.
fn allowance(config: &Path, args: &SetupArgs) -> Result<AllowanceChange> {
    let target = locate(config)?;
    let (_, node) = node_base(&target)?;
    let before = crate::state::read_allowance(&node.state_dir);
    if args.disallow_unprotected {
        crate::state::clear_allowance(&node.state_dir)?;
        return Ok(match before {
            crate::state::Allowance::On { .. } => AllowanceChange::Disallowed,
            crate::state::Allowance::Off { .. } => AllowanceChange::AlreadyOff,
        });
    }
    let interactive = !args.non_interactive
        && !args.json
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal();
    if !interactive {
        return Err(SetupError {
            exit: 2,
            code: "ERR_CEDEGRID_ALLOWANCE_INTERACTIVE_ONLY",
            message: "unprotected can be allowed only from a terminal".into(),
        }
        .into());
    }
    if let crate::state::Allowance::On { set_at_unix_ms } = before {
        return Ok(AllowanceChange::AlreadyAllowed {
            since_unix_ms: set_at_unix_ms,
        });
    }
    let numbers = crate::telemetry::Collector::for_config(&node)
        .and_then(|mut collector| collector.sample())
        .map(|snapshot| AllowanceNumbers::from_snapshot(&snapshot))
        .unwrap_or_default();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    if !allowance_dialog(&numbers, Style::detect(), &mut stdin.lock(), &mut stdout)? {
        return Ok(AllowanceChange::Cancelled);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("the clock is before 1970")?
        .as_millis();
    crate::state::set_allowance(&node.state_dir, u64::try_from(now)?)?;
    Ok(AllowanceChange::Allowed)
}

/// On a terminal, offer setup for a command whose configuration is missing
/// (FR-046). Returns whether a configuration was written. Never prompts without a
/// terminal.
pub fn offer_for_missing(path: &Path) -> Result<bool> {
    if path.exists() || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(false);
    }
    print!(
        "No configuration at {}. Start setup now? [y/N] ",
        path.display()
    );
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        return Ok(false);
    }
    let args = SetupArgs {
        role: Some(if cfg!(windows) {
            Role::Client
        } else {
            Role::Node
        }),
        ..SetupArgs::default()
    };
    Ok(main(path, &args) == 0 && path.exists())
}

/// One row of the interface: what it edits and its options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Field {
    Role,
    DefaultMode,
    Mode(ResourceKind),
    MaxMode,
    RamFloor,
    GpuFloor,
    Margin(ResourceKind, ContentionMode),
    HintDeadline,
    NoiseTolerance,
    AttributionTolerance,
}

/// A value a field can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Role(Role),
    Mode(ContentionMode),
    /// Follow the default mode, or no explicit cap.
    Inherit,
    Number(u64),
}

/// The node fields in interface order.
pub fn node_fields() -> Vec<Field> {
    let mut fields = vec![Field::Role, Field::DefaultMode];
    fields.extend(ResourceKind::ALL.map(Field::Mode));
    fields.extend([Field::MaxMode, Field::RamFloor, Field::GpuFloor]);
    for kind in ResourceKind::ALL {
        for mode in ContentionMode::MARGINED {
            // Memory keeps only the floor under Neutral and Performance.
            if !kind.is_memory() || mode == ContentionMode::YieldFirst {
                fields.push(Field::Margin(kind, mode));
            }
        }
    }
    fields.extend([
        Field::HintDeadline,
        Field::NoiseTolerance,
        Field::AttributionTolerance,
    ]);
    fields
}

impl Field {
    /// The dotted key a caution option lists for this field.
    pub fn key(self) -> String {
        match self {
            Field::Role => "role".into(),
            Field::DefaultMode => "contention.default_mode".into(),
            Field::Mode(kind) => format!("contention.{}.mode", kind.name()),
            Field::MaxMode => "contention.max_mode".into(),
            Field::RamFloor => "ram.reserve_mib".into(),
            Field::GpuFloor => "gpu.reserve_vram_mib".into(),
            Field::Margin(kind, mode) => format!(
                "contention.{}.{}",
                kind.name(),
                margin_of(&ResourceThresholds::defaults(kind), mode).0
            ),
            Field::HintDeadline => "contention.guaranteed_hint_deadline_s".into(),
            Field::NoiseTolerance => "contention.gpu_memory.noise_tolerance_mib".into(),
            Field::AttributionTolerance => {
                "contention.gpu_compute.attribution_tolerance_percent".into()
            }
        }
    }

    pub fn label(self) -> String {
        match self {
            Field::Role => "Role".into(),
            Field::DefaultMode => "Default mode".into(),
            Field::Mode(kind) => format!("{} mode", resource_label(kind)),
            Field::MaxMode => "Node cap".into(),
            Field::RamFloor => "RAM floor".into(),
            Field::GpuFloor => "GPU memory floor".into(),
            Field::Margin(kind, mode) => {
                format!("{} margin, {}", resource_label(kind), mode.label())
            }
            Field::HintDeadline => "Guaranteed hint deadline".into(),
            Field::NoiseTolerance => "GPU memory noise tolerance".into(),
            Field::AttributionTolerance => "GPU compute attribution tolerance".into(),
        }
    }

    /// A line under the field's title, for fields whose options alone do not say what they
    /// cover.
    pub fn note(self) -> Option<&'static str> {
        match self {
            Field::DefaultMode => Some("Applies to each resource without a mode of its own."),
            Field::MaxMode => Some("The highest mode a job may request on this node."),
            _ => None,
        }
    }

    /// The field's value in a configuration.
    pub fn current(self, config: &Config) -> Pick {
        let contention = &config.contention;
        match self {
            Field::Role => Pick::Role(Role::Node),
            Field::DefaultMode => Pick::Mode(contention.default_mode),
            Field::Mode(kind) => contention
                .resource(kind)
                .mode
                .map_or(Pick::Inherit, Pick::Mode),
            Field::MaxMode => contention.max_mode.map_or(Pick::Inherit, Pick::Mode),
            Field::RamFloor => Pick::Number(config.ram.reserve_mib),
            Field::GpuFloor => Pick::Number(config.gpu.reserve_vram_mib),
            Field::Margin(kind, mode) => {
                Pick::Number(u64::from(margin_of(&contention.thresholds(kind), mode).1))
            }
            Field::HintDeadline => Pick::Number(contention.guaranteed_hint_deadline_s),
            Field::NoiseTolerance => Pick::Number(
                contention
                    .thresholds(ResourceKind::GpuMemory)
                    .noise_tolerance_mib,
            ),
            Field::AttributionTolerance => Pick::Number(u64::from(
                contention
                    .thresholds(ResourceKind::GpuCompute)
                    .attribution_tolerance_percent,
            )),
        }
    }

    /// The options shown for the field, including its current value.
    pub fn options(self, config: &Config, fresh: bool) -> Vec<Pick> {
        // Aggressive is offered too; choosing it, or a cap at it, asks for `aggressive_mode`
        // through the caution step (004 FR-007, FR-008).
        let modes = ContentionMode::ALL.map(Pick::Mode);
        let numbers = |mut values: Vec<u64>, ascending: bool| -> Vec<Pick> {
            if let Pick::Number(current) = self.current(config) {
                values.push(current);
            }
            values.sort_unstable();
            values.dedup();
            if !ascending {
                values.reverse();
            }
            values.into_iter().map(Pick::Number).collect()
        };
        match self {
            Field::Role if fresh => vec![
                Pick::Role(Role::Node),
                Pick::Role(Role::Coordinator),
                Pick::Role(Role::Client),
            ],
            Field::Role => vec![Pick::Role(Role::Node)],
            Field::DefaultMode => modes.to_vec(),
            Field::Mode(_) => std::iter::once(Pick::Inherit).chain(modes).collect(),
            // A cap below a configured mode does not load, so it is not offered.
            Field::MaxMode => {
                let highest = ResourceKind::ALL
                    .into_iter()
                    .map(|kind| config.contention.mode(kind))
                    .max()
                    .unwrap_or(ContentionMode::Neutral);
                std::iter::once(Pick::Inherit)
                    .chain(
                        ContentionMode::ALL
                            .into_iter()
                            .filter(|mode| *mode >= highest)
                            .map(Pick::Mode),
                    )
                    .collect()
            }
            Field::RamFloor => {
                numbers(vec![512, 768, 1024, 2048, 4096, 8192, 16_384, 32_768], true)
            }
            Field::GpuFloor => numbers(vec![512, 768, 1024, 2048, 3072, 4096, 8192], true),
            Field::Margin(kind, mode) => {
                let default = u64::from(margin_of(&ResourceThresholds::defaults(kind), mode).1);
                let lower = default - default.div_ceil(3);
                numbers(
                    [
                        default * 2,
                        default + default / 2,
                        default,
                        lower,
                        default / 2,
                    ]
                    .into_iter()
                    .filter(|value| (1..=90).contains(value))
                    .collect(),
                    false,
                )
            }
            Field::HintDeadline => numbers(vec![2, 5, 10, 15, 30], true),
            Field::NoiseTolerance => numbers(vec![128, 256, 512, 1024], true),
            Field::AttributionTolerance => numbers(vec![5, 10, 15, 20, 30], true),
        }
    }

    /// Record `pick` in `choices`.
    pub fn apply(self, choices: &mut Choices, pick: Pick) {
        let number = match pick {
            Pick::Number(value) => Some(value),
            _ => None,
        };
        let mode = match pick {
            Pick::Mode(mode) => Some(mode),
            _ => None,
        };
        match self {
            Field::Role => {
                if let Pick::Role(role) = pick {
                    choices.role = Some(role);
                }
            }
            Field::DefaultMode => choices.default_mode = mode.or(choices.default_mode),
            Field::Mode(kind) => {
                choices.modes.insert(kind, mode);
            }
            Field::MaxMode => choices.max_mode = Some(mode),
            Field::RamFloor => choices.ram_floor_mib = number,
            Field::GpuFloor => choices.gpu_floor_mib = number,
            Field::Margin(kind, margin) => {
                if let Some(value) = number.and_then(|value| u8::try_from(value).ok()) {
                    choices.margins.insert((kind, margin), value);
                }
            }
            Field::HintDeadline => choices.guaranteed_hint_deadline_s = number,
            Field::NoiseTolerance => {
                if let Some(value) = number {
                    choices.sets.insert(
                        (ResourceKind::GpuMemory, "noise_tolerance_mib".into()),
                        value.to_string(),
                    );
                }
            }
            Field::AttributionTolerance => {
                if let Some(value) = number {
                    choices.sets.insert(
                        (
                            ResourceKind::GpuCompute,
                            "attribution_tolerance_percent".into(),
                        ),
                        value.to_string(),
                    );
                }
            }
        }
    }

    /// The pick as the main screen shows it.
    pub fn show(self, pick: Pick, config: &Config) -> String {
        match (self, pick) {
            (_, Pick::Role(role)) => role.name().into(),
            (_, Pick::Mode(mode)) => mode.label().into(),
            (Field::Mode(_), Pick::Inherit) => {
                format!(
                    "same as default ({})",
                    config.contention.default_mode.label()
                )
            }
            (_, Pick::Inherit) => "no explicit cap".into(),
            (Field::RamFloor | Field::GpuFloor | Field::NoiseTolerance, Pick::Number(value)) => {
                format!("{} MiB", group(value))
            }
            (Field::Margin(..) | Field::AttributionTolerance, Pick::Number(value)) => {
                format!("{value}%")
            }
            (Field::HintDeadline, Pick::Number(value)) => format!("{value} s"),
            (_, Pick::Number(value)) => value.to_string(),
        }
    }
}

/// The caution option `pick` on `field` makes active, if any: whether the
/// resulting configuration lists the field's key under an option (FR-045).
pub fn risk(
    target: &Target,
    field: Field,
    pick: Pick,
    host: Option<&Host>,
) -> Option<&'static str> {
    if field == Field::Role {
        return None;
    }
    let (text, base) = node_base(target).ok()?;
    let mut choices = Choices::default();
    field.apply(&mut choices, pick);
    let mut document: DocumentMut = text.parse().ok()?;
    let mut changes = BTreeMap::new();
    edit_node(&mut document, &base, &choices, &mut changes).ok()?;
    let all: BTreeSet<String> = ACKNOWLEDGEMENTS.iter().map(|s| s.to_string()).collect();
    set_acknowledgements(&mut document, &BTreeSet::new(), &all, &mut changes).ok()?;
    let config = Config::parse_reporting(&document.to_string(), &target.path)
        .ok()?
        .config;
    let key = field.key();
    caution_needed(&config, host)
        .into_iter()
        .find(|detail| detail.settings.iter().any(|setting| setting.key == key))
        .and_then(|detail| {
            ACKNOWLEDGEMENTS
                .into_iter()
                .find(|name| *name == detail.name)
        })
}

/// The one-line consequence of `pick` on this host.
pub fn consequence(field: Field, pick: Pick, config: &Config, host: Option<&Host>) -> String {
    let cpu = host.map(|host| host.cpu_millicores);
    let cpu_free = |percent: u64| -> String {
        match cpu {
            Some(total) => {
                let reserve = match host.and_then(|host| host.physical_cores) {
                    Some(cores) if cores > 0 => {
                        u64::from(config.cpu.reserve_physical_cores) * total / u64::from(cores)
                    }
                    _ => 0,
                };
                let free = (total * percent / 100).max(reserve);
                format!("keeps {} m of {} m CPU free", group(free), group(total))
            }
            None => format!("keeps {percent}% of the CPU free"),
        }
    };
    let ram_floor = |config: &Config| match host {
        Some(host) => format!(
            "RAM floor {} MiB on this host",
            group(config.ram_floor_mib(host.ram_mib))
        ),
        None => format!("RAM floor at least {} MiB", group(config.ram.reserve_mib)),
    };
    let gpu_floor = |config: &Config| match host.and_then(|host| host.gpus.first()) {
        Some((uuid, total)) => format!(
            "GPU memory floor {} MiB on {uuid}",
            group(config.vram_floor_mib(*total))
        ),
        None => format!(
            "no GPU on this host; at least {} MiB on each GPU",
            group(config.gpu.reserve_vram_mib)
        ),
    };
    let behavior = |mode: ContentionMode| match mode {
        ContentionMode::YieldFirst => "pauses under contention",
        ContentionMode::Neutral => "holds the fair share under contention",
        ContentionMode::Performance => "follows what external work takes",
        ContentionMode::Aggressive => "fills idle compute at once",
    };
    let margin = |kind: ResourceKind, mode: ContentionMode| -> u64 {
        u64::from(margin_of(&config.contention.thresholds(kind), mode).1)
    };
    let resource_line_of = |kind: ResourceKind, mode: ContentionMode| -> String {
        match kind {
            ResourceKind::Cpu => format!("{} · {}", cpu_free(margin(kind, mode)), behavior(mode)),
            ResourceKind::Ram if mode == ContentionMode::YieldFirst => format!(
                "keeps {}% beyond the floor free; {}",
                margin(kind, mode),
                ram_floor(config)
            ),
            ResourceKind::Ram => match host {
                Some(host) => format!(
                    "keeps the floor only ({} MiB on this host)",
                    group(config.ram_floor_mib(host.ram_mib))
                ),
                None => "keeps the floor only".into(),
            },
            ResourceKind::GpuCompute => format!(
                "keeps {}% of each GPU's compute free · {}",
                margin(kind, mode),
                behavior(mode)
            ),
            ResourceKind::GpuMemory if mode == ContentionMode::YieldFirst => format!(
                "keeps {}% beyond the floor free; {}",
                margin(kind, mode),
                gpu_floor(config)
            ),
            ResourceKind::GpuMemory => match host.map(|host| host.gpus.first()) {
                Some(Some((uuid, total))) => format!(
                    "keeps the floor only ({} MiB on {uuid})",
                    group(config.vram_floor_mib(*total))
                ),
                Some(None) => "keeps the floor only (no GPU on this host)".into(),
                None => "keeps the floor only".into(),
            },
        }
    };
    // Memory follows Performance under Aggressive (004 FR-006).
    let resource_line = |kind: ResourceKind, mode: ContentionMode| -> String {
        if kind.is_memory() && mode == ContentionMode::Aggressive {
            format!(
                "same as Performance on memory: {}",
                resource_line_of(kind, ContentionMode::Performance)
            )
        } else {
            resource_line_of(kind, mode)
        }
    };
    let with = |apply: &dyn Fn(&mut Config)| {
        let mut changed = config.clone();
        apply(&mut changed);
        changed
    };
    match (field, pick) {
        (Field::Role, Pick::Role(Role::Node)) => {
            "runs work on this host; modes, floors and margins apply".into()
        }
        (Field::Role, Pick::Role(Role::Coordinator)) => {
            "schedules work for nodes; asks for its listen address and TLS files".into()
        }
        (Field::Role, Pick::Role(_)) => {
            "submits and inspects work; asks for the endpoint and TLS files".into()
        }
        (Field::DefaultMode, Pick::Mode(mode)) => behavior(mode).into(),
        (Field::Mode(kind), Pick::Mode(mode)) => resource_line(kind, mode),
        (Field::Mode(kind), _) => format!(
            "follows the default mode: {}",
            resource_line(kind, config.contention.default_mode)
        ),
        (Field::MaxMode, Pick::Mode(mode)) => {
            format!("jobs may request at most {}", mode.label())
        }
        (Field::MaxMode, _) => format!(
            "jobs may request at most the most aggressive configured mode ({})",
            ResourceKind::ALL
                .into_iter()
                .map(|kind| config.contention.mode(kind))
                .max()
                .unwrap_or(ContentionMode::Neutral)
                .label()
        ),
        (Field::RamFloor, Pick::Number(mib)) => ram_floor(&with(&|c| c.ram.reserve_mib = mib)),
        (Field::GpuFloor, Pick::Number(mib)) => gpu_floor(&with(&|c| c.gpu.reserve_vram_mib = mib)),
        (Field::Margin(kind, mode), Pick::Number(percent)) => match kind {
            ResourceKind::Cpu => format!("{} in {}", cpu_free(percent), mode.label()),
            ResourceKind::GpuCompute => format!(
                "keeps {percent}% of each GPU's compute free in {}",
                mode.label()
            ),
            _ => format!(
                "keeps {percent}% of {} beyond the floor free in Yield First",
                resource_label(kind)
            ),
        },
        (Field::HintDeadline, Pick::Number(seconds)) => format!(
            "a hint-ready Guaranteed allocation gets {seconds} s to release memory before it may be drained"
        ),
        (Field::NoiseTolerance, Pick::Number(mib)) => {
            format!("external GPU memory changes under {mib} MiB count as noise")
        }
        (Field::AttributionTolerance, Pick::Number(points)) => format!(
            "per-process GPU samples within {points} points of device use count as measured"
        ),
        _ => String::new(),
    }
}

/// Why each caution option is marked, restated on the confirmation screen
/// (FR-045).
pub fn justification(option: &str) -> &'static str {
    match option {
        "small_floor" => {
            "Less headroom is kept for other workloads' bursts. The enforced minimum and the out-of-memory preference still protect them."
        }
        "reduced_margin" => {
            "Own work leaves less idle capacity for external demand to grow into. Contention handling and floors still hold."
        }
        "aggressive_mode" => {
            "Own work is admitted to all idle CPU and GPU compute at once, with no margin. New work still starts only where others keep their fair share, and floors still hold."
        }
        _ => {
            "Contention and external growth are recognized later or forgotten sooner, so own work holds capacity longer. Floors still hold."
        }
    }
}

/// The summary's per-resource lines on this host.
pub fn host_lines(config: &Config, host: Option<&Host>) -> Vec<String> {
    let mut lines = Vec::new();
    for kind in ResourceKind::ALL {
        let mode = crate::explain::behaves_as(kind, config.contention.mode(kind));
        lines.push(format!(
            "{:<12} {:<12} {}",
            resource_label(kind),
            mode.label(),
            capitalize(&consequence(
                Field::Mode(kind),
                Pick::Mode(mode),
                config,
                host
            ))
        ));
    }
    if let Some(host) = host {
        for (uuid, total) in host.gpus.iter().skip(1) {
            lines.push(format!(
                "{:<25} GPU memory floor {} MiB on {uuid}",
                "",
                group(config.vram_floor_mib(*total))
            ));
        }
    }
    lines
}

/// The summary shown before writing (FR-053).
pub fn summary_lines(plan: &Plan, host: Option<&Host>) -> Vec<String> {
    let mut lines = vec![format!("{} ({})", plan.path.display(), plan.role.name())];
    lines.push(String::new());
    if plan.changes.is_empty() && plan.original.is_some() {
        lines.push("No changes.".into());
    } else {
        lines.push("Changes".into());
        if plan.changes.is_empty() {
            lines.push(format!("  new file with config_version = {CONFIG_VERSION}"));
        }
        for (key, value) in &plan.changes {
            if value.is_null() {
                lines.push(format!("  {key} removed"));
            } else {
                lines.push(format!("  {key} = {value}"));
            }
        }
    }
    if let Some(config) = &plan.config {
        lines.push(String::new());
        lines.push("On this host".into());
        lines.extend(
            host_lines(config, host)
                .into_iter()
                .map(|line| format!("  {line}")),
        );
        lines.push(String::new());
        lines.push("Caution options".into());
        if plan.cautions.is_empty() {
            lines.push("  none".into());
        }
        for detail in &plan.cautions {
            let settings: Vec<String> = detail
                .settings
                .iter()
                .map(|setting| format!("{} = {}", setting.key, setting.value))
                .collect();
            lines.push(format!(
                "  {CAUTION} {}: {}",
                detail.name,
                settings.join(", ")
            ));
        }
    }
    lines
}

/// The marker text; yellow (SGR 33) is added where color is allowed.
pub const CAUTION: &str = "[caution]";
/// The label for unprotected, the one option above caution (006 FR-015).
pub const HIGHEST_WARNING: &str = "[highest warning]";

/// Color for setup's human output (FR-050): yellow for caution, red for a refusal and dim for
/// descriptions, only on a terminal that allows color; plain text otherwise.
#[derive(Clone, Copy, Debug)]
pub struct Style {
    pub color: bool,
}

impl Style {
    /// Color on standard output when it is a terminal, unless `NO_COLOR` is set or `TERM` is
    /// `dumb`.
    pub fn detect() -> Self {
        Self::on(std::io::stdout().is_terminal())
    }

    /// The same rule for standard error.
    pub fn detect_stderr() -> Self {
        Self::on(std::io::stderr().is_terminal())
    }

    fn on(terminal: bool) -> Self {
        Self {
            color: terminal
                && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
                && std::env::var("TERM").map_or(true, |term| term != "dumb"),
        }
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.color && !text.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn caution(self, text: &str) -> String {
        self.paint("33", text)
    }

    pub fn refusal(self, text: &str) -> String {
        self.paint("1;31", text)
    }

    pub fn dim(self, text: &str) -> String {
        self.paint("2", text)
    }

    pub fn bold(self, text: &str) -> String {
        self.paint("1", text)
    }
}

/// One option in a list: a fixed-width prefix (number or pointer), the label, its description
/// and whether it is a caution option.
pub struct OptionRow {
    pub prefix: String,
    pub label: String,
    pub description: String,
    pub caution: bool,
    pub selected: bool,
}

/// Option rows with the labels in one aligned column and dim descriptions beside them; when
/// `width` cannot hold both, each description goes on its own line under the label.
pub fn option_lines(rows: &[OptionRow], style: Style, width: Option<usize>) -> Vec<String> {
    let count = |text: &str| text.chars().count();
    let label_width = rows.iter().map(|row| count(&row.label)).max().unwrap_or(0);
    let prefix_width = rows.iter().map(|row| count(&row.prefix)).max().unwrap_or(0);
    let tag = |caution: bool| {
        if caution {
            format!("  {}", style.caution(CAUTION))
        } else {
            String::new()
        }
    };
    let widest = rows
        .iter()
        .map(|row| count(&row.description) + if row.caution { 11 } else { 0 })
        .max()
        .unwrap_or(0);
    let beside = width.is_none_or(|width| prefix_width + label_width + 2 + widest <= width);
    let mut lines = Vec::new();
    for row in rows {
        let paint = |label: String| {
            if row.selected {
                style.bold(&label)
            } else {
                label
            }
        };
        let description = style.dim(&capitalize(&row.description));
        if beside {
            lines.push(format!(
                "{:<prefix_width$}{}  {description}{}",
                row.prefix,
                paint(format!("{:<label_width$}", row.label)),
                tag(row.caution)
            ));
        } else {
            lines.push(format!(
                "{:<prefix_width$}{}{}",
                row.prefix,
                paint(row.label.clone()),
                tag(row.caution)
            ));
            lines.push(format!("{:prefix_width$}{description}", ""));
        }
    }
    lines
}

/// The text with its first letter in upper case.
pub fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Words wrapped to lines of at most `width` characters; a longer word keeps its own line.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Key/value rows with the keys in one aligned column.
pub fn key_values(rows: &[(&str, String)], indent: usize) -> Vec<String> {
    let width = rows
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|(key, value)| format!("{:indent$}{key:<width$}   {value}", ""))
        .collect()
}

/// A setup error as people read it: the headline, an indented fix where one is known, then the
/// code, dim.
pub fn error_lines(code: &str, message: &str, style: Style) -> Vec<String> {
    // A cancel is the person's own choice, not an error.
    if code == "ERR_CEDEGRID_SETUP_CANCELLED" {
        return vec!["Cancelled. Nothing was written.".into()];
    }
    let mut lines = vec![format!("{} {message}", style.refusal("error:"))];
    if let Some(fix) = fix_for(code) {
        lines.push(format!("  - {fix}"));
    }
    lines.push(style.dim(&format!("  code: {code}")));
    lines
}

/// The fix for a setup error, where the message alone does not say it.
fn fix_for(code: &str) -> Option<&'static str> {
    match code {
        "ERR_CEDEGRID_ALLOWANCE_INTERACTIVE_ONLY" => Some(
            "Run cedegrid setup --allow-unprotected in an interactive shell: it shows this host's numbers, then asks for a fixed phrase.",
        ),
        "ERR_CEDEGRID_SETUP_TERMINAL" => Some(
            "Use a terminal that moves the cursor, or pass the choices as flags with --non-interactive; cedegrid setup --help lists them.",
        ),
        "ERR_CEDEGRID_CAUTION_UNACKNOWLEDGED" => Some(
            "Confirm the option in interactive setup, or pass --acknowledge-caution with its name.",
        ),
        _ => None,
    }
}

/// The interactive paths: the terminal interface and the line prompts.
mod interface;

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Host {
        Host {
            cpu_millicores: 32_000,
            physical_cores: Some(16),
            ram_mib: 262_144,
            gpus: vec![("GPU-a".into(), 24_576)],
        }
    }

    fn target(text: &str) -> (tempfile::TempDir, Target) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        std::fs::write(&path, text).unwrap();
        let target = locate(&path).unwrap();
        (dir, target)
    }

    /// The caution mapping (B3, FR-045): every mode, Performance the default included,
    /// carries no marker; small floors, reduced margins and relaxed detection are
    /// caution.
    #[test]
    fn caution_mapping_follows_fr_045() {
        let (_dir, plain) = target("config_version = 1\n");
        let host = host();
        let risk = |target: &Target, field, pick| risk(target, field, pick, Some(&host));
        for kind in ResourceKind::ALL {
            for mode in ContentionMode::MARGINED {
                assert_eq!(risk(&plain, Field::Mode(kind), Pick::Mode(mode)), None);
            }
            assert_eq!(
                risk(
                    &plain,
                    Field::Mode(kind),
                    Pick::Mode(ContentionMode::Aggressive)
                ),
                Some("aggressive_mode")
            );
            assert_eq!(risk(&plain, Field::Mode(kind), Pick::Inherit), None);
        }
        for field in [Field::DefaultMode, Field::MaxMode] {
            for mode in ContentionMode::MARGINED {
                assert_eq!(risk(&plain, field, Pick::Mode(mode)), None);
            }
            // 004: Aggressive and a cap at it each take the caution step.
            assert_eq!(
                risk(&plain, field, Pick::Mode(ContentionMode::Aggressive)),
                Some("aggressive_mode")
            );
        }
        // Default floors carry no marker. With percentages, 5% of this host is
        // the floor whatever the fixed part says.
        assert_eq!(risk(&plain, Field::RamFloor, Pick::Number(8192)), None);
        assert_eq!(risk(&plain, Field::RamFloor, Pick::Number(600)), None);
        assert_eq!(risk(&plain, Field::GpuFloor, Pick::Number(1024)), None);
        assert_eq!(risk(&plain, Field::GpuFloor, Pick::Number(600)), None);
        let (_dir, fixed) = target(
            "config_version = 1\n[ram]\nreserve_percent = 0\n[gpu]\nreserve_vram_percent = 0\n",
        );
        assert_eq!(risk(&fixed, Field::RamFloor, Pick::Number(1024)), None);
        assert_eq!(
            risk(&fixed, Field::RamFloor, Pick::Number(1023)),
            Some("small_floor")
        );
        assert_eq!(
            risk(&fixed, Field::GpuFloor, Pick::Number(600)),
            Some("small_floor")
        );
        // Margins: the default and larger ones carry no marker; smaller is caution.
        for field in node_fields() {
            if let Field::Margin(kind, mode) = field {
                let default = u64::from(margin_of(&ResourceThresholds::defaults(kind), mode).1);
                assert_eq!(
                    risk(&plain, Field::Margin(kind, mode), Pick::Number(default)),
                    None,
                    "{kind:?} {mode:?}"
                );
            }
        }
        assert_eq!(
            risk(
                &plain,
                Field::Margin(ResourceKind::Cpu, ContentionMode::YieldFirst),
                Pick::Number(30)
            ),
            None
        );
        assert_eq!(
            risk(
                &plain,
                Field::Margin(ResourceKind::Cpu, ContentionMode::Neutral),
                Pick::Number(3)
            ),
            Some("reduced_margin")
        );
        // Detection: beyond the default is caution, the other way is not.
        assert_eq!(
            risk(&plain, Field::NoiseTolerance, Pick::Number(512)),
            Some("relaxed_detection")
        );
        assert_eq!(risk(&plain, Field::NoiseTolerance, Pick::Number(128)), None);
        assert_eq!(
            risk(&plain, Field::AttributionTolerance, Pick::Number(20)),
            Some("relaxed_detection")
        );
        assert_eq!(
            risk(&plain, Field::AttributionTolerance, Pick::Number(10)),
            None
        );
        assert_eq!(risk(&plain, Field::HintDeadline, Pick::Number(10)), None);
    }

    /// Consequence lines use this host's numbers.
    #[test]
    fn consequence_lines_use_host_numbers() {
        let config = Config::default();
        let host = host();
        assert_eq!(
            consequence(
                Field::Mode(ResourceKind::Cpu),
                Pick::Mode(ContentionMode::Neutral),
                &config,
                Some(&host)
            ),
            "keeps 1600 m of 32 000 m CPU free · holds the fair share under contention"
        );
        assert_eq!(
            consequence(Field::RamFloor, Pick::Number(8192), &config, Some(&host)),
            "RAM floor 13 108 MiB on this host"
        );
        assert_eq!(
            consequence(Field::GpuFloor, Pick::Number(1024), &config, Some(&host)),
            "GPU memory floor 1229 MiB on GPU-a"
        );
        // Memory keeps only the floor under Neutral and Performance.
        assert_eq!(
            consequence(
                Field::Mode(ResourceKind::Ram),
                Pick::Mode(ContentionMode::Neutral),
                &config,
                Some(&host)
            ),
            "keeps the floor only (13 108 MiB on this host)"
        );
        assert_eq!(
            consequence(
                Field::Mode(ResourceKind::GpuMemory),
                Pick::Mode(ContentionMode::Performance),
                &config,
                Some(&host)
            ),
            "keeps the floor only (1229 MiB on GPU-a)"
        );
        // Under Aggressive, memory says it follows Performance (004 FR-006).
        assert_eq!(
            consequence(
                Field::Mode(ResourceKind::Ram),
                Pick::Mode(ContentionMode::Aggressive),
                &config,
                Some(&host)
            ),
            "same as Performance on memory: keeps the floor only (13 108 MiB on this host)"
        );
        for kind in [ResourceKind::Ram, ResourceKind::GpuMemory] {
            for mode in [ContentionMode::Neutral, ContentionMode::Performance] {
                assert!(!node_fields().contains(&Field::Margin(kind, mode)));
            }
        }
    }

    /// A list longer than the terminal scrolls so the highlight stays in view, near the middle.
    #[cfg(unix)]
    #[test]
    fn a_long_list_keeps_the_highlight_in_view() {
        use super::interface::window_start;
        assert_eq!(window_start(5, 10, 3), 0);
        assert_eq!(window_start(20, 6, 0), 0);
        assert_eq!(window_start(20, 6, 10), 7);
        assert_eq!(window_start(20, 6, 19), 14);
    }

    /// Option lists keep labels in one column and descriptions beside them, or under them on a
    /// narrow terminal; text is wrapped at word boundaries.
    #[test]
    fn option_lists_align_and_fall_back_to_two_lines() {
        let row = |label: &str, description: &str, caution| OptionRow {
            prefix: "   1  ".into(),
            label: label.into(),
            description: description.into(),
            caution,
            selected: false,
        };
        let rows = [
            row("Neutral", "holds the fair share", false),
            row("Aggressive", "fills idle compute at once", true),
        ];
        let plain = Style { color: false };
        assert_eq!(
            option_lines(&rows, plain, Some(80)),
            [
                "   1  Neutral     Holds the fair share",
                "   1  Aggressive  Fills idle compute at once  [caution]",
            ]
        );
        assert_eq!(
            option_lines(&rows, plain, Some(40)),
            [
                "   1  Neutral",
                "      Holds the fair share",
                "   1  Aggressive  [caution]",
                "      Fills idle compute at once",
            ]
        );
        assert_eq!(wrap("one two three four", 9), ["one two", "three", "four"]);
        assert_eq!(utc_minute(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc_minute(1_795_000_000_000), "2026-11-18 11:06 UTC");
        assert_eq!(
            error_lines(
                "ERR_CEDEGRID_ALLOWANCE_INTERACTIVE_ONLY",
                "no terminal",
                plain
            ),
            [
                "error: no terminal",
                "  - Run cedegrid setup --allow-unprotected in an interactive shell: it shows this host's numbers, then asks for a fixed phrase.",
                "  code: ERR_CEDEGRID_ALLOWANCE_INTERACTIVE_ONLY",
            ]
        );
        assert_eq!(
            error_lines("ERR_CEDEGRID_SETUP_CANCELLED", "cancelled", plain),
            ["Cancelled. Nothing was written."]
        );
    }

    /// Every option the interface offers maps to a key the configuration accepts,
    /// and Performance, the default, is preselected on a new file.
    #[test]
    fn every_offered_option_loads() {
        let (_dir, plain) = target("config_version = 1\n");
        let (_, base) = node_base(&plain).unwrap();
        assert_eq!(
            Field::DefaultMode.current(&base),
            Pick::Mode(ContentionMode::Performance)
        );
        for field in node_fields() {
            for pick in field.options(&base, false) {
                let mut choices = Choices::default();
                field.apply(&mut choices, pick);
                choices.acknowledge = ACKNOWLEDGEMENTS.iter().map(|s| s.to_string()).collect();
                let result = plan(&plain.path, &choices, Some(&host()));
                assert!(result.is_ok(), "{field:?} {pick:?}: {:?}", result.err());
            }
        }
    }
}
