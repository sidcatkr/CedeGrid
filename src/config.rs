//! Portable configuration. Node names and paths are operator choices, never host aliases.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

use crate::model::SCHEMA_VERSION;

pub const CONFIG_VERSION: u32 = 1;
pub const CONFIG_ERROR: &str = "ERR_CEDEGRID_CONFIG";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeConfigKind {
    Node,
    Coordinator,
    Agent,
    Client,
}

/// File settings are separate from the certificate-only transport identity.
/// Keeping this DTO separate also keeps JSON RPC/data types free of config fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeClientConfig {
    pub endpoint: String,
    pub tls: crate::protocol::TlsIdentity,
    #[serde(default = "client_timeout_default")]
    pub timeout_seconds: f64,
    #[serde(default = "client_rate_default")]
    pub max_transfer_bytes_per_second: u64,
}

fn client_timeout_default() -> f64 {
    15.0
}

fn client_rate_default() -> u64 {
    10 * 1024 * 1024
}

fn config_error(error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{CONFIG_ERROR}: {error}")
}

/// Canonicalizing the input file deliberately follows a config-file symlink.
/// Paths *inside* the file are never expanded as shell, environment or tilde text.
pub fn load_runtime<T: DeserializeOwned>(path: &Path, kind: RuntimeConfigKind) -> Result<T> {
    let canonical = path.canonicalize().map_err(config_error)?;
    let contents = std::fs::read_to_string(&canonical).map_err(config_error)?;
    parse_runtime(&contents, &canonical, kind)
}

/// Parse against an already canonical file location. The file and future state
/// target need not exist; migration uses this to validate before publication.
pub fn parse_runtime<T: DeserializeOwned>(
    contents: &str,
    canonical_config_path: &Path,
    kind: RuntimeConfigKind,
) -> Result<T> {
    let result = (|| {
        ensure!(
            canonical_config_path.is_absolute(),
            "config location must be absolute"
        );
        let parsed: toml::Value = toml::from_str(contents)?;
        let mut value = toml_to_json(parsed)?;
        let object = value
            .as_object_mut()
            .context("configuration must be a table")?;
        ensure!(
            object.remove("config_version").and_then(|v| v.as_u64())
                == Some(u64::from(CONFIG_VERSION)),
            "required config_version must be {CONFIG_VERSION}"
        );
        ensure!(
            !object.contains_key("schema_version"),
            "schema_version is not a runtime configuration field"
        );
        if kind == RuntimeConfigKind::Node {
            normalize_toml_cpu_weight(object)?;
        }
        let base = canonical_config_path
            .parent()
            .context("config location has no parent")?;
        let normalized = normalize_effective(value, kind, base)?;
        Ok(serde_json::from_value(normalized)?)
    })();
    result.map_err(|error: anyhow::Error| config_error(format!("{error:#}")))
}

// Convert integers as signed i64, never through f64. TOML 1.0's datetime type
// has no place in runtime configuration, including paths and scalar options.
fn toml_to_json(value: toml::Value) -> Result<Value> {
    Ok(match value {
        toml::Value::String(value) => Value::String(value),
        toml::Value::Integer(value) => Value::from(value),
        toml::Value::Float(value) => {
            Value::Number(serde_json::Number::from_f64(value).context("non-finite config number")?)
        }
        toml::Value::Boolean(value) => Value::Bool(value),
        toml::Value::Datetime(_) => anyhow::bail!("datetime values are not runtime settings"),
        toml::Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(toml_to_json)
                .collect::<Result<_>>()?,
        ),
        toml::Value::Table(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| Ok((key, toml_to_json(value)?)))
                .collect::<Result<_>>()?,
        ),
    })
}

fn normalize_toml_cpu_weight(object: &mut serde_json::Map<String, Value>) -> Result<()> {
    let Some(cgroup) = object.get_mut("cgroup") else {
        return Ok(());
    };
    let cgroup = cgroup.as_object_mut().context("cgroup must be a table")?;
    let Some(weight) = cgroup.get_mut("cpu_weight") else {
        return Ok(());
    };
    let setting = weight
        .as_object()
        .context("cpu_weight requires a mode table")?;
    *weight = match setting.get("mode").and_then(Value::as_str) {
        Some("off") => {
            ensure!(setting.len() == 1, "cpu_weight off accepts only mode");
            Value::Null
        }
        Some("set") => {
            ensure!(
                setting.len() == 2,
                "cpu_weight set requires only mode and value"
            );
            let value = setting
                .get("value")
                .and_then(Value::as_u64)
                .context("cpu_weight value must be an integer")?;
            ensure!(
                (1..=10_000).contains(&value),
                "cpu_weight value must be in 1..=10000"
            );
            Value::from(value)
        }
        _ => anyhow::bail!("cpu_weight mode must be off or set"),
    };
    Ok(())
}

/// Normalize effective values without opening state, certificates or services.
/// Legacy migration supplies explicit old defaults before entering this function.
pub(crate) fn normalize_effective(
    value: Value,
    kind: RuntimeConfigKind,
    base: &Path,
) -> Result<Value> {
    ensure!(base.is_absolute(), "configuration base must be absolute");
    match kind {
        RuntimeConfigKind::Node => {
            let mut settings: Config = serde_json::from_value(value)?;
            settings.validate()?;
            // This is an internal record-format field, not an operator setting.
            settings.schema_version = SCHEMA_VERSION;
            settings.state_dir = resolve_path(base, &settings.state_dir)?;
            settings.kernel.monitor_interval_ms = settings.monitor.interval_ms;
            if settings.kernel.delegated_root.is_none() {
                settings.kernel.delegated_root = settings.cgroup.delegated_root.clone();
            }
            settings.validate()?;
            Ok(serde_json::to_value(settings)?)
        }
        RuntimeConfigKind::Client => {
            let mut settings: RuntimeClientConfig = serde_json::from_value(value)?;
            settings.endpoint = normalize_https_origin(&settings.endpoint)?;
            ensure!(
                settings.timeout_seconds.is_finite()
                    && settings.timeout_seconds > 0.0
                    && std::time::Duration::try_from_secs_f64(settings.timeout_seconds).is_ok(),
                "timeout_seconds must be a positive representable finite duration"
            );
            // Zero explicitly disables client pacing, preserving the audited
            // legacy Rust client's unlimited-rate behavior during conversion.
            // Agent transfer limits remain strictly positive.
            resolve_tls_paths(base, &mut settings.tls)?;
            Ok(serde_json::to_value(settings)?)
        }
        RuntimeConfigKind::Agent => {
            let mut settings: crate::agent::AgentConfig = serde_json::from_value(value)?;
            settings.coordinator_url = normalize_https_origin(&settings.coordinator_url)?;
            ensure!(
                (1..=256).contains(&settings.max_workers),
                "max_workers must be in 1..=256"
            );
            ensure!(
                settings.capacity.cpu_millicores > 0 && settings.capacity.ram_mib > 0,
                "agent capacity requires positive CPU and RAM"
            );
            ensure!(
                settings.max_transfer_bytes_per_second > 0 && settings.max_spool_bytes > 0,
                "transfer and spool limits must be positive"
            );
            if let Some(cpus) = &settings.cpu_affinity {
                let unique: std::collections::BTreeSet<_> = cpus.iter().collect();
                ensure!(
                    !cpus.is_empty() && unique.len() == cpus.len(),
                    "cpu_affinity requires unique CPU IDs"
                );
            }
            resolve_tls_paths(base, &mut settings.tls)?;
            Ok(serde_json::to_value(settings)?)
        }
        RuntimeConfigKind::Coordinator => {
            // Internally tagged enum deserialization otherwise ignores variant
            // extras; runtime settings reject them rather than discarding them.
            if let Some(clients) = value.get("clients").and_then(Value::as_object) {
                for role in clients.values() {
                    let fields = role.as_object().context("client role must be a table")?;
                    let allowed: &[&str] =
                        if fields.get("role").and_then(Value::as_str) == Some("node") {
                            &["role", "node_id"]
                        } else {
                            &["role"]
                        };
                    ensure!(
                        fields.keys().all(|key| allowed.contains(&key.as_str())),
                        "unknown client role setting"
                    );
                }
            }
            let mut settings: crate::protocol::CoordinatorConfig = serde_json::from_value(value)?;
            ensure!(
                !settings.storage_profile.is_replayable(),
                "coordinator requires durable storage"
            );
            ensure!(
                settings.lease_ms > 0 && settings.telemetry_ttl_ms > 0,
                "lease and telemetry deadlines must be positive"
            );
            ensure!(
                settings.retry_limit > 0
                    && settings.retry_backoff_ms > 0
                    && settings.retry_backoff_max_ms >= settings.retry_backoff_ms
                    && settings.yield_retry_backoff_ms > 0
                    && settings.yield_retry_backoff_max_ms >= settings.yield_retry_backoff_ms,
                "invalid retry policy"
            );
            ensure!(
                settings.max_artifact_bytes > 0
                    && settings.artifact_quota_bytes >= settings.max_artifact_bytes,
                "invalid artifact limits"
            );
            ensure!(
                !settings.clients.is_empty(),
                "at least one authorized client is required"
            );
            for (fingerprint, principal) in &settings.clients {
                crate::artifacts::validate_hash(fingerprint)?;
                if let crate::protocol::Principal::Node { node_id } = principal {
                    ensure!(
                        !node_id.is_empty()
                            && node_id.len() <= 256
                            && !node_id.chars().any(char::is_control),
                        "invalid authorized node ID"
                    );
                }
            }
            settings.state_dir = resolve_path(base, &settings.state_dir)?;
            resolve_tls_paths(base, &mut settings.tls)?;
            Ok(serde_json::to_value(settings)?)
        }
    }
}

pub(crate) fn resolve_tls_paths(base: &Path, tls: &mut crate::protocol::TlsIdentity) -> Result<()> {
    for path in [&mut tls.ca_cert, &mut tls.certificate, &mut tls.private_key] {
        *path = resolve_path(base, path)?;
    }
    Ok(())
}

/// Eliminate dot components without requiring the target to exist. When `..`
/// follows an existing symlink, resolve that prefix before applying the parent,
/// so normalization cannot redirect a path to the symlink's lexical parent.
pub(crate) fn resolve_path(base: &Path, path: &Path) -> Result<PathBuf> {
    ensure!(
        !path.as_os_str().is_empty(),
        "configuration path cannot be empty"
    );
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    ensure!(
        joined.is_absolute(),
        "resolved configuration path must be absolute"
    );
    let mut resolved = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                match std::fs::canonicalize(&resolved) {
                    Ok(actual) => resolved = actual,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        ensure!(
                            !std::fs::symlink_metadata(&resolved)
                                .is_ok_and(|m| m.file_type().is_symlink()),
                            "cannot resolve parent traversal through a dangling symlink"
                        );
                    }
                    Err(error) => return Err(error.into()),
                }
                resolved.pop();
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    Ok(resolved)
}

/// Return one normalized HTTPS origin, never a path, credential or query URL.
pub fn normalize_https_origin(input: &str) -> Result<String> {
    let result = (|| {
        ensure!(
            !input.chars().any(char::is_whitespace) && !input.contains('\\'),
            "invalid HTTPS origin syntax"
        );
        let (scheme, rest) = input.split_once("://").context("HTTPS origin required")?;
        ensure!(
            scheme.eq_ignore_ascii_case("https"),
            "HTTPS origin required"
        );
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        ensure!(!authority.contains('@'), "origin cannot contain userinfo");
        let port = if authority.starts_with('[') {
            let (_, suffix) = authority.split_once(']').context("invalid IPv6 origin")?;
            if suffix.is_empty() {
                None
            } else {
                Some(
                    suffix
                        .strip_prefix(':')
                        .context("invalid IPv6 origin port")?,
                )
            }
        } else {
            authority.split_once(':').map(|(_, port)| port)
        };
        if let Some(port) = port {
            ensure!(
                !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()),
                "explicit origin port must be an integer"
            );
            let port: u32 = port.parse().context("invalid origin port")?;
            ensure!(
                (1..=65535).contains(&port),
                "origin port must be in 1..=65535"
            );
        }
        let url = reqwest::Url::parse(input).context("invalid HTTPS origin")?;
        ensure!(
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "endpoint must be an HTTPS origin without userinfo, path, query or fragment"
        );
        Ok(url.origin().ascii_serialization())
    })();
    result.map_err(|error: anyhow::Error| config_error(format!("{error:#}")))
}

/// Serialize the public TOML shape; internal state-schema fields never leak into it.
pub fn serialize_runtime<T: Serialize>(settings: &T, kind: RuntimeConfigKind) -> Result<String> {
    let result = (|| {
        let mut value = serde_json::to_value(settings)?;
        let object = value
            .as_object_mut()
            .context("configuration must be an object")?;
        object.remove("schema_version");
        object.insert("config_version".into(), Value::from(CONFIG_VERSION));
        if kind == RuntimeConfigKind::Node
            && let Some(cgroup) = object.get_mut("cgroup").and_then(Value::as_object_mut)
            && let Some(weight) = cgroup.get_mut("cpu_weight")
        {
            *weight = if weight.is_null() {
                serde_json::json!({"mode":"off"})
            } else {
                serde_json::json!({"mode":"set", "value":weight})
            };
        }
        let toml = json_to_toml(value)?;
        Ok(toml::to_string_pretty(&toml)?)
    })();
    result.map_err(|error: anyhow::Error| config_error(format!("{error:#}")))
}

fn json_to_toml(value: Value) -> Result<toml::Value> {
    Ok(match value {
        Value::Null => anyhow::bail!("null is not a TOML value"),
        Value::Bool(value) => toml::Value::Boolean(value),
        Value::String(value) => toml::Value::String(value),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                toml::Value::Integer(value)
            } else if value.is_u64() {
                anyhow::bail!("integer cannot be represented by signed-64-bit TOML")
            } else {
                toml::Value::Float(value.as_f64().context("invalid finite number")?)
            }
        }
        Value::Array(values) => toml::Value::Array(
            values
                .into_iter()
                .map(json_to_toml)
                .collect::<Result<_>>()?,
        ),
        Value::Object(values) => toml::Value::Table(
            values
                .into_iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| Ok((key, json_to_toml(value)?)))
                .collect::<Result<_>>()?,
        ),
    })
}

pub fn example(kind: RuntimeConfigKind) -> Result<String> {
    let text = match kind {
        RuntimeConfigKind::Node => include_str!("../examples/node.toml"),
        RuntimeConfigKind::Coordinator => include_str!("../examples/coordinator.toml"),
        RuntimeConfigKind::Agent => include_str!("../examples/agent.toml"),
        RuntimeConfigKind::Client => include_str!("../examples/client.toml"),
    };
    Ok(text.to_owned())
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeMode {
    Guaranteed,
    #[default]
    Opportunistic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub node_id: String,
    pub node_mode: NodeMode,
    pub state_dir: PathBuf,
    pub storage_profile: crate::state::StorageProfile,
    pub monitor: MonitorConfig,
    pub cpu: CpuConfig,
    pub ram: RamConfig,
    pub gpu: GpuConfig,
    pub lifecycle: LifecycleConfig,
    pub kernel: crate::kernel::KernelConfig,
    pub cgroup: crate::cgroup::CgroupConfig,
    pub execution: ExecutionConfig,
    pub contention: ContentionConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            node_id: "local".into(),
            node_mode: NodeMode::default(),
            state_dir: PathBuf::from(".cedegrid-state"),
            storage_profile: crate::state::StorageProfile::default(),
            monitor: MonitorConfig::default(),
            cpu: CpuConfig::default(),
            ram: RamConfig::default(),
            gpu: GpuConfig::default(),
            lifecycle: LifecycleConfig::default(),
            kernel: crate::kernel::KernelConfig::default(),
            cgroup: crate::cgroup::CgroupConfig::default(),
            execution: ExecutionConfig::default(),
            contention: ContentionConfig::default(),
        }
    }
}

/// How no-harm floors treat Guaranteed allocations. Only A remains; B and C still
/// parse so that loading can name their replacement (constitution 2.0.0).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FloorPolicy {
    #[default]
    A,
    B,
    C,
}

/// Whether floors may push Neutral's memory below its fair share. Only A remains;
/// B still parses so that loading can name its replacement.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NeutralMemoryPolicy {
    #[default]
    A,
    B,
}

/// The `[contention]` section. Missing settings resolve to provisional defaults.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ContentionConfig {
    pub default_mode: crate::contention::ContentionMode,
    /// The most aggressive mode a job may request; unset, the most aggressive
    /// node mode, so no job reaches Performance unless the operator chose it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_mode: Option<crate::contention::ContentionMode>,
    pub floor_policy: FloorPolicy,
    pub neutral_memory_policy: NeutralMemoryPolicy,
    pub acknowledge_caution: std::collections::BTreeSet<String>,
    /// How long a hint-ready Guaranteed allocation may take to release memory
    /// before it is drained (FR-024).
    pub guaranteed_hint_deadline_s: u64,
    /// A floor breach is critical below this share of the floor (FR-063).
    pub critical_floor_percent: u8,
    /// The hint deadline on the critical floor path.
    pub critical_hint_deadline_s: u64,
    /// User IDs below this are system accounts: background demand (FR-059).
    pub uid_min: u32,
    /// Consecutive observations a user needs to join the principals.
    pub principal_window_intervals: u32,
    /// How long a principal's usage must stay below the threshold before it leaves.
    pub principal_leave_s: u64,
    /// Never start own GPU work on a device with external contexts, in any mode
    /// (FR-064).
    pub never_share_occupied_gpus: bool,
    pub cpu: ResourceContentionConfig,
    pub ram: ResourceContentionConfig,
    pub gpu_compute: ResourceContentionConfig,
    pub gpu_memory: ResourceContentionConfig,
}

impl Default for ContentionConfig {
    fn default() -> Self {
        Self {
            default_mode: crate::contention::ContentionMode::Neutral,
            max_mode: None,
            floor_policy: FloorPolicy::A,
            neutral_memory_policy: NeutralMemoryPolicy::A,
            acknowledge_caution: Default::default(),
            guaranteed_hint_deadline_s: 5,
            critical_floor_percent: 50,
            critical_hint_deadline_s: 1,
            uid_min: 1000,
            principal_window_intervals: 4,
            principal_leave_s: 60,
            never_share_occupied_gpus: false,
            cpu: Default::default(),
            ram: Default::default(),
            gpu_compute: Default::default(),
            gpu_memory: Default::default(),
        }
    }
}

/// Caution options a file acknowledges by name (FR-051, FR-054).
const ACKNOWLEDGEMENTS: [&str; 4] = [
    "performance_mode",
    "small_floor",
    "reduced_margin",
    "relaxed_detection",
];

/// Floors are never smaller than these (FR-023).
pub const MIN_RAM_FLOOR_MIB: u64 = 512;
pub const MIN_VRAM_FLOOR_MIB: u64 = 512;

/// Settings an operator wrote for one resource; unset keys keep their defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ResourceContentionConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<crate::contention::ContentionMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yield_first_margin_percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub neutral_margin_percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub performance_margin_percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yield_first_response_intervals: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale_up_step_interval_s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub neutral_convergence_s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub neutral_tolerance_points: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub performance_ramp_s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub performance_ramp_percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reclaim_s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint_deadline_s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown_window_intervals: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hysteresis_percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_step_dwell_intervals: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pause_bound_s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub noise_tolerance_mib: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quiet_window_intervals: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale_up_cooldown_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal_min_percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guaranteed_breach_window_s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_peak_window_s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saturation_percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reclaim_backoff_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reclaim_backoff_max_ms: Option<u64>,
}

impl ContentionConfig {
    fn resource(&self, kind: crate::contention::ResourceKind) -> &ResourceContentionConfig {
        use crate::contention::ResourceKind::*;
        match kind {
            Cpu => &self.cpu,
            Ram => &self.ram,
            GpuCompute => &self.gpu_compute,
            GpuMemory => &self.gpu_memory,
        }
    }

    /// The node's mode for a resource before any per-job override.
    pub fn mode(&self, kind: crate::contention::ResourceKind) -> crate::contention::ContentionMode {
        self.resource(kind).mode.unwrap_or(self.default_mode)
    }

    /// Effective thresholds for a resource with defaults applied.
    pub fn thresholds(
        &self,
        kind: crate::contention::ResourceKind,
    ) -> crate::contention::ResourceThresholds {
        let set = self.resource(kind);
        let defaults = crate::contention::ResourceThresholds::defaults(kind);
        crate::contention::ResourceThresholds {
            yield_first_margin_percent: set
                .yield_first_margin_percent
                .unwrap_or(defaults.yield_first_margin_percent),
            neutral_margin_percent: set
                .neutral_margin_percent
                .unwrap_or(defaults.neutral_margin_percent),
            performance_margin_percent: set
                .performance_margin_percent
                .unwrap_or(defaults.performance_margin_percent),
            yield_first_response_intervals: set
                .yield_first_response_intervals
                .unwrap_or(defaults.yield_first_response_intervals),
            scale_up_step_interval_s: set.scale_up_step_interval_s,
            neutral_convergence_s: set
                .neutral_convergence_s
                .unwrap_or(defaults.neutral_convergence_s),
            neutral_tolerance_points: set
                .neutral_tolerance_points
                .unwrap_or(defaults.neutral_tolerance_points),
            performance_ramp_s: set
                .performance_ramp_s
                .unwrap_or(defaults.performance_ramp_s),
            performance_ramp_percent: set
                .performance_ramp_percent
                .unwrap_or(defaults.performance_ramp_percent),
            reclaim_s: set.reclaim_s.unwrap_or(defaults.reclaim_s),
            hint_deadline_s: set.hint_deadline_s.unwrap_or(defaults.hint_deadline_s),
            unknown_window_intervals: set
                .unknown_window_intervals
                .unwrap_or(defaults.unknown_window_intervals),
            hysteresis_percent: set
                .hysteresis_percent
                .unwrap_or(defaults.hysteresis_percent),
            min_step_dwell_intervals: set
                .min_step_dwell_intervals
                .unwrap_or(defaults.min_step_dwell_intervals),
            pause_bound_s: set.pause_bound_s.unwrap_or(defaults.pause_bound_s),
            noise_tolerance_mib: set
                .noise_tolerance_mib
                .unwrap_or(defaults.noise_tolerance_mib),
            quiet_window_intervals: set
                .quiet_window_intervals
                .unwrap_or(defaults.quiet_window_intervals),
            scale_up_cooldown_ms: set.scale_up_cooldown_ms.or(defaults.scale_up_cooldown_ms),
            principal_min_percent: set
                .principal_min_percent
                .unwrap_or(defaults.principal_min_percent),
            guaranteed_breach_window_s: set
                .guaranteed_breach_window_s
                .unwrap_or(defaults.guaranteed_breach_window_s),
            memory_peak_window_s: set
                .memory_peak_window_s
                .unwrap_or(defaults.memory_peak_window_s),
            saturation_percent: set
                .saturation_percent
                .unwrap_or(defaults.saturation_percent),
            reclaim_backoff_ms: set
                .reclaim_backoff_ms
                .unwrap_or(defaults.reclaim_backoff_ms),
            reclaim_backoff_max_ms: set
                .reclaim_backoff_max_ms
                .unwrap_or(defaults.reclaim_backoff_max_ms),
        }
    }

    /// The node cap: the configured `max_mode`, else the most aggressive node mode.
    pub fn max_mode(&self) -> crate::contention::ContentionMode {
        self.max_mode.unwrap_or_else(|| {
            crate::contention::ResourceKind::ALL
                .into_iter()
                .map(|kind| self.mode(kind))
                .max()
                .unwrap_or(self.default_mode)
        })
    }

    fn performance_chosen(&self) -> bool {
        self.max_mode() == crate::contention::ContentionMode::Performance
    }

    /// GPU quiet window in monitor intervals. An unset window is raised from its
    /// default until it spans longer than the GPU sample age, so a fast monitor
    /// never declares Quiet from evidence a stale sample could still explain.
    pub fn quiet_window_intervals(
        &self,
        monitor_interval_ms: u64,
        gpu_sample_max_age_ms: u64,
    ) -> u32 {
        let configured = self.thresholds(crate::contention::ResourceKind::GpuCompute);
        if self.gpu_compute.quiet_window_intervals.is_some() {
            return configured.quiet_window_intervals;
        }
        let spanning = gpu_sample_max_age_ms / monitor_interval_ms.max(1) + 1;
        configured
            .quiet_window_intervals
            .max(u32::try_from(spanning).unwrap_or(u32::MAX))
    }

    /// The shortest hint deadline of any resource, for the supervisor's targets.
    pub fn hint_deadline_ms(&self) -> u64 {
        crate::contention::ResourceKind::ALL
            .into_iter()
            .map(|kind| self.thresholds(kind).hint_deadline_s.saturating_mul(1000))
            .min()
            .unwrap_or(5000)
    }

    fn validate(&self, monitor_interval_ms: u64, gpu_sample_max_age_ms: u64) -> Result<()> {
        use crate::contention::ResourceKind::{self, *};
        if let Some(removed) = match self.floor_policy {
            FloorPolicy::A => None,
            FloorPolicy::B => Some("b"),
            FloorPolicy::C => Some("c"),
        } {
            anyhow::bail!(
                "contention.floor_policy = \"{removed}\" was removed: Guaranteed memory is released last under policy A; tune contention.guaranteed_hint_deadline_s and contention.ram.guaranteed_breach_window_s"
            );
        }
        ensure!(
            self.neutral_memory_policy == NeutralMemoryPolicy::A,
            "contention.neutral_memory_policy = \"b\" was removed: floors always come before the fair share; size them with ram.reserve_mib, ram.reserve_percent, gpu.reserve_vram_mib and gpu.reserve_vram_percent"
        );
        for name in &self.acknowledge_caution {
            ensure!(
                ACKNOWLEDGEMENTS.contains(&name.as_str()),
                "contention.acknowledge_caution has unknown entry {name:?}"
            );
        }
        ensure!(
            self.default_mode <= self.max_mode(),
            "contention.default_mode cannot be more aggressive than contention.max_mode"
        );
        ensure!(
            !self.performance_chosen() || self.acknowledge_caution.contains("performance_mode"),
            "ERR_CEDEGRID_CAUTION_UNACKNOWLEDGED: Performance mode requires acknowledge_caution = [\"performance_mode\"]"
        );
        ensure!(
            (1..=30).contains(&self.guaranteed_hint_deadline_s),
            "contention.guaranteed_hint_deadline_s must be between 1 and 30"
        );
        ensure!(
            (10..=90).contains(&self.critical_floor_percent),
            "contention.critical_floor_percent must be between 10 and 90"
        );
        ensure!(
            self.principal_window_intervals >= 1,
            "contention.principal_window_intervals must be at least 1"
        );
        ensure!(
            (1..=3600).contains(&self.principal_leave_s),
            "contention.principal_leave_s must be between 1 and 3600"
        );
        let shortest_hint = [Ram, GpuMemory]
            .into_iter()
            .map(|kind| self.thresholds(kind).hint_deadline_s)
            .chain([self.guaranteed_hint_deadline_s])
            .min()
            .unwrap_or(1);
        ensure!(
            (1..=shortest_hint).contains(&self.critical_hint_deadline_s),
            "contention.critical_hint_deadline_s must be at least 1 and at most every memory hint deadline"
        );
        for kind in ResourceKind::ALL {
            let name = kind.name();
            let set = self.resource(kind);
            let applies = |field: &str, allowed: &[ResourceKind], present: bool| -> Result<()> {
                ensure!(
                    !present || allowed.contains(&kind),
                    "contention.{name}.{field} does not apply to {name}"
                );
                Ok(())
            };
            applies(
                "performance_margin_percent",
                &[Cpu, GpuCompute],
                set.performance_margin_percent.is_some(),
            )?;
            applies(
                "neutral_margin_percent",
                &[Cpu, GpuCompute],
                set.neutral_margin_percent.is_some(),
            )?;
            applies(
                "yield_first_response_intervals",
                &[Cpu, GpuCompute],
                set.yield_first_response_intervals.is_some(),
            )?;
            applies(
                "pause_bound_s",
                &[Cpu, GpuCompute],
                set.pause_bound_s.is_some(),
            )?;
            applies(
                "noise_tolerance_mib",
                &[GpuMemory],
                set.noise_tolerance_mib.is_some(),
            )?;
            applies(
                "quiet_window_intervals",
                &[GpuCompute],
                set.quiet_window_intervals.is_some(),
            )?;
            applies(
                "scale_up_cooldown_ms",
                &[Cpu, Ram],
                set.scale_up_cooldown_ms.is_some(),
            )?;
            applies(
                "guaranteed_breach_window_s",
                &[Ram],
                set.guaranteed_breach_window_s.is_some(),
            )?;
            applies("reclaim_s", &[Cpu, GpuCompute], set.reclaim_s.is_some())?;
            applies(
                "saturation_percent",
                &[Cpu, GpuCompute],
                set.saturation_percent.is_some(),
            )?;
            for (field, present) in [
                ("memory_peak_window_s", set.memory_peak_window_s.is_some()),
                ("reclaim_backoff_ms", set.reclaim_backoff_ms.is_some()),
                (
                    "reclaim_backoff_max_ms",
                    set.reclaim_backoff_max_ms.is_some(),
                ),
            ] {
                applies(field, &[Ram, GpuMemory], present)?;
            }
            ensure!(
                self.mode(kind) <= self.max_mode(),
                "contention.{name}.mode cannot be more aggressive than contention.max_mode"
            );
            let t = self.thresholds(kind);
            ensure!(
                (50..=100).contains(&t.saturation_percent),
                "contention.{name}.saturation_percent must be between 50 and 100"
            );
            // Bounded deadlines and windows keep escalation bounded in time (III).
            ensure!(
                t.hint_deadline_s <= 30,
                "contention.{name}.hint_deadline_s must be at most 30"
            );
            ensure!(
                t.unknown_window_intervals <= 20,
                "contention.{name}.unknown_window_intervals must be at most 20"
            );
            for (field, value, maximum) in [
                (
                    "min_step_dwell_intervals",
                    u64::from(t.min_step_dwell_intervals),
                    10,
                ),
                (
                    "yield_first_response_intervals",
                    u64::from(t.yield_first_response_intervals),
                    10,
                ),
                ("reclaim_s", t.reclaim_s, 60),
                ("pause_bound_s", t.pause_bound_s, 60),
            ] {
                ensure!(
                    value <= maximum,
                    "contention.{name}.{field} must be at most {maximum}"
                );
            }
            ensure!(
                (1..=90).contains(&t.yield_first_margin_percent),
                "contention.{name}.yield_first_margin_percent must be between 1 and 90"
            );
            if matches!(kind, Cpu | GpuCompute) {
                ensure!(
                    t.performance_margin_percent <= 50,
                    "contention.{name}.performance_margin_percent must be between 0 and 50"
                );
                ensure!(
                    t.yield_first_margin_percent > t.performance_margin_percent,
                    "contention.{name}.yield_first_margin_percent must exceed performance_margin_percent"
                );
                // Ordered margins keep the targets ordered (FR-065).
                ensure!(
                    t.yield_first_margin_percent >= t.neutral_margin_percent
                        && t.neutral_margin_percent >= t.performance_margin_percent,
                    "contention.{name}.neutral_margin_percent must be between performance_margin_percent and yield_first_margin_percent"
                );
            }
            for (field, value) in [
                (
                    "yield_first_response_intervals",
                    u64::from(t.yield_first_response_intervals),
                ),
                (
                    "scale_up_step_interval_s",
                    t.scale_up_step_interval_s.unwrap_or(1),
                ),
                ("neutral_convergence_s", t.neutral_convergence_s),
                ("performance_ramp_s", t.performance_ramp_s),
                ("reclaim_s", t.reclaim_s),
                ("hint_deadline_s", t.hint_deadline_s),
                (
                    "unknown_window_intervals",
                    u64::from(t.unknown_window_intervals),
                ),
                (
                    "min_step_dwell_intervals",
                    u64::from(t.min_step_dwell_intervals),
                ),
                ("pause_bound_s", t.pause_bound_s),
                (
                    "quiet_window_intervals",
                    u64::from(t.quiet_window_intervals),
                ),
            ] {
                ensure!(value >= 1, "contention.{name}.{field} must be at least 1");
            }
            ensure!(
                (1..=50).contains(&t.neutral_tolerance_points),
                "contention.{name}.neutral_tolerance_points must be between 1 and 50"
            );
            ensure!(
                (1..=100).contains(&t.performance_ramp_percent),
                "contention.{name}.performance_ramp_percent must be between 1 and 100"
            );
            ensure!(
                (1..=99).contains(&t.hysteresis_percent),
                "contention.{name}.hysteresis_percent must be between 1 and 99"
            );
            ensure!(
                (1..=50).contains(&t.principal_min_percent),
                "contention.{name}.principal_min_percent must be between 1 and 50"
            );
            ensure!(
                t.guaranteed_breach_window_s <= 60,
                "contention.{name}.guaranteed_breach_window_s must be between 0 and 60"
            );
            ensure!(
                t.memory_peak_window_s >= 1 && t.reclaim_backoff_ms >= 1,
                "contention.{name}.memory_peak_window_s and reclaim_backoff_ms must be at least 1"
            );
            ensure!(
                t.reclaim_backoff_max_ms >= t.reclaim_backoff_ms,
                "contention.{name}.reclaim_backoff_max_ms must be at least reclaim_backoff_ms"
            );
            if let Some(window) = set.quiet_window_intervals {
                ensure!(
                    u64::from(window).saturating_mul(monitor_interval_ms) > gpu_sample_max_age_ms,
                    "contention.gpu_compute.quiet_window_intervals must span longer than gpu.process_sample_max_age_ms"
                );
            }
        }
        for (option, settings) in [
            ("reduced_margin", self.reduced_margins()),
            ("relaxed_detection", self.relaxed_detection()),
        ] {
            ensure!(
                settings.is_empty() || self.acknowledge_caution.contains(option),
                "ERR_CEDEGRID_CAUTION_UNACKNOWLEDGED: {} beyond the default requires acknowledge_caution = [\"{option}\"]",
                settings.join(", ")
            );
        }
        Ok(())
    }

    /// Margins below their defaults, which leave external demand less room
    /// (constitution II).
    fn reduced_margins(&self) -> Vec<String> {
        self.beyond_defaults(|t| {
            [
                ("yield_first_margin_percent", t.yield_first_margin_percent),
                ("neutral_margin_percent", t.neutral_margin_percent),
                ("performance_margin_percent", t.performance_margin_percent),
            ]
            .map(|(key, value)| (key, -i64::from(value)))
        })
    }

    /// Detection thresholds above their defaults, which let more external demand
    /// go unseen (constitution II).
    fn relaxed_detection(&self) -> Vec<String> {
        self.beyond_defaults(|t| {
            [
                ("saturation_percent", i64::from(t.saturation_percent)),
                ("principal_min_percent", i64::from(t.principal_min_percent)),
                (
                    "noise_tolerance_mib",
                    i64::try_from(t.noise_tolerance_mib).unwrap_or(i64::MAX),
                ),
            ]
        })
    }

    /// Settings, as `contention.resource.key`, whose value exceeds the default;
    /// `values` orders each so that larger lets own work hold more.
    fn beyond_defaults<const N: usize>(
        &self,
        values: impl Fn(&crate::contention::ResourceThresholds) -> [(&'static str, i64); N],
    ) -> Vec<String> {
        use crate::contention::{ResourceKind, ResourceThresholds};
        let mut result = Vec::new();
        for kind in ResourceKind::ALL {
            let defaults = values(&ResourceThresholds::defaults(kind));
            for ((key, value), (_, default)) in
                values(&self.thresholds(kind)).into_iter().zip(defaults)
            {
                if value > default {
                    result.push(format!("contention.{}.{key}", kind.name()));
                }
            }
        }
        result
    }
}

/// One no-harm floor as it applies on a host (FR-023).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FloorStatus {
    /// The larger of the fixed and the percentage setting.
    pub configured_mib: u64,
    pub minimum_mib: u64,
    pub effective_mib: u64,
    /// Below twice the minimum: a caution option.
    pub near_minimum: bool,
    /// `acknowledge_caution` contains `small_floor`.
    pub confirmed: bool,
    /// Near the minimum and unconfirmed, so it applies as twice the minimum.
    pub raised: bool,
}

/// Every floor on a host, as `doctor` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FloorReport {
    pub ram: FloorStatus,
    pub gpus: std::collections::BTreeMap<String, FloorStatus>,
}

/// How a legacy GPU setting was read (FR-031).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyGpu {
    pub execution_mode: GpuExecutionMode,
    pub interpretation: &'static str,
    pub warnings: Vec<String>,
}

/// A node configuration plus the warnings an operator should see at load time.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionConfig {
    pub enabled: bool,
    pub prepare_timeout_ms: u64,
    pub admission_timeout_ms: u64,
    pub release_confirm_timeout_ms: u64,
}
impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            prepare_timeout_ms: 10_000,
            admission_timeout_ms: 60_000,
            release_confirm_timeout_ms: 10_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MonitorConfig {
    pub interval_ms: u64,
}
impl Default for MonitorConfig {
    fn default() -> Self {
        Self { interval_ms: 500 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CpuConfig {
    pub reserve_physical_cores: u32,
    pub nice: i32,
}
impl Default for CpuConfig {
    fn default() -> Self {
        Self {
            reserve_physical_cores: 2,
            nice: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RamConfig {
    pub reserve_mib: u64,
    pub reserve_percent: u8,
}
impl Default for RamConfig {
    fn default() -> Self {
        Self {
            reserve_mib: 8192,
            reserve_percent: 5,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GpuExecutionMode {
    #[default]
    Auto,
    ContentionAware,
    ConservativeNonSharing,
    /// Explicitly weaker occupied sharing with device-scoped external identities.
    BestEffortOccupied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GpuConfig {
    /// Auto uses fresh process telemetry when available, otherwise empty-device
    /// compatibility. Explicit contention-aware mode never silently falls back.
    pub execution_mode: GpuExecutionMode,
    /// Operator authorization for independently tracked external competitors only.
    /// Every observed external context must match an identity on this device.
    /// Empty by default; same UID or executable name alone never grants permission.
    pub best_effort_external_processes:
        std::collections::BTreeMap<String, Vec<crate::model::ExternalGpuProcessIdentity>>,
    /// Maximum interval since a successful driver timestamp baseline. A longer
    /// gap discards buffered history before process activity can be trusted again.
    pub process_sample_max_age_ms: u64,
    pub reserve_vram_mib: u64,
    /// The per-device floor is also at least this share of the device (FR-023).
    pub reserve_vram_percent: u8,
    pub scale_up_cooldown_ms: u64,
    pub protective_shrink_percent: u8,
    pub active_shrink_percent: u8,
}
impl Default for GpuConfig {
    fn default() -> Self {
        Self {
            execution_mode: GpuExecutionMode::Auto,
            best_effort_external_processes: std::collections::BTreeMap::new(),
            process_sample_max_age_ms: 2000,
            reserve_vram_mib: 3072,
            reserve_vram_percent: 5,
            scale_up_cooldown_ms: 30_000,
            protective_shrink_percent: 25,
            active_shrink_percent: 50,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LifecycleConfig {
    /// Cooperative wait only; not a resource-release guarantee.
    pub drain_timeout_ms: u64,
    /// Additional wait after SIGTERM before requesting forced termination.
    pub term_grace_ms: u64,
    pub heartbeat_interval_ms: u64,
    /// Only authoritative coordinator renewal can extend a remote lease.
    pub allocation_lease_ms: u64,
}
impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            drain_timeout_ms: 3000,
            term_grace_ms: 2000,
            heartbeat_interval_ms: 2000,
            allocation_lease_ms: 10_000,
        }
    }
}

impl Config {
    /// Read only versioned TOML, resolving paths against the canonical file location.
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self::load_reporting(path)?.config)
    }

    /// Load and apply compatibility rules, returning operator warnings.
    pub fn load_reporting(path: &Path) -> Result<LoadedConfig> {
        let mut config: Config = load_runtime(path, RuntimeConfigKind::Node)?;
        let warnings = config.apply_compatibility();
        Ok(LoadedConfig { config, warnings })
    }

    /// Parse against an already canonical location, like [`Config::load_reporting`].
    pub fn parse_reporting(contents: &str, canonical_config_path: &Path) -> Result<LoadedConfig> {
        let mut config: Config =
            parse_runtime(contents, canonical_config_path, RuntimeConfigKind::Node)?;
        let warnings = config.apply_compatibility();
        Ok(LoadedConfig { config, warnings })
    }

    /// Clamp settings older files may still carry and describe active caution options.
    fn apply_compatibility(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.cpu.nice < 0 {
            warnings.push(format!(
                "cpu.nice = {} would raise priority above the default; treated as 0",
                self.cpu.nice
            ));
            self.cpu.nice = 0;
        }
        if self.ram.reserve_mib < MIN_RAM_FLOOR_MIB {
            warnings.push(format!(
                "ram.reserve_mib = {} is below the {MIN_RAM_FLOOR_MIB} MiB minimum floor; the RAM floor is at least {MIN_RAM_FLOOR_MIB} MiB",
                self.ram.reserve_mib
            ));
        }
        if self.gpu.reserve_vram_mib < MIN_VRAM_FLOOR_MIB {
            warnings.push(format!(
                "gpu.reserve_vram_mib = {} is below the {MIN_VRAM_FLOOR_MIB} MiB minimum floor; each GPU floor is at least {MIN_VRAM_FLOOR_MIB} MiB",
                self.gpu.reserve_vram_mib
            ));
        }
        if self.small_floor() && !self.contention.acknowledge_caution.contains("small_floor") {
            warnings.push(format!(
                "a no-harm floor near its minimum is raised to twice the minimum ({} MiB of RAM, {} MiB per GPU); keep it with acknowledge_caution = [\"small_floor\"]",
                2 * MIN_RAM_FLOOR_MIB,
                2 * MIN_VRAM_FLOOR_MIB
            ));
        }
        for option in self.caution_options() {
            warnings.push(format!(
                "caution option {} is active [caution]",
                option.name
            ));
        }
        // Legacy GPU values are read once, here (FR-031); explicit modes win.
        let legacy = self.legacy_gpu();
        match self.gpu.execution_mode {
            GpuExecutionMode::Auto => {}
            GpuExecutionMode::ContentionAware | GpuExecutionMode::BestEffortOccupied => {
                // Only an explicit cap limits it; the implicit cap follows node modes.
                let neutral = crate::contention::ContentionMode::Neutral;
                let neutral = self
                    .contention
                    .max_mode
                    .map_or(neutral, |cap| neutral.min(cap));
                for set in [
                    &mut self.contention.gpu_compute,
                    &mut self.contention.gpu_memory,
                ] {
                    set.mode.get_or_insert(neutral);
                }
            }
            GpuExecutionMode::ConservativeNonSharing => {
                self.contention.never_share_occupied_gpus = true;
            }
        }
        warnings.extend(legacy.warnings);
        warnings
    }

    /// How the legacy `gpu.execution_mode` and `gpu.best_effort_external_processes`
    /// are read (FR-031); `doctor` shows it.
    pub fn legacy_gpu(&self) -> LegacyGpu {
        let mut warnings = Vec::new();
        let interpretation = match self.gpu.execution_mode {
            GpuExecutionMode::Auto => "the contention mode settings apply",
            GpuExecutionMode::ContentionAware => {
                warnings.push("deprecated: gpu.execution_mode = \"contention_aware\" is read as Neutral; set contention.gpu_compute.mode and contention.gpu_memory.mode".into());
                "GPU compute and GPU memory run in Neutral unless their mode is set"
            }
            GpuExecutionMode::ConservativeNonSharing => {
                warnings.push("deprecated: gpu.execution_mode = \"conservative_non_sharing\" is read as contention.never_share_occupied_gpus = true; set that key".into());
                "contention.never_share_occupied_gpus = true"
            }
            GpuExecutionMode::BestEffortOccupied => {
                warnings.push("deprecated: gpu.execution_mode = \"best_effort_occupied\" is read as Neutral; gpu.best_effort_external_processes is no longer used".into());
                "GPU compute and GPU memory run in Neutral unless their mode is set"
            }
        };
        if self.gpu.execution_mode != GpuExecutionMode::BestEffortOccupied
            && !self.gpu.best_effort_external_processes.is_empty()
        {
            warnings
                .push("deprecated: gpu.best_effort_external_processes is no longer used".into());
        }
        LegacyGpu {
            execution_mode: self.gpu.execution_mode,
            interpretation,
            warnings,
        }
    }

    /// The RAM floor on a host with `total_mib` of RAM (FR-023).
    pub fn ram_floor(&self, total_mib: u64) -> FloorStatus {
        let percent = total_mib
            .saturating_mul(u64::from(self.ram.reserve_percent))
            .div_ceil(100);
        self.floor_status(self.ram.reserve_mib.max(percent), MIN_RAM_FLOOR_MIB)
    }

    /// The floor on a GPU device with `total_mib` of memory (FR-023).
    pub fn vram_floor(&self, total_mib: u64) -> FloorStatus {
        let percent = total_mib
            .saturating_mul(u64::from(self.gpu.reserve_vram_percent))
            .div_ceil(100);
        self.floor_status(self.gpu.reserve_vram_mib.max(percent), MIN_VRAM_FLOOR_MIB)
    }

    pub fn ram_floor_mib(&self, total_mib: u64) -> u64 {
        self.ram_floor(total_mib).effective_mib
    }

    pub fn vram_floor_mib(&self, total_mib: u64) -> u64 {
        self.vram_floor(total_mib).effective_mib
    }

    /// A floor never goes below its minimum. Near the minimum it is a caution
    /// option: without its acknowledgement it applies as twice the minimum, so it
    /// is never active unconfirmed.
    fn floor_status(&self, configured_mib: u64, minimum_mib: u64) -> FloorStatus {
        let floor = configured_mib.max(minimum_mib);
        let near_minimum = floor < 2 * minimum_mib;
        let confirmed = self.contention.acknowledge_caution.contains("small_floor");
        FloorStatus {
            configured_mib,
            minimum_mib,
            effective_mib: if near_minimum && !confirmed {
                2 * minimum_mib
            } else {
                floor
            },
            near_minimum,
            confirmed,
            raised: near_minimum && !confirmed,
        }
    }

    /// Every floor as it applies on the observed host, for `doctor` and reports.
    pub fn floors(&self, snapshot: &crate::model::Snapshot) -> FloorReport {
        FloorReport {
            ram: self.ram_floor(snapshot.total_ram_mib),
            gpus: snapshot
                .gpus
                .iter()
                .map(|gpu| (gpu.uuid.clone(), self.vram_floor(gpu.total_memory_mib)))
                .collect(),
        }
    }

    /// Warnings for floors a percentage puts near the minimum on this host, which
    /// the load-time warning cannot judge (FR-054).
    pub fn host_floor_warnings(&self, snapshot: &crate::model::Snapshot) -> Vec<String> {
        let floors = self.floors(snapshot);
        let ram =
            (self.ram.reserve_percent != 0).then(|| ("the RAM floor".to_string(), &floors.ram));
        let gpus = floors
            .gpus
            .iter()
            .filter(|_| self.gpu.reserve_vram_percent != 0)
            .map(|(uuid, floor)| (format!("the floor of GPU {uuid}"), floor));
        ram.into_iter()
            .chain(gpus)
            .filter(|(_, floor)| floor.raised)
            .map(|(name, floor)| {
                format!(
                    "{name} of {} MiB is near its {} MiB minimum and applies as {} MiB; keep it with acknowledge_caution = [\"small_floor\"]",
                    floor.configured_mib, floor.minimum_mib, floor.effective_mib
                )
            })
            .collect()
    }

    /// A floor below twice its minimum, judged from the settings alone; floors set
    /// by a percentage are judged on the host by [`Config::floors`].
    fn small_floor(&self) -> bool {
        (self.ram.reserve_percent == 0 && self.ram.reserve_mib < 2 * MIN_RAM_FLOOR_MIB)
            || (self.gpu.reserve_vram_percent == 0
                && self.gpu.reserve_vram_mib < 2 * MIN_VRAM_FLOOR_MIB)
    }

    /// Active caution options on the observed host, with floors judged after their
    /// percentages are applied (FR-043).
    pub fn caution_options_on(
        &self,
        snapshot: &crate::model::Snapshot,
    ) -> Vec<crate::model::ExceptionNotice> {
        let floors = self.floors(snapshot);
        let small = std::iter::once(&floors.ram)
            .chain(floors.gpus.values())
            .any(|floor| floor.near_minimum && floor.confirmed);
        let mut result = self.caution_options();
        if small && !result.iter().any(|option| option.name == "small_floor") {
            result.push(crate::model::ExceptionNotice {
                name: "small_floor".into(),
                marker: crate::model::RiskMarker::Caution,
            });
        }
        result
    }

    /// Active caution options with their markers (FR-043, FR-045).
    pub fn caution_options(&self) -> Vec<crate::model::ExceptionNotice> {
        use crate::model::{ExceptionNotice, RiskMarker};
        let mut result = Vec::new();
        if self.contention.performance_chosen() {
            result.push(ExceptionNotice {
                name: "performance_mode".into(),
                marker: RiskMarker::Caution,
            });
        }
        if self.small_floor() && self.contention.acknowledge_caution.contains("small_floor") {
            result.push(ExceptionNotice {
                name: "small_floor".into(),
                marker: RiskMarker::Caution,
            });
        }
        // Loading refuses these unacknowledged, so in effect means active.
        for (name, settings) in [
            ("reduced_margin", self.contention.reduced_margins()),
            ("relaxed_detection", self.contention.relaxed_detection()),
        ] {
            if !settings.is_empty() {
                result.push(ExceptionNotice {
                    name: name.into(),
                    marker: RiskMarker::Caution,
                });
            }
        }
        result
    }

    /// The launch-time nice value. Own priority is never raised above the default.
    pub fn effective_nice(&self) -> i32 {
        self.cpu.nice.max(0)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            crate::model::supported_schema(self.schema_version),
            "unsupported configuration schema_version {}",
            self.schema_version
        );
        ensure!(
            !self.node_id.is_empty() && self.node_id.len() <= 128,
            "node_id must contain 1 to 128 ASCII characters"
        );
        ensure!(
            self.node_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)),
            "node_id may contain only ASCII letters, digits, '.', '_' and '-'"
        );
        ensure!(
            self.node_id != "." && self.node_id != "..",
            "node_id cannot be '.' or '..'"
        );
        ensure!(
            !self.state_dir.as_os_str().is_empty(),
            "state_dir cannot be empty"
        );
        ensure!(
            self.monitor.interval_ms > 0,
            "monitor.interval_ms must be positive"
        );
        self.kernel.validate().map_err(anyhow::Error::msg)?;
        self.cgroup.validate()?;
        ensure!(
            self.execution.release_confirm_timeout_ms > 0,
            "execution.release_confirm_timeout_ms must be positive"
        );
        ensure!(
            self.execution.prepare_timeout_ms > 0,
            "execution.prepare_timeout_ms must be positive"
        );
        ensure!(
            self.execution.admission_timeout_ms > 0,
            "execution.admission_timeout_ms must be positive"
        );
        ensure!(
            (-20..=19).contains(&self.cpu.nice),
            "cpu.nice must be between -20 and 19; availability of priority changes is capability-dependent"
        );
        ensure!(
            self.ram.reserve_percent <= 100,
            "ram.reserve_percent must be between 0 and 100"
        );
        ensure!(
            self.gpu.reserve_vram_percent <= 100,
            "gpu.reserve_vram_percent must be between 0 and 100"
        );
        ensure!(
            self.gpu.process_sample_max_age_ms > 0,
            "gpu.process_sample_max_age_ms must be positive"
        );
        ensure!(
            self.gpu.execution_mode != GpuExecutionMode::BestEffortOccupied
                || !self.gpu.best_effort_external_processes.is_empty(),
            "best_effort_occupied requires explicit device-scoped external process identities"
        );
        for (uuid, identities) in &self.gpu.best_effort_external_processes {
            ensure!(
                !uuid.trim().is_empty() && !identities.is_empty(),
                "best-effort external authorization requires a device UUID and identities"
            );
            let mut pids = std::collections::BTreeSet::new();
            for identity in identities {
                ensure!(
                    identity.pid > 0
                        && identity.start_ticks > 0
                        && !identity.boot_id.trim().is_empty()
                        && pids.insert(identity.pid),
                    "best-effort external authorization requires unique PIDs, boot identities, and native start ticks per device"
                );
            }
        }
        ensure!(
            (1..=100).contains(&self.gpu.protective_shrink_percent),
            "gpu.protective_shrink_percent must be between 1 and 100"
        );
        ensure!(
            (1..=100).contains(&self.gpu.active_shrink_percent),
            "gpu.active_shrink_percent must be between 1 and 100"
        );
        ensure!(
            self.gpu.active_shrink_percent >= self.gpu.protective_shrink_percent,
            "gpu.active_shrink_percent must be at least protective_shrink_percent"
        );
        ensure!(
            self.lifecycle.heartbeat_interval_ms > 0,
            "lifecycle.heartbeat_interval_ms must be positive"
        );
        ensure!(
            self.lifecycle.allocation_lease_ms > self.lifecycle.heartbeat_interval_ms,
            "lifecycle.allocation_lease_ms must exceed heartbeat_interval_ms"
        );
        self.lifecycle
            .drain_timeout_ms
            .checked_add(self.lifecycle.term_grace_ms)
            .and_then(|v| v.checked_add(self.lifecycle.allocation_lease_ms))
            .context("lifecycle deadline sum overflows milliseconds")?;
        self.contention
            .validate(self.monitor.interval_ms, self.gpu.process_sample_max_age_ms)?;
        Ok(())
    }
}
