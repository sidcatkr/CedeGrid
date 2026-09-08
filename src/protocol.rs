//! Version-one transport contract. Authentication belongs to TLS, never workload argv.
use crate::{
    execution_model::{AllocationClass, LaunchRequest},
    model::Resources,
    state::TaskRecord,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

pub const API_VERSION: u32 = 1;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsIdentity {
    pub ca_cert: PathBuf,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub endpoint: String,
    pub tls: TlsIdentity,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Principal {
    Operator,
    Node { node_id: String },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoordinatorConfig {
    pub state_dir: PathBuf,
    #[serde(default)]
    pub storage_profile: crate::state::StorageProfile,
    pub listen: std::net::SocketAddr,
    pub tls: TlsIdentity,
    /// SHA256 DER client leaf certificate -> authorized role. A CA signature alone grants no role.
    pub clients: BTreeMap<String, Principal>,
    #[serde(default = "lease_default")]
    pub lease_ms: u64,
    #[serde(default = "fresh_default")]
    pub telemetry_ttl_ms: u64,
    #[serde(default = "artifact_default")]
    pub max_artifact_bytes: u64,
    #[serde(default = "disk_default")]
    pub artifact_quota_bytes: u64,
    #[serde(default = "retry_limit_default")]
    pub retry_limit: u32,
    #[serde(default = "retry_backoff_default")]
    pub retry_backoff_ms: u64,
    #[serde(default = "retry_backoff_max_default")]
    pub retry_backoff_max_ms: u64,
    #[serde(default = "retry_backoff_default")]
    pub yield_retry_backoff_ms: u64,
    #[serde(default = "retry_backoff_max_default")]
    pub yield_retry_backoff_max_ms: u64,
}
fn retry_limit_default() -> u32 {
    3
}
fn retry_backoff_default() -> u64 {
    1000
}
fn retry_backoff_max_default() -> u64 {
    30_000
}
fn lease_default() -> u64 {
    10_000
}
fn fresh_default() -> u64 {
    3_000
}
fn artifact_default() -> u64 {
    256 * 1024 * 1024
}
fn disk_default() -> u64 {
    20 * 1024 * 1024 * 1024
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolSpec {
    pub pool_id: String,
    pub class: AllocationClass,
    pub node_ids: Vec<String>,
    pub min_workers: u32,
    pub max_workers: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    pub job_id: String,
    pub pool_id: String,
    #[serde(default)]
    pub priority: i32,
    pub tasks: Vec<LaunchRequest>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllocationReport {
    pub assignment_id: String,
    pub generation: u64,
    pub phase: RemotePhase,
    pub observed: Option<Resources>,
    #[serde(default)]
    pub detail: String,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemotePhase {
    Offered,
    Prepared,
    Authorized,
    Running,
    Draining,
    Uncertain,
    Released,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeReport {
    pub node_id: String,
    pub boot_id: String,
    pub observed_at_unix_ms: u64,
    pub managed_budget: Resources,
    pub expansion_allowed: bool,
    /// Positive retained GPU budgets are accounting caps, not admission evidence.
    #[serde(default)]
    pub gpu_expansion_allowed: bool,
    /// Full-yield compatibility modes cannot honor Guaranteed GPU continuity.
    /// Missing legacy capability is false; CPU Guaranteed work is unaffected.
    #[serde(default)]
    pub gpu_guaranteed_allowed: bool,
    /// Maximum additional unstarted assignments this agent can accept now.
    #[serde(default)]
    pub launch_slots: u32,
    pub available_controls: Vec<String>,
    pub allocations: Vec<AllocationReport>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub node_id: String,
    pub generation: u64,
    pub coordinator_epoch: u64,
    pub request: LaunchRequest,
    #[serde(default)]
    pub checkpoint: Option<ResultSubmission>,
}
/// Strong-coordinator authority for rebuilding non-authoritative local state.
/// Inclusion never proves process absence or permits relaunching an old attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayRecoverySnapshot {
    pub session_id: String,
    pub coordinator_epoch: u64,
    pub allocations: Vec<ReplayRecoveryAllocation>,
    pub unrecognized: Vec<AllocationReport>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayRecoveryAllocation {
    pub assignment: Assignment,
    pub prepared: Option<crate::execution_model::ExecutionRecord>,
    pub previous_boot_id: Option<String>,
    pub lease_sequence: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub assignment_id: String,
    pub generation: u64,
    pub coordinator_epoch: u64,
    pub sequence: u64,
    pub valid_for_ms: u64,
    pub drain: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatReply {
    pub coordinator_epoch: u64,
    pub assignments: Vec<Assignment>,
    pub drain: Vec<String>,
    pub uncertain: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactRef {
    pub sha256: String,
    pub size: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultSubmission {
    pub task_id: String,
    pub assignment_id: String,
    pub generation: u64,
    pub result: serde_json::Value,
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub task_id: String,
    pub assignment_id: String,
    pub generation: u64,
    pub receipt_hash: String,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    #[default]
    ExecutionFailure,
    Yielded,
}

/// All calls POST JSON to /v1/rpc. Chunk payloads are hexadecimal, bounded at 1 MiB.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    OpenReplaySession {
        node_id: String,
        boot_id: String,
        session_id: String,
    },
    /// Mandatory for every request from a registered replayable node, including
    /// artifacts and receipt replay. Nested wrappers are rejected.
    NodeSession {
        session_id: String,
        request: Box<Request>,
    },
    PutPool {
        pool: PoolSpec,
    },
    Submit {
        job: JobSpec,
    },
    Status {
        job_id: Option<String>,
    },
    StatusPage {
        #[serde(flatten)]
        query: crate::pagination::PageQuery,
    },
    GetResult {
        task_id: String,
    },
    Retry {
        task_id: String,
        #[serde(default)]
        confirm_side_effects_reconciled: bool,
    },
    Cancel {
        job_id: String,
    },
    DrainNode {
        node_id: String,
        drain: bool,
    },
    Heartbeat {
        report: NodeReport,
    },
    Prepared {
        assignment_id: String,
        generation: u64,
        coordinator_epoch: u64,
        record: crate::execution_model::ExecutionRecord,
    },
    Renew {
        assignment_id: String,
        generation: u64,
        coordinator_epoch: u64,
        previous_sequence: u64,
    },
    Complete {
        submission: ResultSubmission,
    },
    Fail {
        assignment_id: String,
        generation: u64,
        detail: String,
        #[serde(default)]
        failure_kind: FailureKind,
    },
    PublishCheckpoint {
        submission: ResultSubmission,
    },
    BeginUpload {
        assignment_id: String,
        generation: u64,
        artifact: ArtifactRef,
    },
    UploadChunk {
        upload_id: String,
        offset: u64,
        data_hex: String,
    },
    CommitUpload {
        upload_id: String,
    },
    AbortUpload {
        upload_id: String,
    },
    ReadArtifact {
        sha256: String,
        offset: u64,
        max_bytes: u32,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    StatusPage {
        #[serde(flatten)]
        page: crate::pagination::Page,
    },
    ReplayRecovery {
        snapshot: ReplayRecoverySnapshot,
    },
    Result {
        submission: Option<ResultSubmission>,
    },
    Ok,
    Heartbeat {
        reply: HeartbeatReply,
    },
    Lease {
        lease: Lease,
    },
    Receipt {
        receipt: Receipt,
    },
    Status {
        tasks: Vec<TaskRecord>,
        jobs: Vec<serde_json::Value>,
        pools: Vec<PoolSpec>,
        nodes: Vec<serde_json::Value>,
        allocations: Vec<serde_json::Value>,
    },
    Upload {
        upload_id: String,
        offset: u64,
    },
    Artifact {
        artifact: ArtifactRef,
    },
    Chunk {
        data_hex: String,
        eof: bool,
    },
    Error {
        message: String,
    },
}

/// HTTPS-only client; redirects are forbidden so identities cannot be redirected.
pub struct RpcClient {
    client: reqwest::Client,
    endpoint: String,
    timeout: std::time::Duration,
    rate: u64,
    next_wire: tokio::sync::Mutex<tokio::time::Instant>,
}
impl RpcClient {
    pub fn new(endpoint: &str, tls: &TlsIdentity) -> anyhow::Result<Self> {
        Self::with_settings(endpoint, tls, 15.0, 10 * 1024 * 1024)
    }
    pub fn with_settings(
        endpoint: &str,
        tls: &TlsIdentity,
        timeout_seconds: f64,
        rate: u64,
    ) -> anyhow::Result<Self> {
        let origin = crate::config::normalize_https_origin(endpoint)?;
        anyhow::ensure!(
            timeout_seconds.is_finite()
                && timeout_seconds > 0.0
                && timeout_seconds <= 86400.0
                && rate <= i64::MAX as u64,
            "ERR_CEDEGRID_CONFIG: invalid transport settings"
        );
        let timeout = std::time::Duration::try_from_secs_f64(timeout_seconds)?;
        let ca = reqwest::Certificate::from_pem(&std::fs::read(&tls.ca_cert)?)?;
        let mut identity = std::fs::read(&tls.certificate)?;
        identity.extend_from_slice(&std::fs::read(&tls.private_key)?);
        let identity = reqwest::Identity::from_pem(&identity)?;
        let client = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .tls_built_in_root_certs(false)
            .add_root_certificate(ca)
            .identity(identity)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(timeout)
            .timeout(timeout)
            .build()?;
        Ok(Self {
            client,
            endpoint: format!("{origin}/v1/rpc"),
            timeout,
            rate,
            next_wire: tokio::sync::Mutex::new(tokio::time::Instant::now()),
        })
    }
    async fn pace(&self, bytes: usize) -> anyhow::Result<()> {
        if self.rate == 0 || bytes == 0 {
            return Ok(());
        }
        let charge = (bytes as u64)
            .checked_mul(11)
            .and_then(|n| n.checked_add(9))
            .ok_or_else(|| anyhow::anyhow!("wire charge overflow"))?
            / 10;
        let delay = std::time::Duration::from_secs_f64(charge as f64 / self.rate as f64);
        let until = {
            let mut next = self.next_wire.lock().await;
            let until = (*next)
                .max(tokio::time::Instant::now())
                .checked_add(delay)
                .ok_or_else(|| anyhow::anyhow!("pacing deadline overflow"))?;
            *next = until;
            until
        };
        tokio::time::sleep_until(until).await;
        Ok(())
    }
    pub async fn request(&self, request: &Request) -> anyhow::Result<Response> {
        tokio::time::timeout(self.timeout, async {
            let payload = serde_json::to_vec(request)?;
            anyhow::ensure!(
                payload.len() <= 3 * 1024 * 1024,
                "ERR_CEDEGRID_REQUEST_TOO_LARGE: RPC request exceeds 3 MiB"
            );
            self.pace(payload.len()).await?;
            let mut response = self
                .client
                .post(&self.endpoint)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(payload)
                .send()
                .await?;
            anyhow::ensure!(
                response.status().is_success(),
                "RPC HTTP status {}",
                response.status()
            );
            anyhow::ensure!(
                response
                    .content_length()
                    .is_none_or(|s| s <= 8 * 1024 * 1024),
                "RPC response too large"
            );
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                anyhow::ensure!(
                    body.len()
                        .checked_add(chunk.len())
                        .is_some_and(|len| len <= 8 * 1024 * 1024),
                    "RPC response too large"
                );
                self.pace(chunk.len()).await?;
                body.extend_from_slice(&chunk);
            }
            let response: Response = crate::numeric::from_slice(&body)?;
            if let Response::Error { message } = &response {
                anyhow::bail!("coordinator rejected request: {message}");
            }
            Ok(response)
        })
        .await
        .map_err(|_| anyhow::anyhow!("ERR_CEDEGRID_DEADLINE: RPC end-to-end deadline exceeded"))?
    }
}

pub async fn rpc(endpoint: &str, tls: &TlsIdentity, request: &Request) -> anyhow::Result<Response> {
    RpcClient::new(endpoint, tls)?.request(request).await
}

impl NodeReport {
    pub fn validate_bounds(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.allocations.len() <= 2048,
            "ERR_CEDEGRID_INVENTORY_LIMIT: maximum 2048 allocation entries; reservations remain retained"
        );
        anyhow::ensure!(
            self.available_controls.len() <= 64
                && self.available_controls.iter().all(|c| !c.is_empty()
                    && c.len() <= 128
                    && c.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))),
            "ERR_CEDEGRID_INVENTORY_LIMIT: invalid controls"
        );
        let mut ids = std::collections::BTreeSet::new();
        for a in &self.allocations {
            anyhow::ensure!(
                !a.assignment_id.is_empty()
                    && a.assignment_id.len() <= 256
                    && !a.assignment_id.chars().any(char::is_control)
                    && a.detail.len() <= 1024
                    && a.generation <= i64::MAX as u64
                    && ids.insert(&a.assignment_id),
                "ERR_CEDEGRID_INVENTORY_LIMIT: invalid or duplicate allocation detail"
            );
        }
        anyhow::ensure!(
            serde_json::to_vec(self)?.len() <= 2 * 1024 * 1024,
            "ERR_CEDEGRID_INVENTORY_LIMIT: report exceeds 2 MiB; reservations remain retained"
        );
        Ok(())
    }
}

/// Submission validation is independent of the client's OS or local filesystem.
pub fn validate_launch_request(request: &LaunchRequest) -> anyhow::Result<()> {
    anyhow::ensure!(
        !request.task_id.is_empty()
            && request.task_id.len() <= 256
            && !request.task_id.chars().any(char::is_control),
        "ERR_CEDEGRID_ARGUMENT: invalid task ID"
    );
    anyhow::ensure!(
        !request.argv.is_empty()
            && !request.argv[0].is_empty()
            && request.argv.iter().all(|a| !a.contains('\0')),
        "ERR_CEDEGRID_ARGUMENT: invalid argv"
    );
    let cwd = request
        .cwd
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("ERR_CEDEGRID_ARGUMENT: cwd must be UTF-8"))?;
    anyhow::ensure!(
        cwd.starts_with('/') && !cwd.contains('\0'),
        "ERR_CEDEGRID_ARGUMENT: execution cwd must be an absolute POSIX path"
    );
    anyhow::ensure!(
        request.env.iter().all(|(k, v)| !k.is_empty()
            && !k.contains(['=', '\0'])
            && !v.contains('\0')
            && (!k.starts_with("CEDEGRID_") || k == "CEDEGRID_METADATA")),
        "ERR_CEDEGRID_ARGUMENT: invalid or reserved environment"
    );
    if let Some(metadata) = request.env.get("CEDEGRID_METADATA") {
        let _: serde_json::Value = crate::numeric::from_slice(metadata.as_bytes())?;
    }
    anyhow::ensure!(
        request.resources.cpu_millicores > 0
            && request.resources.ram_mib > 0
            && request.resources.cpu_millicores <= i64::MAX as u64
            && request.resources.ram_mib <= i64::MAX as u64
            && request
                .resources
                .gpu_memory_mib
                .values()
                .all(|n| *n > 0 && *n <= i64::MAX as u64),
        "ERR_CEDEGRID_ARGUMENT: invalid resource amount"
    );
    anyhow::ensure!(
        request.managed_child_limit <= 8
            && (request.managed_child_limit == 0 || (!request.single_process && request.no_escape)),
        "ERR_CEDEGRID_ARGUMENT: invalid managed-child contract"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ResultSubmission;

    #[test]
    fn result_payload_numbers_survive_all_protocol_parse_hops() {
        // These first two finite values exposed one-ULP changes in real SDK
        // result metadata. Also retain signed zero, subnormals and exact u64s.
        let mut wire = br#"{"task_id":"task","assignment_id":"assignment","generation":1,"result":{"observations":[18.400785964971874,31.859559996519238,-0.0,1e-300,5e-324,1.7976931348623157e308],"integer":9007199254740993},"artifacts":[]}"#.to_vec();
        let expected = [
            18.400785964971874_f64,
            31.859559996519238,
            -0.0,
            1e-300,
            5e-324,
            f64::MAX,
        ];
        for hop in 0..4 {
            let value: ResultSubmission = serde_json::from_slice(&wire).unwrap();
            let observations = value.result["observations"].as_array().unwrap();
            for (actual, expected) in observations.iter().zip(expected) {
                assert_eq!(
                    actual.as_f64().unwrap().to_bits(),
                    expected.to_bits(),
                    "parse hop {hop}"
                );
            }
            assert_eq!(
                value.result["integer"].as_u64(),
                Some(9_007_199_254_740_993)
            );
            wire = serde_json::to_vec(&value).unwrap();
        }
    }
}
