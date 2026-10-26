//! What the node advertises to the coordinator and reports about contention.
use super::*;

/// Controls this node can apply to new work. Cgroup controls are advertised only
/// when the backend prepares with authorized settings; kernel observations such
/// as PSI are never controls (audit H8).
pub fn advertised_controls(config: &Config, cpu_affinity: bool) -> Vec<String> {
    let mut controls = vec!["cpu.nice".to_string()];
    controls.push(if config.storage_profile.is_replayable() {
        "storage.replayable_local".into()
    } else {
        "storage.durable_local".into()
    });
    if cpu_affinity {
        controls.push("cpu.affinity".into());
    }
    if crate::telemetry::pidfd_capability().status == crate::model::CapabilityStatus::Available {
        controls.push("process_handle".into());
    }
    if config.cgroup.enabled && crate::cgroup::CgroupBackend::new(config.cgroup.clone()).is_ok() {
        controls.extend(config.cgroup.advertised_controls());
    }
    controls
}

/// Coordinator features learned from the latest heartbeat reply. A 0.2
/// coordinator rejects unknown report fields, so negotiated fields are sent only
/// while the last reply advertised them (contracts/wire-negotiation.md).
#[derive(Debug, Default)]
pub(super) struct NegotiatedFeatures {
    pub(super) contention: bool,
}

impl NegotiatedFeatures {
    pub(super) fn learn(&mut self, reply: &HeartbeatReply) {
        self.contention = reply.features.iter().any(|f| f == FEATURE_CONTENTION);
    }

    /// A rejected or failed heartbeat may come from a restarted older coordinator.
    pub(super) fn forget(&mut self) {
        self.contention = false;
    }

    pub(super) fn apply(
        &self,
        report: &mut NodeReport,
        contention: impl FnOnce() -> NodeContentionReport,
    ) {
        if self.contention {
            report.features = COORDINATOR_FEATURES.map(String::from).to_vec();
            report.contention = Some(contention());
        } else {
            report.features.clear();
            report.contention = None;
            for allocation in &mut report.allocations {
                allocation.yield_cause = None;
                allocation.hint_ready = None;
                allocation.observes_drain = None;
            }
        }
    }
}

/// Whether a RAM cap step can apply to work on `backend`: a delegated cgroup leaf
/// with `memory.high` authorized. Rootless backends have no RAM cap.
pub(super) fn ram_cap_available(config: &Config, backend: Option<&str>) -> bool {
    backend == Some("cgroup_v2")
        && config.cgroup.enabled
        && config
            .cgroup
            .authorized_controls
            .iter()
            .any(|control| control == "memory.high")
}

/// Resources whose mode or admission is not yet known are left out, which a
/// negotiated coordinator treats as not open.
pub fn node_contention_report(
    config: &Config,
    decision: &crate::model::Decision,
    max_workers: usize,
) -> NodeContentionReport {
    let resources = decision
        .resources
        .iter()
        .filter_map(|(key, status)| {
            let mut step_summary = BTreeMap::new();
            for action in decision.actions.iter().filter(|a| &a.resource == key) {
                *step_summary.entry(action.step).or_insert(0u32) += 1;
            }
            Some((
                key.clone(),
                ResourceContentionReport {
                    mode: status.effective_mode.or(status.mode)?,
                    admission: status.admission.clone()?,
                    own: status.own,
                    external: status.external,
                    target_share: status.target_share,
                    step_summary,
                    held_back: status.held_back.clone(),
                },
            ))
        })
        .collect();
    NodeContentionReport {
        resources,
        exceptions: decision.exceptions.clone(),
        max_mode: Some(config.contention.max_mode()),
        max_workers: Some(u32::try_from(max_workers).unwrap_or(u32::MAX)),
    }
}
