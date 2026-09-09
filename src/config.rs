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
        }
    }
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
        load_runtime(path, RuntimeConfigKind::Node)
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
        Ok(())
    }
}
