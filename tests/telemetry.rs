use cedegrid::model::{CapabilityStatus, SCHEMA_VERSION, Snapshot};
use cedegrid::telemetry::Collector;

#[test]
fn empty_node_identity_is_rejected() {
    assert!(Collector::new(" \n").is_err());
}

#[test]
fn observation_is_serializable_and_never_claims_enforcement() {
    let snapshot = Collector::new("portable-test-node")
        .unwrap()
        .sample()
        .unwrap();
    assert_eq!(snapshot.schema_version, SCHEMA_VERSION);
    assert_eq!(snapshot.node_id, "portable-test-node");
    assert!(
        snapshot
            .capabilities
            .values()
            .all(|capability| !capability.enforced)
    );
    assert_eq!(
        snapshot.capabilities["process_supervision"].status,
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            CapabilityStatus::Available
        } else {
            CapabilityStatus::Unsupported
        }
    );
    assert!(
        snapshot
            .cpu_busy_millicores
            .is_none_or(|usage| usage <= snapshot.cpu_capacity_millicores)
    );
    let json = serde_json::to_string(&snapshot).unwrap();
    let decoded: Snapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.node_id, snapshot.node_id);
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
#[test]
fn unsupported_gpu_adapter_does_not_invent_available_capacity() {
    let snapshot = Collector::new("non-nvml-platform")
        .unwrap()
        .sample()
        .unwrap();
    assert_eq!(snapshot.gpu_inventory, CapabilityStatus::Unsupported);
    assert!(snapshot.gpus.is_empty());
    assert_eq!(
        snapshot.capabilities["pidfd"].status,
        CapabilityStatus::Unsupported
    );
}
