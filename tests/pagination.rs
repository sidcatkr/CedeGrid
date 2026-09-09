use cedegrid::{
    coordinator::Coordinator,
    execution_model::{AllocationClass, LaunchRequest},
    model::Resources,
    pagination::{Collection, PageQuery},
    protocol::*,
};
use std::{collections::BTreeMap, path::PathBuf};
fn config(root: &std::path::Path) -> CoordinatorConfig {
    CoordinatorConfig {
        storage_profile: Default::default(),
        state_dir: root.join("state"),
        listen: "127.0.0.1:0".parse().unwrap(),
        tls: TlsIdentity {
            ca_cert: PathBuf::new(),
            certificate: PathBuf::new(),
            private_key: PathBuf::new(),
        },
        clients: BTreeMap::new(),
        lease_ms: 10000,
        telemetry_ttl_ms: 3000,
        max_artifact_bytes: 268435456,
        artifact_quota_bytes: 1073741824,
        retry_limit: 3,
        retry_backoff_ms: 1000,
        retry_backoff_max_ms: 30000,
        yield_retry_backoff_ms: 1000,
        yield_retry_backoff_max_ms: 30000,
    }
}
fn task(id: &str) -> LaunchRequest {
    LaunchRequest {
        task_id: id.into(),
        assignment_id: String::new(),
        argv: vec!["/bin/true".into()],
        cwd: "/tmp".into(),
        env: BTreeMap::new(),
        resources: Resources {
            cpu_millicores: 1000,
            ram_mib: 128,
            gpu_memory_mib: BTreeMap::new(),
        },
        replay_safe: true,
        class: AllocationClass::Opportunistic,
        no_escape: true,
        single_process: true,
        managed_child_limit: 0,
        max_attempts: None,
        input_artifacts: vec![],
        required_controls: vec![],
        allow_fallback: true,
    }
}
fn submit(c: &mut Coordinator, job: &str, pool: &str, tasks: usize) {
    c.handle_at(
        &Principal::Operator,
        Request::PutPool {
            pool: PoolSpec {
                pool_id: pool.into(),
                class: AllocationClass::Opportunistic,
                node_ids: vec![format!("{pool}-node")],
                min_workers: 0,
                max_workers: 100,
            },
        },
        1,
    )
    .unwrap();
    c.handle_at(
        &Principal::Operator,
        Request::Submit {
            job: JobSpec {
                job_id: job.into(),
                pool_id: pool.into(),
                priority: 0,
                tasks: (0..tasks).map(|n| task(&format!("{job}-{n}"))).collect(),
            },
        },
        1,
    )
    .unwrap();
}
fn query(collection: Collection) -> PageQuery {
    PageQuery {
        collection,
        job_id: None,
        node_id: None,
        pool_id: None,
        limit: 2,
        cursor: None,
    }
}
fn page(c: &mut Coordinator, q: PageQuery) -> cedegrid::pagination::Page {
    let wire = serde_json::to_vec(&Request::StatusPage { query: q }).unwrap();
    let request = serde_json::from_slice(&wire).unwrap();
    let Response::StatusPage { page } = c.handle_at(&Principal::Operator, request, 1).unwrap()
    else {
        panic!()
    };
    assert!(serde_json::to_vec(&page).unwrap().len() < 7 * 1024 * 1024);
    page
}
#[test]
fn continuous_inserts_do_not_extend_traversal_and_epoch_invalidates_cursor() {
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let cfg = config(dir.path());
    let mut c = Coordinator::open(cfg.clone()).unwrap();
    submit(&mut c, "a", "p", 5);
    let mut q = query(Collection::Tasks);
    let first = page(&mut c, q.clone());
    assert_eq!(first.items.len(), 2);
    q.cursor = first.next_cursor.clone();
    submit(&mut c, "later", "p", 5);
    let second = page(&mut c, q.clone());
    assert_eq!(second.items.len(), 2);
    q.cursor = second.next_cursor;
    let last = page(&mut c, q);
    assert_eq!(last.items.len(), 1);
    assert!(last.next_cursor.is_none());
    drop(c);
    let mut c = Coordinator::open(cfg).unwrap();
    let mut q = query(Collection::Tasks);
    q.cursor = first.next_cursor;
    assert!(
        format!(
            "{:#}",
            c.handle_at(&Principal::Operator, Request::StatusPage { query: q }, 1)
                .unwrap_err()
        )
        .contains("CURSOR_EXPIRED")
    );
}
#[test]
fn filters_exclude_unrelated_jobs_pools_and_membership_arrays() {
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let mut c = Coordinator::open(config(dir.path())).unwrap();
    submit(&mut c, "a", "p", 1);
    submit(&mut c, "b", "q", 1);
    for collection in [Collection::Tasks, Collection::Jobs, Collection::Pools] {
        let mut q = query(collection);
        q.job_id = Some("a".into());
        let p = page(&mut c, q);
        assert_eq!(p.items.len(), 1);
        assert!(p.items[0].get("task_ids").is_none());
        assert!(p.items[0].get("node_ids").is_none());
    }
    let mut q = query(Collection::PoolNodes);
    q.job_id = Some("a".into());
    q.pool_id = Some("p".into());
    assert_eq!(page(&mut c, q.clone()).items[0]["node_id"], "p-node");
    q.pool_id = Some("q".into());
    assert!(
        c.handle_at(&Principal::Operator, Request::StatusPage { query: q }, 1)
            .is_err()
    );
    let mut q = query(Collection::UnrecognizedAllocations);
    q.node_id = Some("p-node".into());
    q.job_id = Some("a".into());
    assert!(
        c.handle_at(&Principal::Operator, Request::StatusPage { query: q }, 1)
            .is_err()
    );
}
#[test]
fn legacy_large_status_refuses_before_full_materialization() {
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let mut c = Coordinator::open(config(dir.path())).unwrap();
    submit(&mut c, "a", "p", 1001);
    let error = c
        .handle_at(&Principal::Operator, Request::Status { job_id: None }, 1)
        .unwrap_err();
    assert!(format!("{error:#}").contains("PAGINATION_REQUIRED"));
    assert_eq!(page(&mut c, query(Collection::Tasks)).items.len(), 2);
}
#[test]
fn report_overflow_and_invalid_controls_fail_without_dropping_inventory() {
    let mut report = NodeReport {
        node_id: "node".into(),
        boot_id: "boot".into(),
        observed_at_unix_ms: 1,
        managed_budget: Resources::default(),
        expansion_allowed: false,
        gpu_expansion_allowed: false,
        gpu_guaranteed_allowed: false,
        launch_slots: 0,
        available_controls: vec![],
        allocations: vec![],
    };
    report.allocations = (0..2049)
        .map(|n| AllocationReport {
            assignment_id: format!("a{n}"),
            generation: 1,
            phase: RemotePhase::Uncertain,
            observed: None,
            detail: String::new(),
        })
        .collect();
    assert!(report.validate_bounds().is_err());
    assert_eq!(report.allocations.len(), 2049);
    report.allocations.truncate(1);
    report.allocations[0].detail = "世".repeat(342);
    assert!(report.validate_bounds().is_err());
    report.allocations[0].detail.clear();
    assert!(report.validate_bounds().is_ok());
}
