//! Portable configuration. Node names and paths are operator choices, never host aliases.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::model::SCHEMA_VERSION;

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
            state_dir: PathBuf::from(".resource-manager-state"),
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
    /// Parse strictly. The caller resolves relative state paths against the config directory.
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("read configuration {}", path.display()))?;
        let config: Self = serde_yaml::from_str(&contents)
            .with_context(|| format!("parse configuration {}", path.display()))?;
        config.validate()?;
        Ok(config)
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
