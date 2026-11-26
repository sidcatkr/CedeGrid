//! The local agent (003 research R1, contracts/local-agent.md): one agent per user and host that
//! keeps its own coordinator in the process. `run`, `ps`, `logs`, `stop` and `top` reach it over
//! a socket only the user can open, and its runs take the same launch path as cluster work, so
//! they need no network, no certificates and no second service (FR-003).

use crate::coordinator::{Coordinator, wait_for_work};
use crate::execution_model::AllocationClass;
use crate::protocol::{CoordinatorConfig, PoolSpec, Principal, Request, Response};
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

/// The node ID a local agent takes when its configuration names none.
pub const LOCAL_NODE: &str = "local";

/// The pools its runs go to, one per allocation class.
pub fn local_pool(class: AllocationClass) -> &'static str {
    match class {
        AllocationClass::Guaranteed => "local-guaranteed",
        AllocationClass::Opportunistic => "local-opportunistic",
    }
}

/// The coordinator inside a local agent. Its state lives in the agent's state directory, and it
/// answers only this process: the node's requests and those the user's socket carries.
pub struct LocalCoordinator {
    node_id: String,
    coordinator: Mutex<Coordinator>,
    work: tokio::sync::watch::Receiver<u64>,
    waiting: AtomicUsize,
}

impl LocalCoordinator {
    /// Opens the coordinator under `state_dir/local-coordinator` and makes the local pools,
    /// each holding the node `node_id` alone with room for `max_workers` tasks.
    pub fn open(
        state_dir: &Path,
        storage_profile: crate::state::StorageProfile,
        node_id: &str,
        max_workers: u32,
    ) -> Result<Self> {
        // Defaults come from the configuration's own serde defaults. The listen address and the
        // TLS files are never used: nothing is served.
        let config: CoordinatorConfig = serde_json::from_value(serde_json::json!({
            "state_dir": state_dir.join("local-coordinator"),
            "storage_profile": storage_profile,
            "listen": "127.0.0.1:0",
            "tls": {"ca_cert": "", "certificate": "", "private_key": ""},
            "clients": {},
        }))
        .context("the local coordinator's configuration")?;
        super::private_dir(&config.state_dir)?;
        let mut coordinator = Coordinator::open(config)?;
        for class in [AllocationClass::Guaranteed, AllocationClass::Opportunistic] {
            let pool = PoolSpec {
                pool_id: local_pool(class).to_owned(),
                class,
                node_ids: vec![node_id.to_owned()],
                min_workers: 0,
                max_workers,
            };
            if let Response::Error { message, .. } =
                coordinator.handle(&Principal::Operator, Request::PutPool { pool })?
            {
                bail!("the local pool was refused: {message}");
            }
        }
        let work = coordinator.subscribe_work();
        Ok(Self {
            node_id: node_id.to_owned(),
            coordinator: Mutex::new(coordinator),
            work,
            waiting: AtomicUsize::new(0),
        })
    }

    /// One request from this agent's node side. A wait for work is held here, as the server
    /// holds it for a remote node (006 SC-001).
    pub(super) async fn node_request(&self, request: &Request) -> Result<Response> {
        if let Request::AwaitWork { seen, wait_ms } = request {
            wait_for_work(
                self.work.clone(),
                *seen,
                Duration::from_millis(*wait_ms),
                &self.waiting,
            )
            .await;
        }
        self.handle(
            &Principal::Node {
                node_id: self.node_id.clone(),
            },
            request.clone(),
        )
    }

    /// One request on the user's behalf, as an operator would send it.
    pub fn operator_request(&self, request: Request) -> Result<Response> {
        self.handle(&Principal::Operator, request)
    }

    fn handle(&self, principal: &Principal, request: Request) -> Result<Response> {
        self.coordinator
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .handle(principal, request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Response;

    /// The local coordinator answers in process: the pools exist, an operator's status request
    /// is served, and a node request is answered as the node.
    #[test]
    fn the_local_coordinator_answers_without_a_network() {
        let dir = crate::namespace::TestStateDir(
            tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap(),
        );
        let local = LocalCoordinator::open(
            dir.path(),
            crate::state::StorageProfile::default(),
            LOCAL_NODE,
            4,
        )
        .unwrap();
        match local
            .operator_request(Request::Status { job_id: None })
            .unwrap()
        {
            Response::Status { pools, .. } => {
                let ids: Vec<&str> = pools.iter().map(|pool| pool.pool_id.as_str()).collect();
                assert!(ids.contains(&"local-guaranteed") && ids.contains(&"local-opportunistic"));
            }
            other => panic!("{other:?}"),
        }
        // Reopening keeps the same pools.
        drop(local);
        LocalCoordinator::open(
            dir.path(),
            crate::state::StorageProfile::default(),
            LOCAL_NODE,
            4,
        )
        .unwrap();
    }
}
