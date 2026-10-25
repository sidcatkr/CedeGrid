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
    /// performance_mode, small_floor, reduced_margin or relaxed_detection.
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

pub fn mode_name(mode: ContentionMode) -> &'static str {
    match mode {
        ContentionMode::YieldFirst => "yield_first",
        ContentionMode::Neutral => "neutral",
        ContentionMode::Performance => "performance",
    }
}

fn mode_label(mode: ContentionMode) -> &'static str {
    match mode {
        ContentionMode::YieldFirst => "Yield First",
        ContentionMode::Neutral => "Neutral",
        ContentionMode::Performance => "Performance",
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
        _ => Err(argument(format!(
            "{flag} takes yield_first, neutral or performance, not {text:?}"
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

/// Large numbers with a space between thousands, as consequence lines show them.
fn group(value: u64) -> String {
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
    match mode {
        ContentionMode::YieldFirst => (
            "yield_first_margin_percent",
            thresholds.yield_first_margin_percent,
        ),
        ContentionMode::Neutral => ("neutral_margin_percent", thresholds.neutral_margin_percent),
        ContentionMode::Performance => (
            "performance_margin_percent",
            thresholds.performance_margin_percent,
        ),
    }
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
            mode_name(mode).into(),
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
                mode_name(mode).into(),
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
                Some(mode) => set(document, &section, "mode", mode_name(*mode).into(), changes)?,
                None => remove(document, &section, "mode", changes)?,
            }
        }
        let thresholds = contention.thresholds(kind);
        for mode in ContentionMode::ALL {
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
        Err(error) => {
            let (exit, code, message) = match error.downcast_ref::<SetupError>() {
                Some(error) => (error.exit, error.code, error.message.clone()),
                None => (1, "ERR_CEDEGRID_SETUP", format!("{error:#}")),
            };
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({"error": {"code": code, "message": message}})
                );
            } else {
                eprintln!("{code}: {message}");
            }
            exit
        }
    }
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
        for mode in ContentionMode::ALL {
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
                format!("{} margin, {}", resource_label(kind), mode_label(mode))
            }
            Field::HintDeadline => "Guaranteed hint deadline".into(),
            Field::NoiseTolerance => "GPU memory noise tolerance".into(),
            Field::AttributionTolerance => "GPU compute attribution tolerance".into(),
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
            (_, Pick::Mode(mode)) => mode_label(mode).into(),
            (Field::Mode(_), Pick::Inherit) => {
                format!(
                    "same as default ({})",
                    mode_label(config.contention.default_mode)
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
    };
    let margin = |kind: ResourceKind, mode: ContentionMode| -> u64 {
        u64::from(margin_of(&config.contention.thresholds(kind), mode).1)
    };
    let resource_line = |kind: ResourceKind, mode: ContentionMode| -> String {
        match kind {
            ResourceKind::Cpu => format!("{}; {}", cpu_free(margin(kind, mode)), behavior(mode)),
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
                "keeps {}% of each GPU's compute free; {}",
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
        (Field::DefaultMode, Pick::Mode(mode)) => format!(
            "{} on every resource without its own mode",
            resource_line(ResourceKind::Cpu, mode)
        ),
        (Field::Mode(kind), Pick::Mode(mode)) => resource_line(kind, mode),
        (Field::Mode(kind), _) => format!(
            "follows the default mode: {}",
            resource_line(kind, config.contention.default_mode)
        ),
        (Field::MaxMode, Pick::Mode(mode)) => {
            format!("jobs may request at most {}", mode_label(mode))
        }
        (Field::MaxMode, _) => format!(
            "jobs may request at most the most aggressive configured mode ({})",
            mode_label(
                ResourceKind::ALL
                    .into_iter()
                    .map(|kind| config.contention.mode(kind))
                    .max()
                    .unwrap_or(ContentionMode::Neutral)
            )
        ),
        (Field::RamFloor, Pick::Number(mib)) => ram_floor(&with(&|c| c.ram.reserve_mib = mib)),
        (Field::GpuFloor, Pick::Number(mib)) => gpu_floor(&with(&|c| c.gpu.reserve_vram_mib = mib)),
        (Field::Margin(kind, mode), Pick::Number(percent)) => match kind {
            ResourceKind::Cpu => format!("{} in {}", cpu_free(percent), mode_label(mode)),
            ResourceKind::GpuCompute => format!(
                "keeps {percent}% of each GPU's compute free in {}",
                mode_label(mode)
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
        "performance_mode" => {
            "Own work keeps all idle capacity minus a small margin and grows quickly. Other workloads still get their fair share within the reclaim bounds, and floors still hold."
        }
        "small_floor" => {
            "Less headroom is kept for other workloads' bursts. The enforced minimum and the out-of-memory preference still protect them."
        }
        "reduced_margin" => {
            "Own work leaves less idle capacity for external demand to grow into. Contention handling and floors still hold."
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
        let mode = config.contention.mode(kind);
        lines.push(format!(
            "{:<12} {}: {}",
            resource_label(kind),
            mode_label(mode),
            consequence(Field::Mode(kind), Pick::Mode(mode), config, host)
        ));
    }
    if let Some(host) = host {
        for (uuid, total) in host.gpus.iter().skip(1) {
            lines.push(format!(
                "{:<12} GPU memory floor {} MiB on {uuid}",
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

/// The interactive paths: the terminal interface and the line prompts.
mod interface {
    use super::*;

    /// Whether color may be used (FR-050).
    #[cfg(unix)]
    fn color_allowed() -> bool {
        std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
            && std::env::var("TERM").map_or(true, |term| term != "dumb")
            && std::io::stdout().is_terminal()
    }

    #[cfg(unix)]
    fn cursor_control() -> bool {
        std::env::var("TERM").map_or(true, |term| term != "dumb" && !term.is_empty())
    }

    pub fn run(target: &Target, flags: Choices, host: Option<&Host>) -> Result<Option<Choices>> {
        let role = match (target.role, flags.role) {
            (Some(role), _) | (None, Some(role)) => role,
            (None, None) => {
                if cfg!(windows) {
                    Role::Client
                } else {
                    Role::Node
                }
            }
        };
        if role != Role::Node {
            return prompts::connection(target, role, flags);
        }
        let (_, base) = node_base(target)?;
        let mut state = State::new(target.clone(), base, flags, host.cloned());
        #[cfg(unix)]
        if cursor_control()
            && let Some(size) = terminal::size()
            && size.0 as usize >= state.rows().len() + 8
            && size.1 >= 72
        {
            return match screen::run(&mut state, color_allowed())? {
                screen::Outcome::Done(choices) => Ok(Some(choices)),
                screen::Outcome::Cancelled => Ok(None),
                screen::Outcome::Role(role) => {
                    prompts::connection(target, role, Choices::default())
                }
            };
        }
        prompts::node(&mut state)
    }

    /// The interface's choices so far, on top of the current configuration.
    pub struct State {
        pub target: Target,
        pub base: Config,
        pub values: BTreeMap<Field, Pick>,
        pub acknowledged: BTreeSet<String>,
        pub host: Option<Host>,
        pub fresh: bool,
        flags: Choices,
    }

    impl State {
        fn new(target: Target, base: Config, flags: Choices, host: Option<Host>) -> Self {
            let fresh = target.original.is_none();
            let mut values = BTreeMap::new();
            // Start from the file, with any flags already applied.
            let preview = plan(&target.path, &flags, host.as_ref())
                .ok()
                .and_then(|plan| plan.config)
                .unwrap_or_else(|| base.clone());
            for field in node_fields() {
                values.insert(field, field.current(&preview));
            }
            Self {
                acknowledged: flags.acknowledge.clone(),
                target,
                base,
                values,
                host,
                fresh,
                flags,
            }
        }

        pub fn rows(&self) -> Vec<Field> {
            node_fields()
        }

        pub fn choices(&self) -> Choices {
            let mut choices = self.flags.clone();
            choices.role = Some(Role::Node);
            for (field, pick) in &self.values {
                if *field != Field::Role {
                    field.apply(&mut choices, *pick);
                }
            }
            choices.acknowledge = self.acknowledged.clone();
            choices
        }

        /// The configuration the current choices give, for consequence lines.
        pub fn preview(&self) -> Config {
            let mut choices = self.choices();
            choices.acknowledge = ACKNOWLEDGEMENTS.iter().map(|s| s.to_string()).collect();
            plan(&self.target.path, &choices, self.host.as_ref())
                .ok()
                .and_then(|plan| plan.config)
                .unwrap_or_else(|| self.base.clone())
        }

        pub fn risk(&self, field: Field, pick: Pick) -> Option<&'static str> {
            risk(&self.target, field, pick, self.host.as_ref())
        }

        /// Whether choosing `option` needs the confirmation screen.
        pub fn needs_confirmation(&self, option: &str) -> bool {
            !self.acknowledged.contains(option)
                && !self.base.contention.acknowledge_caution.contains(option)
        }

        pub fn plan(&self) -> Result<Plan> {
            plan(&self.target.path, &self.choices(), self.host.as_ref())
        }
    }

    /// Line-based prompts, for terminals the interface cannot drive (FR-055).
    mod prompts {
        use super::*;

        fn ask(question: &str) -> Result<Option<String>> {
            print!("{question}");
            std::io::stdout().flush()?;
            let mut line = String::new();
            if std::io::stdin().lock().read_line(&mut line)? == 0 {
                return Ok(None);
            }
            Ok(Some(line.trim().to_string()))
        }

        pub fn node(state: &mut State) -> Result<Option<Choices>> {
            println!("CedeGrid setup: {} (node)", state.target.path.display());
            println!("Press Enter to keep the current value. Ctrl-D cancels.");
            for field in state.rows() {
                if field == Field::Role {
                    continue;
                }
                let config = state.preview();
                let options = field.options(&config, state.fresh);
                println!();
                println!(
                    "{}: {}",
                    field.label(),
                    field.show(state.values[&field], &config)
                );
                for (index, option) in options.iter().enumerate() {
                    let marker = state
                        .risk(field, *option)
                        .map_or(String::new(), |_| format!(" {CAUTION}"));
                    println!(
                        "  {}) {}{marker}: {}",
                        index + 1,
                        field.show(*option, &config),
                        consequence(field, *option, &config, state.host.as_ref())
                    );
                }
                loop {
                    let Some(answer) = ask(&format!("Choose 1-{}: ", options.len()))? else {
                        return Ok(None);
                    };
                    if answer.is_empty() {
                        break;
                    }
                    let Some(pick) = answer
                        .parse::<usize>()
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                        .and_then(|n| options.get(n))
                        .copied()
                    else {
                        println!("Enter a number from 1 to {}.", options.len());
                        continue;
                    };
                    if let Some(option) = state.risk(field, pick)
                        && state.needs_confirmation(option)
                    {
                        println!("{CAUTION} {}", justification(option));
                        let Some(answer) =
                            ask(&format!("Type yes to record {option} and use this value: "))?
                        else {
                            return Ok(None);
                        };
                        if answer != "yes" {
                            println!("Kept {}.", field.show(state.values[&field], &config));
                            break;
                        }
                        state.acknowledged.insert(option.to_string());
                    }
                    state.values.insert(field, pick);
                    break;
                }
            }
            let plan = state.plan()?;
            println!();
            for line in summary_lines(&plan, state.host.as_ref()) {
                println!("{line}");
            }
            let Some(answer) = ask("Write this configuration? [y/N] ")? else {
                return Ok(None);
            };
            Ok(matches!(answer.as_str(), "y" | "Y" | "yes").then(|| state.choices()))
        }

        pub fn connection(target: &Target, role: Role, flags: Choices) -> Result<Option<Choices>> {
            println!(
                "CedeGrid setup: {} ({})",
                target.path.display(),
                role.name()
            );
            let current: Option<DocumentMut> =
                target.original.as_ref().and_then(|text| text.parse().ok());
            let mut choices = flags;
            choices.role = Some(role);
            for key in ConnectionKey::for_role(role) {
                let (path, name) = key.path();
                if *key == ConnectionKey::OperatorFingerprint && current.is_some() {
                    // An existing coordinator keeps its authorized clients.
                    continue;
                }
                let existing = choices.connection.get(key).cloned().or_else(|| {
                    let mut item = current.as_ref().map(|document| document.as_item())?;
                    for segment in path {
                        item = item.get(segment)?;
                    }
                    item.get(name)?.as_str().map(str::to_string)
                });
                let shown = existing.clone().unwrap_or_default();
                let Some(answer) = ask(&format!("{} [{shown}]: ", key.label()))? else {
                    return Ok(None);
                };
                match (answer.is_empty(), existing) {
                    (false, _) => {
                        choices.connection.insert(*key, answer);
                    }
                    (true, Some(value)) => {
                        choices.connection.insert(*key, value);
                    }
                    (true, None) => {
                        return Err(missing(format!("{} needs a value", key.label())));
                    }
                }
            }
            let plan = plan(&target.path, &choices, None)?;
            for line in summary_lines(&plan, None) {
                println!("{line}");
            }
            let Some(answer) = ask("Write this configuration? [y/N] ")? else {
                return Ok(None);
            };
            Ok(matches!(answer.as_str(), "y" | "Y" | "yes").then_some(choices))
        }
    }

    /// The keyboard interface (FR-046 to FR-051).
    #[cfg(unix)]
    mod screen {
        use super::*;
        use terminal::Key;

        pub enum Outcome {
            Done(Choices),
            Cancelled,
            Role(Role),
        }

        enum View {
            Main,
            Options {
                field: Field,
                options: Vec<Pick>,
                cursor: usize,
                marked: usize,
            },
            Confirm {
                field: Field,
                pick: Pick,
                option: &'static str,
                cursor: usize,
                marked: usize,
            },
            Summary(Box<Result<Plan, String>>),
        }

        struct Painter {
            color: bool,
            out: std::io::Stdout,
        }

        impl Painter {
            fn caution(&self) -> String {
                if self.color {
                    format!("\x1b[33m{CAUTION}\x1b[0m")
                } else {
                    CAUTION.to_string()
                }
            }

            /// One full frame. Without color the frame is plain text, so no
            /// escape sequence is written at all.
            fn frame(&mut self, lines: &[String]) -> Result<()> {
                let mut text = String::new();
                if self.color {
                    text.push_str("\x1b[H\x1b[2J");
                } else {
                    text.push('\n');
                }
                for line in lines {
                    text.push_str(line);
                    text.push('\n');
                }
                self.out.write_all(text.as_bytes())?;
                self.out.flush()?;
                Ok(())
            }
        }

        pub fn run(state: &mut State, color: bool) -> Result<Outcome> {
            let _raw = terminal::Raw::enter().context("cannot drive the terminal")?;
            let mut keys = terminal::Keys::default();
            let mut painter = Painter {
                color,
                out: std::io::stdout(),
            };
            let rows = state.rows();
            let mut row = 0usize;
            let mut view = View::Main;
            let mut notice: Option<String> = None;
            loop {
                let lines = render(state, &painter, &rows, row, &view, notice.as_deref());
                painter.frame(&lines)?;
                let key = keys.next()?;
                if key == Key::Cancel {
                    return Ok(Outcome::Cancelled);
                }
                let total = rows.len() + 2;
                view = match view {
                    View::Main => match key {
                        Key::Up => {
                            row = (row + total - 1) % total;
                            View::Main
                        }
                        Key::Down => {
                            row = (row + 1) % total;
                            View::Main
                        }
                        Key::Space | Key::Enter if row == rows.len() => {
                            notice = None;
                            View::Summary(Box::new(state.plan().map_err(|error| error.to_string())))
                        }
                        Key::Space | Key::Enter if row == rows.len() + 1 => {
                            return Ok(Outcome::Cancelled);
                        }
                        Key::Space | Key::Enter => {
                            let field = rows[row];
                            let config = state.preview();
                            let options = field.options(&config, state.fresh);
                            let current = options
                                .iter()
                                .position(|pick| *pick == state.values[&field])
                                .unwrap_or(0);
                            View::Options {
                                field,
                                options,
                                cursor: current,
                                marked: current,
                            }
                        }
                        _ => View::Main,
                    },
                    View::Options {
                        field,
                        options,
                        mut cursor,
                        mut marked,
                    } => match key {
                        Key::Up => {
                            cursor = cursor.saturating_sub(1);
                            View::Options {
                                field,
                                options,
                                cursor,
                                marked,
                            }
                        }
                        Key::Down => {
                            cursor = (cursor + 1).min(options.len() - 1);
                            View::Options {
                                field,
                                options,
                                cursor,
                                marked,
                            }
                        }
                        Key::Space => {
                            marked = cursor;
                            View::Options {
                                field,
                                options,
                                cursor,
                                marked,
                            }
                        }
                        Key::Enter => {
                            let pick = options[marked];
                            if let Pick::Role(role) = pick {
                                if role != Role::Node {
                                    return Ok(Outcome::Role(role));
                                }
                                View::Main
                            } else {
                                match state.risk(field, pick) {
                                    Some(option) if state.needs_confirmation(option) => {
                                        View::Confirm {
                                            field,
                                            pick,
                                            option,
                                            cursor: 0,
                                            marked: 0,
                                        }
                                    }
                                    _ => {
                                        state.values.insert(field, pick);
                                        View::Main
                                    }
                                }
                            }
                        }
                        _ => View::Options {
                            field,
                            options,
                            cursor,
                            marked,
                        },
                    },
                    View::Confirm {
                        field,
                        pick,
                        option,
                        mut cursor,
                        mut marked,
                    } => match key {
                        Key::Up => {
                            cursor = 0;
                            View::Confirm {
                                field,
                                pick,
                                option,
                                cursor,
                                marked,
                            }
                        }
                        Key::Down => {
                            cursor = 1;
                            View::Confirm {
                                field,
                                pick,
                                option,
                                cursor,
                                marked,
                            }
                        }
                        Key::Space => {
                            marked = cursor;
                            View::Confirm {
                                field,
                                pick,
                                option,
                                cursor,
                                marked,
                            }
                        }
                        Key::Enter => {
                            if marked == 1 {
                                state.acknowledged.insert(option.to_string());
                                state.values.insert(field, pick);
                            }
                            View::Main
                        }
                        _ => View::Confirm {
                            field,
                            pick,
                            option,
                            cursor,
                            marked,
                        },
                    },
                    View::Summary(plan) => match (key, *plan) {
                        (Key::Enter, Ok(_)) => return Ok(Outcome::Done(state.choices())),
                        (Key::Enter | Key::Space, Err(error)) => {
                            notice = Some(error);
                            View::Main
                        }
                        (_, plan) => View::Summary(Box::new(plan)),
                    },
                };
            }
        }

        fn render(
            state: &State,
            painter: &Painter,
            rows: &[Field],
            row: usize,
            view: &View,
            notice: Option<&str>,
        ) -> Vec<String> {
            let config = state.preview();
            let pointer = |selected: bool| if selected { "> " } else { "  " };
            let marker = |risky: bool| {
                if risky {
                    format!(" {}", painter.caution())
                } else {
                    String::new()
                }
            };
            let mut lines = Vec::new();
            match view {
                View::Main => {
                    lines.push(format!(
                        "CedeGrid setup: {} (node)",
                        state.target.path.display()
                    ));
                    lines.push("Up/Down move, Space or Enter open, Esc or q cancel".into());
                    lines.push(String::new());
                    for (index, field) in rows.iter().enumerate() {
                        let pick = state.values[field];
                        // Floors also show what applies on this host.
                        let host = match (field, state.host.as_ref()) {
                            (Field::RamFloor, Some(host)) => format!(
                                "; {} MiB on this host",
                                group(config.ram_floor_mib(host.ram_mib))
                            ),
                            (Field::GpuFloor, Some(host)) => host
                                .gpus
                                .first()
                                .map(|(uuid, total)| {
                                    format!(
                                        "; {} MiB on {uuid}",
                                        group(config.vram_floor_mib(*total))
                                    )
                                })
                                .unwrap_or_else(|| "; no GPU on this host".into()),
                            _ => String::new(),
                        };
                        lines.push(format!(
                            "{}{:<36}{}{host}{}",
                            pointer(index == row),
                            field.label(),
                            field.show(pick, &config),
                            marker(state.risk(*field, pick).is_some())
                        ));
                    }
                    lines.push(format!("{}Review and write", pointer(row == rows.len())));
                    lines.push(format!("{}Cancel", pointer(row == rows.len() + 1)));
                    if let Some(notice) = notice {
                        lines.push(String::new());
                        lines.push(format!("Cannot write: {notice}"));
                    }
                }
                View::Options {
                    field,
                    options,
                    cursor,
                    marked,
                } => {
                    lines.push(field.label());
                    lines.push("Up/Down move, Space select, Enter confirm, Esc or q cancel".into());
                    lines.push(String::new());
                    for (index, option) in options.iter().enumerate() {
                        lines.push(format!(
                            "{}({}) {}{}",
                            pointer(index == *cursor),
                            if index == *marked { '*' } else { ' ' },
                            field.show(*option, &config),
                            marker(state.risk(*field, *option).is_some())
                        ));
                        lines.push(format!(
                            "        {}",
                            consequence(*field, *option, &config, state.host.as_ref())
                        ));
                    }
                }
                View::Confirm {
                    field,
                    pick,
                    option,
                    cursor,
                    marked,
                } => {
                    let kept = state.values[field];
                    lines.push("Confirm a caution option".into());
                    lines.push(String::new());
                    lines.push(format!(
                        "{} {}: {}",
                        painter.caution(),
                        field.label(),
                        field.show(*pick, &config)
                    ));
                    lines.push(consequence(*field, *pick, &config, state.host.as_ref()));
                    lines.push(justification(option).into());
                    lines.push(String::new());
                    lines.push("Up/Down move, Space select, Enter confirm, Esc or q cancel".into());
                    lines.push(format!(
                        "{}({}) No, keep {}",
                        pointer(*cursor == 0),
                        if *marked == 0 { '*' } else { ' ' },
                        field.show(kept, &config)
                    ));
                    lines.push(format!(
                        "{}({}) Yes, use {} and record {option} in contention.acknowledge_caution",
                        pointer(*cursor == 1),
                        if *marked == 1 { '*' } else { ' ' },
                        field.show(*pick, &config)
                    ));
                }
                View::Summary(plan) => {
                    lines.push("Review before writing".into());
                    lines.push(String::new());
                    match plan.as_ref() {
                        Ok(plan) => {
                            for line in summary_lines(plan, state.host.as_ref()) {
                                lines.push(line.replace(CAUTION, &painter.caution()));
                            }
                            lines.push(String::new());
                            lines.push("Enter write, Esc or q cancel".into());
                        }
                        Err(error) => {
                            lines.push(format!("Cannot write: {error}"));
                            lines.push(String::new());
                            lines.push("Enter back, Esc or q cancel".into());
                        }
                    }
                }
            }
            lines
        }
    }

    /// Raw mode through termios, restored by a drop guard and on signals.
    #[cfg(unix)]
    pub(super) mod terminal {
        use std::sync::atomic::{AtomicI32, Ordering};

        static SAVED_FD: AtomicI32 = AtomicI32::new(-1);
        static mut SAVED: std::mem::MaybeUninit<libc::termios> = std::mem::MaybeUninit::uninit();
        const SIGNALS: [libc::c_int; 4] =
            [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT];

        extern "C" fn restore_and_reraise(signal: libc::c_int) {
            let fd = SAVED_FD.load(Ordering::SeqCst);
            // tcsetattr, signal and raise are async-signal-safe.
            unsafe {
                if fd >= 0 {
                    libc::tcsetattr(fd, libc::TCSANOW, (&raw const SAVED).cast());
                }
                libc::signal(signal, libc::SIG_DFL);
                libc::raise(signal);
            }
        }

        /// Raw input on stdin until dropped.
        pub struct Raw {
            saved: libc::termios,
            previous: Vec<(libc::c_int, libc::sigaction)>,
        }

        impl Raw {
            pub fn enter() -> std::io::Result<Self> {
                let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
                if unsafe { libc::tcgetattr(0, saved.as_mut_ptr()) } != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let saved = unsafe { saved.assume_init() };
                unsafe {
                    (&raw mut SAVED).write(std::mem::MaybeUninit::new(saved));
                }
                SAVED_FD.store(0, Ordering::SeqCst);
                let mut previous = Vec::new();
                for signal in SIGNALS {
                    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
                    action.sa_sigaction = restore_and_reraise as *const () as libc::sighandler_t;
                    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
                    if unsafe { libc::sigaction(signal, &action, &mut old) } == 0 {
                        previous.push((signal, old));
                    }
                }
                let guard = Self { saved, previous };
                let mut raw = saved;
                raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
                raw.c_iflag &= !(libc::IXON | libc::ICRNL | libc::INLCR);
                raw.c_cc[libc::VMIN] = 1;
                raw.c_cc[libc::VTIME] = 0;
                if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(guard)
            }
        }

        impl Drop for Raw {
            fn drop(&mut self) {
                unsafe {
                    libc::tcsetattr(0, libc::TCSANOW, &self.saved);
                }
                SAVED_FD.store(-1, Ordering::SeqCst);
                for (signal, old) in &self.previous {
                    unsafe {
                        libc::sigaction(*signal, old, std::ptr::null_mut());
                    }
                }
            }
        }

        /// The terminal's rows and columns.
        pub fn size() -> Option<(u16, u16)> {
            let mut size: libc::winsize = unsafe { std::mem::zeroed() };
            (unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_row > 0)
                .then_some((size.ws_row, size.ws_col))
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Key {
            Up,
            Down,
            Space,
            Enter,
            Cancel,
            Other,
        }

        /// Keys read straight from fd 0; bytes after one key stay queued.
        #[derive(Default)]
        pub struct Keys {
            pending: std::collections::VecDeque<u8>,
        }

        impl Keys {
            fn fill(&mut self, wait_ms: i32) -> std::io::Result<bool> {
                if wait_ms >= 0 {
                    let mut poll = libc::pollfd {
                        fd: 0,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    if unsafe { libc::poll(&mut poll, 1, wait_ms) } <= 0 {
                        return Ok(false);
                    }
                }
                let mut buffer = [0u8; 64];
                let read = unsafe { libc::read(0, buffer.as_mut_ptr().cast(), buffer.len()) };
                if read < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        return Ok(true);
                    }
                    return Err(error);
                }
                self.pending.extend(&buffer[..read as usize]);
                Ok(read > 0)
            }

            pub fn next(&mut self) -> std::io::Result<Key> {
                if self.pending.is_empty() && !self.fill(-1)? {
                    // End of input cancels.
                    return Ok(Key::Cancel);
                }
                let Some(first) = self.pending.pop_front() else {
                    return Ok(Key::Cancel);
                };
                Ok(match first {
                    0x1b => {
                        // A lone Esc, or the start of an arrow key.
                        if self.pending.is_empty() {
                            self.fill(50)?;
                        }
                        match (self.pending.front().copied(), self.pending.get(1).copied()) {
                            (Some(b'[' | b'O'), Some(code)) => {
                                self.pending.drain(..2);
                                match code {
                                    b'A' => Key::Up,
                                    b'B' => Key::Down,
                                    _ => Key::Other,
                                }
                            }
                            _ => Key::Cancel,
                        }
                    }
                    b' ' => Key::Space,
                    b'\r' | b'\n' => Key::Enter,
                    b'q' | b'Q' | 0x03 => Key::Cancel,
                    _ => Key::Other,
                })
            }
        }
    }
}

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

    /// The caution mapping (B3, FR-045): defaults and more yielding choices carry
    /// no marker; Performance, small floors, reduced margins and relaxed detection
    /// are caution.
    #[test]
    fn caution_mapping_follows_fr_045() {
        let (_dir, plain) = target("config_version = 1\n");
        let host = host();
        let risk = |target: &Target, field, pick| risk(target, field, pick, Some(&host));
        for kind in ResourceKind::ALL {
            for mode in [ContentionMode::YieldFirst, ContentionMode::Neutral] {
                assert_eq!(risk(&plain, Field::Mode(kind), Pick::Mode(mode)), None);
            }
            assert_eq!(risk(&plain, Field::Mode(kind), Pick::Inherit), None);
            assert_eq!(
                risk(
                    &plain,
                    Field::Mode(kind),
                    Pick::Mode(ContentionMode::Performance)
                ),
                Some("performance_mode")
            );
        }
        for field in [Field::DefaultMode, Field::MaxMode] {
            assert_eq!(
                risk(&plain, field, Pick::Mode(ContentionMode::Neutral)),
                None
            );
            assert_eq!(
                risk(&plain, field, Pick::Mode(ContentionMode::Performance)),
                Some("performance_mode")
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

    /// Consequence lines use this host's numbers (item 008, pin 2).
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
            "keeps 1600 m of 32 000 m CPU free; holds the fair share under contention"
        );
        assert_eq!(
            consequence(Field::RamFloor, Pick::Number(8192), &config, Some(&host)),
            "RAM floor 13 108 MiB on this host"
        );
        assert_eq!(
            consequence(Field::GpuFloor, Pick::Number(1024), &config, Some(&host)),
            "GPU memory floor 1229 MiB on GPU-a"
        );
        // Memory keeps only the floor under Neutral and Performance (item 009 B).
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
        for kind in [ResourceKind::Ram, ResourceKind::GpuMemory] {
            for mode in [ContentionMode::Neutral, ContentionMode::Performance] {
                assert!(!node_fields().contains(&Field::Margin(kind, mode)));
            }
        }
    }

    /// Every option the interface offers maps to a key the configuration accepts,
    /// and Neutral is preselected on a new file (item 008, pin 3).
    #[test]
    fn every_offered_option_loads() {
        let (_dir, plain) = target("config_version = 1\n");
        let (_, base) = node_base(&plain).unwrap();
        assert_eq!(
            Field::DefaultMode.current(&base),
            Pick::Mode(ContentionMode::Neutral)
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
