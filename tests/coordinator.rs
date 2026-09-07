use resource_manager::{
    coordinator::Coordinator,
    execution_model::{
        AllocationClass, ExecutionPhase, ExecutionRecord, LaunchRequest, ProcessIdentity,
    },
    model::Resources,
    protocol::*,
};
use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf};
fn dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(".coordinator-test-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap()
}
fn config(path: &std::path::Path) -> CoordinatorConfig {
    CoordinatorConfig {
        storage_profile: Default::default(),
        state_dir: path.join("state"),
        listen: "127.0.0.1:0".parse().unwrap(),
        tls: TlsIdentity {
            ca_cert: PathBuf::new(),
            certificate: PathBuf::new(),
            private_key: PathBuf::new(),
        },
        clients: BTreeMap::new(),
        lease_ms: 10_000,
        telemetry_ttl_ms: 3000,
        max_artifact_bytes: 1024 * 1024,
        artifact_quota_bytes: 10 * 1024 * 1024,
        retry_limit: 3,
        retry_backoff_ms: 1000,
        retry_backoff_max_ms: 30000,
        yield_retry_backoff_ms: 1000,
        yield_retry_backoff_max_ms: 30000,
    }
}
fn pool() -> PoolSpec {
    PoolSpec {
        pool_id: "pool".into(),
        class: AllocationClass::Opportunistic,
        node_ids: vec!["node".into()],
        min_workers: 0,
        max_workers: 2,
    }
}
fn task(id: &str) -> LaunchRequest {
    LaunchRequest {
        task_id: id.into(),
        assignment_id: String::new(),
        argv: vec!["/bin/true".into()],
        cwd: std::env::current_dir().unwrap(),
        env: BTreeMap::new(),
        resources: Resources {
            cpu_millicores: 1000,
            ram_mib: 100,
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
fn report(now: u64) -> NodeReport {
    NodeReport {
        node_id: "node".into(),
        boot_id: "boot".into(),
        observed_at_unix_ms: now,
        managed_budget: Resources {
            cpu_millicores: 2000,
            ram_mib: 1000,
            gpu_memory_mib: BTreeMap::new(),
        },
        expansion_allowed: true,
        gpu_expansion_allowed: true,
        gpu_guaranteed_allowed: false,
        launch_slots: 2,
        available_controls: vec![],
        allocations: vec![],
    }
}
fn node() -> Principal {
    Principal::Node {
        node_id: "node".into(),
    }
}
fn setup(c: &mut Coordinator, ids: &[&str], now: u64) {
    c.handle_at(&Principal::Operator, Request::PutPool { pool: pool() }, now)
        .unwrap();
    c.handle_at(
        &Principal::Operator,
        Request::Submit {
            job: JobSpec {
                job_id: "job".into(),
                pool_id: "pool".into(),
                priority: 0,
                tasks: ids.iter().map(|id| task(id)).collect(),
            },
        },
        now,
    )
    .unwrap();
}
fn offers(c: &mut Coordinator, now: u64) -> HeartbeatReply {
    let Response::Heartbeat { reply } = c
        .handle_at(
            &node(),
            Request::Heartbeat {
                report: report(now),
            },
            now,
        )
        .unwrap()
    else {
        panic!()
    };
    reply
}
fn prepared(a: &Assignment) -> ExecutionRecord {
    ExecutionRecord {
        task_id: a.request.task_id.clone(),
        assignment_id: a.request.assignment_id.clone(),
        generation: a.generation,
        class: a.request.class,
        phase: ExecutionPhase::Prepared,
        resources: a.request.resources.clone(),
        identity: Some(ProcessIdentity {
            pid: 123,
            boot_id: "boot".into(),
            start_time: 456,
            assignment_id: a.request.assignment_id.clone(),
            generation: a.generation,
        }),
        backend: "rootless".into(),
        evidence: vec![],
        detail: "verified".into(),
    }
}
fn authorize(c: &mut Coordinator, a: &Assignment, now: u64) -> Lease {
    let Response::Lease { lease } = c
        .handle_at(
            &node(),
            Request::Prepared {
                assignment_id: a.request.assignment_id.clone(),
                generation: a.generation,
                coordinator_epoch: a.coordinator_epoch,
                record: prepared(a),
            },
            now,
        )
        .unwrap()
    else {
        panic!()
    };
    lease
}
#[test]
fn one_coordinator_lock_and_restart_epoch() {
    let d = dir();
    let cfg = config(d.path());
    let c = Coordinator::open(cfg.clone()).unwrap();
    let epoch = c.epoch();
    assert!(Coordinator::open(cfg.clone()).is_err());
    drop(c);
    assert_eq!(Coordinator::open(cfg).unwrap().epoch(), epoch + 1);
}
#[test]
fn reservations_are_atomic_and_priority_fifo() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["low"], 100);
    c.handle_at(
        &Principal::Operator,
        Request::Submit {
            job: JobSpec {
                job_id: "high-job".into(),
                pool_id: "pool".into(),
                priority: 10,
                tasks: vec![task("high-1"), task("high-2")],
            },
        },
        100,
    )
    .unwrap();
    let first = offers(&mut c, 100);
    assert_eq!(
        first
            .assignments
            .iter()
            .map(|a| a.request.task_id.as_str())
            .collect::<Vec<_>>(),
        vec!["high-1", "high-2"]
    );
    let again = offers(&mut c, 101);
    assert_eq!(again.assignments.len(), 2);
    assert_eq!(
        again.assignments[0].request.assignment_id,
        first.assignments[0].request.assignment_id
    );
}
#[test]
fn node_cert_cannot_impersonate_or_operate() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    assert!(
        c.handle_at(&node(), Request::PutPool { pool: pool() }, 0)
            .is_err()
    );
    assert!(
        c.handle_at(
            &Principal::Node {
                node_id: "other".into()
            },
            Request::Heartbeat { report: report(0) },
            0
        )
        .is_err()
    );
}
#[test]
fn lost_lease_ack_does_not_extend_deadline() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["task"], 100);
    let a = offers(&mut c, 100).assignments.remove(0);
    let first = authorize(&mut c, &a, 100);
    assert_eq!(first.sequence, 1);
    assert_eq!(authorize(&mut c, &a, 200).valid_for_ms, 9900);
    let req = Request::Renew {
        assignment_id: a.request.assignment_id.clone(),
        generation: a.generation,
        coordinator_epoch: a.coordinator_epoch,
        previous_sequence: 1,
    };
    let Response::Lease { lease } = c.handle_at(&node(), req.clone(), 1000).unwrap() else {
        panic!()
    };
    assert_eq!(lease.sequence, 2);
    let Response::Lease { lease } = c.handle_at(&node(), req, 2000).unwrap() else {
        panic!()
    };
    assert_eq!(lease.sequence, 2);
    assert_eq!(lease.valid_for_ms, 9000);
}
#[test]
fn no_user_execution_authority_without_prepared_identity() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["task"], 100);
    let a = offers(&mut c, 100).assignments.remove(0);
    let mut record = prepared(&a);
    record.identity = None;
    assert!(
        c.handle_at(
            &node(),
            Request::Prepared {
                assignment_id: a.request.assignment_id,
                generation: a.generation,
                coordinator_epoch: a.coordinator_epoch,
                record
            },
            100
        )
        .is_err()
    );
}
#[test]
fn uncertain_allocations_stay_reserved_until_release() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["task", "next"], 100);
    let mut r = report(100);
    r.managed_budget.cpu_millicores = 1000;
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report: r.clone() }, 100)
        .unwrap()
    else {
        panic!()
    };
    let a = &reply.assignments[0];
    authorize(&mut c, a, 100);
    r.observed_at_unix_ms = 20_000;
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report: r.clone() }, 20_000)
        .unwrap()
    else {
        panic!()
    };
    assert!(reply.assignments.is_empty());
    assert_eq!(reply.uncertain.len(), 1);
    r.allocations.push(AllocationReport {
        assignment_id: a.request.assignment_id.clone(),
        generation: a.generation,
        phase: RemotePhase::Released,
        observed: None,
        detail: "verified pidfd exit and resource release".into(),
    });
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report: r.clone() }, 20_001)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(reply.assignments.len(), 1);
    assert_eq!(reply.assignments[0].request.task_id, "next");
    assert_eq!(reply.assignments[0].generation, 1);
    r.observed_at_unix_ms = 21_001;
    r.managed_budget.cpu_millicores = 2000;
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report: r }, 21_001)
        .unwrap()
    else {
        panic!()
    };
    assert!(
        reply
            .assignments
            .iter()
            .any(|a| a.request.task_id == "task" && a.generation == 2)
    );
}
#[test]
fn required_controls_and_stale_telemetry_block_admission() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    c.handle_at(&Principal::Operator, Request::PutPool { pool: pool() }, 100)
        .unwrap();
    let mut t = task("task");
    t.required_controls.push("cpu.max".into());
    c.handle_at(
        &Principal::Operator,
        Request::Submit {
            job: JobSpec {
                job_id: "job".into(),
                pool_id: "pool".into(),
                priority: 0,
                tasks: vec![t],
            },
        },
        100,
    )
    .unwrap();
    assert!(offers(&mut c, 100).assignments.is_empty());
    let mut r = report(100);
    r.available_controls.push("cpu.max".into());
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report: r }, 5000)
        .unwrap()
    else {
        panic!()
    };
    assert!(reply.assignments.is_empty());
}
#[test]
fn final_receipt_replay_is_idempotent_and_payload_queryable() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["task"], 100);
    let a = offers(&mut c, 100).assignments.remove(0);
    authorize(&mut c, &a, 100);
    let submission = ResultSubmission {
        task_id: "task".into(),
        assignment_id: a.request.assignment_id,
        generation: a.generation,
        result: json!({"answer":42}),
        artifacts: vec![],
    };
    let first = c
        .handle_at(
            &node(),
            Request::Complete {
                submission: submission.clone(),
            },
            100,
        )
        .unwrap();
    let second = c
        .handle_at(
            &node(),
            Request::Complete {
                submission: submission.clone(),
            },
            200,
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(second).unwrap()
    );
    let mut changed = submission;
    changed.result = json!({"answer":43});
    assert!(
        c.handle_at(
            &node(),
            Request::Complete {
                submission: changed
            },
            200
        )
        .is_err()
    );
    let Response::Result { submission } = c
        .handle_at(
            &Principal::Operator,
            Request::GetResult {
                task_id: "task".into(),
            },
            200,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(submission.unwrap().result["answer"], 42);
}

// These decimal literals are legacy serialized values from the pre-roundtrip
// parser. Seed their bytes directly: constructing this fixture with json! or
// parsing and reserializing it would test new data rather than stored old data.
fn assert_legacy_float_receipt_recovery(checkpoint: bool) {
    use sha2::{Digest, Sha256};

    let d = dir();
    let cfg = config(d.path());
    let mut c = Coordinator::open(cfg.clone()).unwrap();
    setup(&mut c, &["legacy-float"], 100);
    let a = offers(&mut c, 100).assignments.remove(0);
    authorize(&mut c, &a, 100);
    drop(c);

    let sequence = if checkpoint {
        r#""checkpoint_sequence":1,"#
    } else {
        ""
    };
    let legacy = format!(
        r#"{{"task_id":"legacy-float","assignment_id":"{}","generation":{},"result":{{{}"first":18.40078596497187,"second":31.85955999651924}},"artifacts":[]}}"#,
        a.request.assignment_id, a.generation, sequence
    );
    let legacy_hash = hex::encode(Sha256::digest(legacy.as_bytes()));
    let database = cfg.state_dir.join("state.sqlite3");
    {
        let mut db = rusqlite::Connection::open(&database).unwrap();
        db.pragma_update(None, "foreign_keys", "ON").unwrap();
        let tx = db.transaction().unwrap();
        if checkpoint {
            tx.execute(
                "INSERT INTO checkpoints(task_id,submission_json,receipt_hash) VALUES (?1,?2,?3)",
                rusqlite::params!["legacy-float", legacy, legacy_hash],
            )
            .unwrap();
        } else {
            tx.execute(
                "INSERT INTO result_payloads(task_id,submission_json) VALUES (?1,?2)",
                rusqlite::params!["legacy-float", legacy],
            )
            .unwrap();
            tx.execute(
                "UPDATE tasks SET status='completed',receipt_hash=?2 WHERE task_id=?1",
                rusqlite::params!["legacy-float", legacy_hash],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }

    let mut c = Coordinator::open(cfg).unwrap();
    let db = rusqlite::Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let stored = || -> (String, String) {
        db.query_row(
            if checkpoint {
                "SELECT submission_json,receipt_hash FROM checkpoints WHERE task_id='legacy-float'"
            } else {
                "SELECT submission_json,receipt_hash FROM result_payloads JOIN tasks USING(task_id) WHERE task_id='legacy-float'"
            },
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    };
    let expected = (legacy.clone(), legacy_hash.clone());
    assert_eq!(
        stored(),
        expected,
        "opening old state must not migrate accepted bytes or hashes"
    );
    let request = |submission| {
        if checkpoint {
            Request::PublishCheckpoint { submission }
        } else {
            Request::Complete { submission }
        }
    };
    let canonical: ResultSubmission = serde_json::from_str(&legacy).unwrap();
    assert_eq!(serde_json::to_string(&canonical).unwrap(), legacy);
    let Response::Receipt { receipt } = c
        .handle_at(&node(), request(canonical.clone()), 101)
        .unwrap()
    else {
        panic!("legacy canonical replay did not return its receipt")
    };
    assert_eq!(receipt.receipt_hash, legacy_hash);
    assert_eq!(
        stored(),
        expected,
        "canonical replay changed legacy storage"
    );

    if !checkpoint {
        let Response::Result { submission } = c
            .handle_at(
                &Principal::Operator,
                Request::GetResult {
                    task_id: "legacy-float".into(),
                },
                102,
            )
            .unwrap()
        else {
            panic!("legacy result disappeared after reopening")
        };
        assert_eq!(serde_json::to_string(&submission.unwrap()).unwrap(), legacy);
        assert_eq!(stored(), expected, "reading a legacy result rewrote it");
    }

    // The original worker/wire values are different from the legacy rounded
    // payload. An upgrade must expose that conflict, not replace accepted data
    // or silently accept it through a weaker/alternate digest comparison.
    let original = legacy
        .replace("18.40078596497187", "18.400785964971874")
        .replace("31.85955999651924", "31.859559996519238");
    let original: ResultSubmission = serde_json::from_str(&original).unwrap();
    assert_ne!(serde_json::to_string(&original).unwrap(), legacy);
    let mut conflicting = canonical;
    conflicting.result["first"] = json!(999.0);
    for submission in [original, conflicting] {
        let error = c.handle_at(&node(), request(submission), 103).unwrap_err();
        assert!(error.to_string().contains(if checkpoint {
            "checkpoint replay would rewind or replace accepted progress"
        } else {
            "different result already accepted"
        }));
        assert_eq!(
            stored(),
            expected,
            "conflicting replay changed legacy storage"
        );
    }
}

#[test]
fn legacy_float_result_receipt_survives_reopen_without_payload_migration() {
    assert_legacy_float_receipt_recovery(false);
}

#[test]
fn legacy_float_checkpoint_receipt_survives_reopen_without_payload_migration() {
    assert_legacy_float_receipt_recovery(true);
}

#[test]
fn cancel_and_pool_shrink_issue_drain() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["one", "two"], 100);
    let a = offers(&mut c, 100);
    assert_eq!(a.assignments.len(), 2);
    let mut p = pool();
    p.max_workers = 1;
    c.handle_at(&Principal::Operator, Request::PutPool { pool: p }, 100)
        .unwrap();
    assert_eq!(offers(&mut c, 101).drain.len(), 1);
    c.handle_at(
        &Principal::Operator,
        Request::Cancel {
            job_id: "job".into(),
        },
        102,
    )
    .unwrap();
    assert_eq!(offers(&mut c, 103).drain.len(), 2);
    let Response::Status { jobs, tasks, .. } = c
        .handle_at(&Principal::Operator, Request::Status { job_id: None }, 104)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["job_id"], "job");
    assert_eq!(jobs[0]["cancelled"], true);
    assert_eq!(jobs[0]["task_ids"], json!(["one", "two"]));
    assert_eq!(tasks.len(), 2); // Cancellation retains historical task/attempt state.
}
#[test]
fn checkpoints_do_not_rewind_and_follow_retries() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["task"], 100);
    let a = offers(&mut c, 100).assignments.remove(0);
    authorize(&mut c, &a, 100);
    let submission = |sequence| ResultSubmission {
        task_id: "task".into(),
        assignment_id: a.request.assignment_id.clone(),
        generation: a.generation,
        result: json!({"checkpoint_sequence":sequence,"position":sequence}),
        artifacts: vec![],
    };
    c.handle_at(
        &node(),
        Request::PublishCheckpoint {
            submission: submission(1),
        },
        101,
    )
    .unwrap();
    c.handle_at(
        &node(),
        Request::PublishCheckpoint {
            submission: submission(2),
        },
        102,
    )
    .unwrap();
    assert!(
        c.handle_at(
            &node(),
            Request::PublishCheckpoint {
                submission: submission(1)
            },
            103
        )
        .is_err()
    );
    c.handle_at(
        &node(),
        Request::PublishCheckpoint {
            submission: submission(2),
        },
        104,
    )
    .unwrap();
    let mut r = report(105);
    r.allocations.push(AllocationReport {
        assignment_id: a.request.assignment_id.clone(),
        generation: a.generation,
        phase: RemotePhase::Released,
        observed: None,
        detail: "released after checkpoint drain".into(),
    });
    c.handle_at(&node(), Request::Heartbeat { report: r }, 105)
        .unwrap();
    c.handle_at(
        &node(),
        Request::Fail {
            assignment_id: a.request.assignment_id,
            generation: a.generation,
            detail: "retry from checkpoint".into(),
            failure_kind: FailureKind::Yielded,
        },
        106,
    )
    .unwrap();
    assert!(offers(&mut c, 107).assignments.is_empty());
    let b = offers(&mut c, 1106).assignments.remove(0);
    assert_eq!(b.generation, 2);
    assert_eq!(b.checkpoint.unwrap().result["position"], 2);
}
#[test]
fn uploaded_blob_requires_publication_commit_before_result() {
    use sha2::{Digest, Sha256};
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["task"], 100);
    let a = offers(&mut c, 100).assignments.remove(0);
    authorize(&mut c, &a, 100);
    let bytes = b"real result";
    let artifact = ArtifactRef {
        sha256: hex::encode(Sha256::digest(bytes)),
        size: bytes.len() as u64,
    };
    let submission = ResultSubmission {
        task_id: "task".into(),
        assignment_id: a.request.assignment_id.clone(),
        generation: a.generation,
        result: json!({"output":"real result"}),
        artifacts: vec![artifact.clone()],
    };
    let Response::Upload { upload_id, .. } = c
        .handle_at(
            &node(),
            Request::BeginUpload {
                assignment_id: a.request.assignment_id.clone(),
                generation: a.generation,
                artifact,
            },
            101,
        )
        .unwrap()
    else {
        panic!()
    };
    c.handle_at(
        &node(),
        Request::UploadChunk {
            upload_id: upload_id.clone(),
            offset: 0,
            data_hex: hex::encode(bytes),
        },
        102,
    )
    .unwrap();
    assert!(
        c.handle_at(
            &node(),
            Request::Complete {
                submission: submission.clone()
            },
            103
        )
        .is_err()
    );
    c.handle_at(
        &node(),
        Request::CommitUpload {
            upload_id: upload_id.clone(),
        },
        104,
    )
    .unwrap();
    c.handle_at(
        &node(),
        Request::Complete {
            submission: submission.clone(),
        },
        105,
    )
    .unwrap();
    c.handle_at(&node(), Request::AbortUpload { upload_id }, 106)
        .unwrap();
    c.handle_at(&node(), Request::Complete { submission }, 107)
        .unwrap();
}
#[test]
fn coordinator_restart_retains_uncertainty_and_fences_old_authority() {
    let d = dir();
    let cfg = config(d.path());
    let mut c = Coordinator::open(cfg.clone()).unwrap();
    setup(&mut c, &["task"], 100);
    let a = offers(&mut c, 100).assignments.remove(0);
    authorize(&mut c, &a, 100);
    drop(c);
    let mut c = Coordinator::open(cfg).unwrap();
    assert!(
        c.handle_at(
            &node(),
            Request::Renew {
                assignment_id: a.request.assignment_id,
                generation: a.generation,
                coordinator_epoch: a.coordinator_epoch,
                previous_sequence: 1
            },
            101
        )
        .is_err()
    );
    let reply = offers(&mut c, 102);
    assert!(reply.assignments.is_empty());
    assert_eq!(reply.uncertain.len(), 1);
}
#[test]
fn same_user_other_node_cannot_signal_or_complete_ownership() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["task"], 100);
    let a = offers(&mut c, 100).assignments.remove(0);
    let wrong = Principal::Node {
        node_id: "other-node-same-user".into(),
    };
    assert!(
        c.handle_at(
            &wrong,
            Request::Prepared {
                assignment_id: a.request.assignment_id.clone(),
                generation: a.generation,
                coordinator_epoch: a.coordinator_epoch,
                record: prepared(&a)
            },
            101
        )
        .is_err()
    );
    assert!(
        c.handle_at(
            &wrong,
            Request::Complete {
                submission: ResultSubmission {
                    task_id: "task".into(),
                    assignment_id: a.request.assignment_id,
                    generation: a.generation,
                    result: json!({}),
                    artifacts: vec![]
                }
            },
            102
        )
        .is_err()
    );
}
#[test]
fn higher_priority_preemption_waits_for_verified_release() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["low"], 100);
    let mut r = report(100);
    r.managed_budget.cpu_millicores = 1000;
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report: r.clone() }, 100)
        .unwrap()
    else {
        panic!()
    };
    let a = reply.assignments[0].clone();
    authorize(&mut c, &a, 100);
    c.handle_at(
        &Principal::Operator,
        Request::Submit {
            job: JobSpec {
                job_id: "urgent".into(),
                pool_id: "pool".into(),
                priority: 100,
                tasks: vec![task("high")],
            },
        },
        101,
    )
    .unwrap();
    r.observed_at_unix_ms = 102;
    r.allocations.push(AllocationReport {
        assignment_id: a.request.assignment_id.clone(),
        generation: a.generation,
        phase: RemotePhase::Running,
        observed: Some(a.request.resources.clone()),
        detail: "running before preemption".into(),
    });
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report: r.clone() }, 102)
        .unwrap()
    else {
        panic!()
    };
    assert!(reply.assignments.is_empty());
    assert_eq!(reply.drain, vec![a.request.assignment_id.clone()]);
    r.allocations[0] = AllocationReport {
        assignment_id: a.request.assignment_id,
        generation: a.generation,
        phase: RemotePhase::Released,
        observed: None,
        detail: "verified release".into(),
    };
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report: r }, 103)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(reply.assignments[0].request.task_id, "high");
}
#[test]
fn clock_rollback_cannot_extend_a_durable_lease() {
    let d = dir();
    let cfg = config(d.path());
    let mut c = Coordinator::open(cfg.clone()).unwrap();
    setup(&mut c, &["task"], 100);
    let a = offers(&mut c, 100).assignments.remove(0);
    authorize(&mut c, &a, 100);
    assert!(
        c.handle_at(
            &node(),
            Request::Renew {
                assignment_id: a.request.assignment_id.clone(),
                generation: a.generation,
                coordinator_epoch: a.coordinator_epoch,
                previous_sequence: 1
            },
            99
        )
        .is_err()
    );
    drop(c);
    let mut c = Coordinator::open(cfg).unwrap();
    assert!(
        c.handle_at(&Principal::Operator, Request::Status { job_id: None }, 99)
            .is_err()
    );
}
#[test]
fn unknown_managed_allocation_blocks_expansion() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["task"], 100);
    let mut report = report(100);
    report.allocations.push(AllocationReport {
        assignment_id: "unknown-local-allocation".into(),
        generation: 1,
        phase: RemotePhase::Running,
        observed: Some(Resources::default()),
        detail: "requires local reconciliation".into(),
    });
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report }, 100)
        .unwrap()
    else {
        panic!()
    };
    assert!(reply.assignments.is_empty());
    assert_eq!(reply.uncertain, vec!["unknown-local-allocation"]);
}
#[test]
fn worker_slot_ceiling_counts_existing_unstarted_offers() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["one", "two"], 100);
    let mut r = report(100);
    r.launch_slots = 1;
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report: r.clone() }, 100)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(reply.assignments.len(), 1);
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report: r }, 101)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(reply.assignments.len(), 1);
}
#[test]
fn lost_offer_delivery_can_reconcile_cancellation_without_execution() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["task"], 100);
    let lost = offers(&mut c, 100);
    let id = lost.assignments[0].request.assignment_id.clone();
    c.handle_at(
        &Principal::Operator,
        Request::Cancel {
            job_id: "job".into(),
        },
        101,
    )
    .unwrap();
    let replay = offers(&mut c, 102);
    assert_eq!(replay.drain, vec![id.clone()]);
    assert_eq!(replay.assignments[0].request.assignment_id, id);
    let a = &replay.assignments[0];
    let Response::Lease { lease } = c
        .handle_at(
            &node(),
            Request::Prepared {
                assignment_id: id,
                generation: a.generation,
                coordinator_epoch: a.coordinator_epoch,
                record: prepared(a),
            },
            103,
        )
        .unwrap()
    else {
        panic!()
    };
    assert!(lease.drain);
    assert_eq!(lease.sequence, 0);
}
fn released_failure(c: &mut Coordinator, a: &Assignment, now: u64, kind: FailureKind) {
    let mut r = report(now);
    r.launch_slots = 0;
    r.allocations.push(AllocationReport {
        assignment_id: a.request.assignment_id.clone(),
        generation: a.generation,
        phase: RemotePhase::Released,
        observed: None,
        detail: "verified release".into(),
    });
    c.handle_at(&node(), Request::Heartbeat { report: r }, now)
        .unwrap();
    c.handle_at(
        &node(),
        Request::Fail {
            assignment_id: a.request.assignment_id.clone(),
            generation: a.generation,
            detail: "owned workload ended".into(),
            failure_kind: kind,
        },
        now,
    )
    .unwrap();
}
#[test]
fn failures_back_off_and_stop_after_configured_budget_without_ack_double_count() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["bad"], 100);
    let mut now = 100;
    for index in 1..=3 {
        let a = offers(&mut c, now).assignments.remove(0);
        assert_eq!(a.generation, index);
        authorize(&mut c, &a, now);
        now += 1;
        released_failure(&mut c, &a, now, FailureKind::ExecutionFailure);
        c.handle_at(
            &node(),
            Request::Fail {
                assignment_id: a.request.assignment_id,
                generation: a.generation,
                detail: "lost ack retry".into(),
                failure_kind: FailureKind::ExecutionFailure,
            },
            now,
        )
        .unwrap();
        if index < 3 {
            let delay = 1000 * (1u64 << (index - 1));
            assert!(offers(&mut c, now + delay - 1).assignments.is_empty());
            now += delay;
        }
    }
    let Response::Status { tasks, .. } = c
        .handle_at(&Principal::Operator, Request::Status { job_id: None }, now)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        tasks[0].status,
        resource_manager::state::TaskStatus::NeedsReconciliation
    );
    assert!(offers(&mut c, now + 60_000).assignments.is_empty());
    c.handle_at(
        &Principal::Operator,
        Request::Retry {
            task_id: "bad".into(),
            confirm_side_effects_reconciled: false,
        },
        now + 60_001,
    )
    .unwrap();
    assert_eq!(offers(&mut c, now + 60_002).assignments[0].generation, 4);
}
#[test]
fn ordinary_yields_do_not_consume_failure_budget() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["task"], 100);
    for n in 0..6 {
        let now = 100 + n * 60_000;
        let a = offers(&mut c, now).assignments.remove(0);
        authorize(&mut c, &a, now);
        released_failure(&mut c, &a, now + 1, FailureKind::Yielded);
    }
    assert_eq!(offers(&mut c, 360_200).assignments[0].generation, 7);
}
#[test]
fn a_delayed_prepared_message_cannot_replace_verified_process_identity() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["task"], 100);
    let a = offers(&mut c, 100).assignments.remove(0);
    authorize(&mut c, &a, 100);
    let mut record = prepared(&a);
    record.identity.as_mut().unwrap().start_time += 1;
    assert!(
        c.handle_at(
            &node(),
            Request::Prepared {
                assignment_id: a.request.assignment_id,
                generation: a.generation,
                coordinator_epoch: a.coordinator_epoch,
                record
            },
            101
        )
        .is_err()
    );
}
#[test]
fn draining_and_uncertain_reports_cannot_be_reactivated_by_later_running_reports() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["task"], 100);
    let a = offers(&mut c, 100).assignments.remove(0);
    let lease = authorize(&mut c, &a, 100);
    let mut r = report(101);
    r.launch_slots = 0;
    r.allocations.push(AllocationReport {
        assignment_id: a.request.assignment_id.clone(),
        generation: a.generation,
        phase: RemotePhase::Draining,
        observed: Some(a.request.resources.clone()),
        detail: "local policy drain".into(),
    });
    c.handle_at(&node(), Request::Heartbeat { report: r.clone() }, 101)
        .unwrap();
    r.observed_at_unix_ms = 102;
    r.allocations[0].phase = RemotePhase::Running;
    c.handle_at(&node(), Request::Heartbeat { report: r.clone() }, 102)
        .unwrap();
    let Response::Lease { lease } = c
        .handle_at(
            &node(),
            Request::Renew {
                assignment_id: a.request.assignment_id.clone(),
                generation: a.generation,
                coordinator_epoch: a.coordinator_epoch,
                previous_sequence: lease.sequence,
            },
            103,
        )
        .unwrap()
    else {
        panic!()
    };
    assert!(lease.drain);
    r.observed_at_unix_ms = 104;
    r.allocations[0].phase = RemotePhase::Uncertain;
    c.handle_at(&node(), Request::Heartbeat { report: r.clone() }, 104)
        .unwrap();
    r.observed_at_unix_ms = 105;
    r.allocations[0].phase = RemotePhase::Running;
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report: r }, 105)
        .unwrap()
    else {
        panic!()
    };
    assert!(reply.uncertain.contains(&a.request.assignment_id));
}
#[test]
fn delayed_node_telemetry_is_rejected_and_new_live_release_contradiction_reclaims_reservation() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["task"], 100);
    let a = offers(&mut c, 100).assignments.remove(0);
    authorize(&mut c, &a, 100);
    let mut r = report(110);
    r.launch_slots = 0;
    r.allocations.push(AllocationReport {
        assignment_id: a.request.assignment_id.clone(),
        generation: a.generation,
        phase: RemotePhase::Released,
        observed: None,
        detail: "verified exit".into(),
    });
    c.handle_at(&node(), Request::Heartbeat { report: r.clone() }, 110)
        .unwrap();
    r.observed_at_unix_ms = 105;
    assert!(
        c.handle_at(&node(), Request::Heartbeat { report: r.clone() }, 111)
            .is_err()
    );
    r.observed_at_unix_ms = 112;
    r.allocations[0].phase = RemotePhase::Running;
    r.allocations[0].observed = Some(a.request.resources);
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report: r }, 112)
        .unwrap()
    else {
        panic!()
    };
    assert!(reply.uncertain.contains(&a.request.assignment_id));
    assert!(reply.assignments.is_empty());
}

#[test]
fn named_inputs_require_publication_and_only_authorize_current_assignees() {
    use resource_manager::execution_model::NamedArtifact;
    use sha2::{Digest, Sha256};
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["producer"], 100);
    let a = offers(&mut c, 100).assignments.remove(0);
    authorize(&mut c, &a, 100);
    let data = b"immutable model";
    let hash = hex::encode(Sha256::digest(data));
    let input = NamedArtifact {
        name: "model.bin".into(),
        sha256: hash.clone(),
        size: data.len() as u64,
    };
    let mut r = task("consumer");
    r.input_artifacts = vec![input.clone()];
    c.handle_at(
        &Principal::Operator,
        Request::PutPool {
            pool: PoolSpec {
                pool_id: "consumer-pool".into(),
                node_ids: vec!["consumer-node".into()],
                ..pool()
            },
        },
        101,
    )
    .unwrap();
    let job = JobSpec {
        job_id: "consumer-job".into(),
        pool_id: "consumer-pool".into(),
        priority: 0,
        tasks: vec![r],
    };
    assert!(
        c.handle_at(
            &Principal::Operator,
            Request::Submit { job: job.clone() },
            102
        )
        .is_err()
    );
    let Response::Upload { upload_id, .. } = c
        .handle_at(
            &node(),
            Request::BeginUpload {
                assignment_id: a.request.assignment_id,
                generation: a.generation,
                artifact: ArtifactRef {
                    sha256: hash.clone(),
                    size: input.size,
                },
            },
            103,
        )
        .unwrap()
    else {
        panic!()
    };
    c.handle_at(
        &node(),
        Request::UploadChunk {
            upload_id: upload_id.clone(),
            offset: 0,
            data_hex: hex::encode(data),
        },
        104,
    )
    .unwrap();
    c.handle_at(&node(), Request::CommitUpload { upload_id }, 105)
        .unwrap();
    let mut bad = job.clone();
    bad.tasks[0].input_artifacts[0].name = "../model".into();
    assert!(
        c.handle_at(&Principal::Operator, Request::Submit { job: bad }, 106)
            .is_err()
    );
    c.handle_at(
        &Principal::Operator,
        Request::Submit { job: job.clone() },
        107,
    )
    .unwrap();
    c.handle_at(&Principal::Operator, Request::Submit { job }, 108)
        .unwrap();
    let principal = Principal::Node {
        node_id: "consumer-node".into(),
    };
    let read = || Request::ReadArtifact {
        sha256: hash.clone(),
        offset: 0,
        max_bytes: 100,
    };
    assert!(c.handle_at(&principal, read(), 109).is_err());
    let mut nr = report(110);
    nr.node_id = "consumer-node".into();
    let Response::Heartbeat { reply } = c
        .handle_at(&principal, Request::Heartbeat { report: nr.clone() }, 110)
        .unwrap()
    else {
        panic!()
    };
    let assigned = reply.assignments[0].clone();
    assert_eq!(assigned.request.input_artifacts, vec![input]);
    assert!(matches!(
        c.handle_at(&principal, read(), 111).unwrap(),
        Response::Chunk { eof: true, .. }
    ));
    nr.observed_at_unix_ms = 112;
    nr.expansion_allowed = false;
    nr.allocations = vec![AllocationReport {
        assignment_id: assigned.request.assignment_id,
        generation: assigned.generation,
        phase: RemotePhase::Released,
        observed: None,
        detail: "never launched; release verified".into(),
    }];
    c.handle_at(&principal, Request::Heartbeat { report: nr }, 112)
        .unwrap();
    assert!(c.handle_at(&principal, read(), 113).is_err());
}

#[test]
fn missing_live_allocation_blocks_growth_and_cannot_be_revived_by_running_report() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["live"], 100);
    let a = offers(&mut c, 100).assignments.remove(0);
    let lease = authorize(&mut c, &a, 100);
    c.handle_at(
        &Principal::Operator,
        Request::Submit {
            job: JobSpec {
                job_id: "next".into(),
                pool_id: "pool".into(),
                priority: 0,
                tasks: vec![task("new-task")],
            },
        },
        101,
    )
    .unwrap();
    let reply = offers(&mut c, 102);
    assert!(reply.assignments.is_empty());
    assert!(reply.uncertain.contains(&a.request.assignment_id));
    let mut r = report(103);
    r.allocations.push(AllocationReport {
        assignment_id: a.request.assignment_id.clone(),
        generation: a.generation,
        phase: RemotePhase::Running,
        observed: Some(a.request.resources.clone()),
        detail: "late running report".into(),
    });
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report: r.clone() }, 103)
        .unwrap()
    else {
        panic!()
    };
    assert!(reply.uncertain.contains(&a.request.assignment_id));
    assert!(
        c.handle_at(
            &node(),
            Request::Renew {
                assignment_id: a.request.assignment_id.clone(),
                generation: a.generation,
                coordinator_epoch: a.coordinator_epoch,
                previous_sequence: lease.sequence
            },
            104
        )
        .is_err()
    );
    r.observed_at_unix_ms = 105;
    r.allocations[0].phase = RemotePhase::Released;
    r.allocations[0].observed = None;
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report: r }, 105)
        .unwrap()
    else {
        panic!()
    };
    assert!(
        reply
            .assignments
            .iter()
            .any(|a| a.request.task_id == "new-task")
    );
}
#[test]
fn task_uncertain_on_one_node_cannot_preempt_another_nodes_healthy_work() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    c.handle_at(
        &Principal::Operator,
        Request::PutPool {
            pool: PoolSpec {
                node_ids: vec!["node".into(), "other".into()],
                ..pool()
            },
        },
        100,
    )
    .unwrap();
    c.handle_at(
        &Principal::Operator,
        Request::Submit {
            job: JobSpec {
                job_id: "urgent".into(),
                pool_id: "pool".into(),
                priority: 100,
                tasks: vec![task("high")],
            },
        },
        100,
    )
    .unwrap();
    let a = offers(&mut c, 100).assignments.remove(0);
    authorize(&mut c, &a, 100);
    c.handle_at(
        &Principal::Operator,
        Request::Submit {
            job: JobSpec {
                job_id: "ordinary".into(),
                pool_id: "pool".into(),
                priority: 0,
                tasks: vec![task("low")],
            },
        },
        9000,
    )
    .unwrap();
    let other = Principal::Node {
        node_id: "other".into(),
    };
    let mut r = report(9000);
    r.node_id = "other".into();
    r.managed_budget.cpu_millicores = 1000;
    let Response::Heartbeat { reply } = c
        .handle_at(&other, Request::Heartbeat { report: r.clone() }, 9000)
        .unwrap()
    else {
        panic!()
    };
    let b = reply.assignments[0].clone();
    c.handle_at(
        &other,
        Request::Prepared {
            assignment_id: b.request.assignment_id.clone(),
            generation: b.generation,
            coordinator_epoch: b.coordinator_epoch,
            record: prepared(&b),
        },
        9000,
    )
    .unwrap();
    r.observed_at_unix_ms = 10200;
    r.allocations.push(AllocationReport {
        assignment_id: b.request.assignment_id.clone(),
        generation: b.generation,
        phase: RemotePhase::Running,
        observed: Some(b.request.resources.clone()),
        detail: "healthy".into(),
    });
    let Response::Heartbeat { reply } = c
        .handle_at(&other, Request::Heartbeat { report: r }, 10200)
        .unwrap()
    else {
        panic!()
    };
    assert!(reply.drain.is_empty());
    assert!(reply.assignments.is_empty());
}
#[test]
fn prepared_controls_and_initial_freshness_are_checked_before_grant() {
    use resource_manager::execution_model::ControlEvidence;
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    c.handle_at(&Principal::Operator, Request::PutPool { pool: pool() }, 100)
        .unwrap();
    let mut t = task("task");
    t.required_controls = vec!["cpu.nice".into()];
    c.handle_at(
        &Principal::Operator,
        Request::Submit {
            job: JobSpec {
                job_id: "job".into(),
                pool_id: "pool".into(),
                priority: 0,
                tasks: vec![t],
            },
        },
        100,
    )
    .unwrap();
    let mut r = report(100);
    r.available_controls = vec!["cpu.nice".into()];
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report: r.clone() }, 100)
        .unwrap()
    else {
        panic!()
    };
    let a = reply.assignments[0].clone();
    let mut record = prepared(&a);
    record.evidence = vec![ControlEvidence {
        control: "cpu.nice".into(),
        available: None,
        permitted: None,
        configured: true,
        applied: true,
        fallback: false,
        scope: "child".into(),
        requested: Some("10".into()),
        effective: Some("10".into()),
        detail: "ambiguous permission".into(),
    }];
    let request = |record| Request::Prepared {
        assignment_id: a.request.assignment_id.clone(),
        generation: a.generation,
        coordinator_epoch: a.coordinator_epoch,
        record,
    };
    assert!(c.handle_at(&node(), request(record.clone()), 101).is_err());
    record.evidence[0].available = Some(true);
    record.evidence[0].permitted = Some(true);
    record.identity.as_mut().unwrap().start_time = 0;
    assert!(c.handle_at(&node(), request(record.clone()), 102).is_err());
    record.identity.as_mut().unwrap().start_time = 456;
    assert!(c.handle_at(&node(), request(record.clone()), 3101).is_err());
    r.observed_at_unix_ms = 3102;
    c.handle_at(&node(), Request::Heartbeat { report: r }, 3102)
        .unwrap();
    assert!(matches!(
        c.handle_at(&node(), request(record), 3102).unwrap(),
        Response::Lease { .. }
    ));
}

#[test]
fn unknown_allocations_remain_fenced_through_omission_and_restart_until_verified_release() {
    let d = dir();
    let cfg = config(d.path());
    let mut c = Coordinator::open(cfg.clone()).unwrap();
    setup(&mut c, &["task"], 100);
    let mut r = report(100);
    r.allocations.push(AllocationReport {
        assignment_id: "unknown".into(),
        generation: 7,
        phase: RemotePhase::Running,
        observed: Some(Resources::default()),
        detail: "post-snapshot local journal".into(),
    });
    c.handle_at(&node(), Request::Heartbeat { report: r.clone() }, 100)
        .unwrap();
    assert!(offers(&mut c, 101).assignments.is_empty());
    drop(c);
    let mut c = Coordinator::open(cfg).unwrap();
    let reply = offers(&mut c, 102);
    assert!(reply.assignments.is_empty());
    assert_eq!(reply.uncertain, vec!["unknown"]);
    let Response::Status { nodes, .. } = c
        .handle_at(&Principal::Operator, Request::Status { job_id: None }, 102)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(nodes[0]["unrecognized_allocations"][0]["generation"], 7);
    r.observed_at_unix_ms = 103;
    r.allocations[0].phase = RemotePhase::Released;
    r.allocations[0].generation = 8;
    assert!(
        c.handle_at(&node(), Request::Heartbeat { report: r.clone() }, 103)
            .is_err()
    );
    r.allocations[0].generation = 7;
    r.observed_at_unix_ms = 104;
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report: r }, 104)
        .unwrap()
    else {
        panic!()
    };
    assert!(reply.uncertain.is_empty());
    assert_eq!(reply.assignments[0].request.task_id, "task");
}

#[test]
fn published_file_without_database_commit_recovers_after_real_restart_and_ack_replay() {
    use resource_manager::artifacts::ArtifactStore;
    use sha2::{Digest, Sha256};
    let d = dir();
    let cfg = config(d.path());
    let mut c = Coordinator::open(cfg.clone()).unwrap();
    setup(&mut c, &["task"], 100);
    let a = offers(&mut c, 100).assignments.remove(0);
    authorize(&mut c, &a, 100);
    let bytes = b"checkpoint publication boundary";
    let artifact = ArtifactRef {
        sha256: hex::encode(Sha256::digest(bytes)),
        size: bytes.len() as u64,
    };
    let Response::Upload { upload_id, .. } = c
        .handle_at(
            &node(),
            Request::BeginUpload {
                assignment_id: a.request.assignment_id.clone(),
                generation: a.generation,
                artifact: artifact.clone(),
            },
            101,
        )
        .unwrap()
    else {
        panic!()
    };
    c.handle_at(
        &node(),
        Request::UploadChunk {
            upload_id: upload_id.clone(),
            offset: 0,
            data_hex: hex::encode(&bytes[..5]),
        },
        102,
    )
    .unwrap();
    drop(c);
    let mut c = Coordinator::open(cfg.clone()).unwrap();
    let Response::Upload { offset, .. } = c
        .handle_at(
            &node(),
            Request::BeginUpload {
                assignment_id: a.request.assignment_id.clone(),
                generation: a.generation,
                artifact: artifact.clone(),
            },
            103,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(offset, 5);
    let Response::Upload { offset, .. } = c
        .handle_at(
            &node(),
            Request::UploadChunk {
                upload_id: upload_id.clone(),
                offset: 0,
                data_hex: hex::encode(&bytes[..5]),
            },
            104,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(offset, 5);
    c.handle_at(
        &node(),
        Request::UploadChunk {
            upload_id: upload_id.clone(),
            offset: 5,
            data_hex: hex::encode(&bytes[5..]),
        },
        105,
    )
    .unwrap();
    drop(c);
    // Construct the precise filesystem-before-DB fault boundary with the real
    // publication implementation. This is restart testing, not a power-loss test.
    let store = ArtifactStore::open(
        &cfg.state_dir.join("artifacts"),
        cfg.max_artifact_bytes,
        cfg.artifact_quota_bytes,
    )
    .unwrap();
    store.publish(&upload_id).unwrap();
    drop(store);
    assert!(
        cfg.state_dir
            .join("artifacts/blobs")
            .join(&artifact.sha256)
            .is_file()
    );
    let mut c = Coordinator::open(cfg.clone()).unwrap();
    let submission = ResultSubmission {
        task_id: "task".into(),
        assignment_id: a.request.assignment_id.clone(),
        generation: a.generation,
        result: json!({"checkpoint":"real bytes"}),
        artifacts: vec![artifact],
    };
    assert!(
        c.handle_at(
            &node(),
            Request::Complete {
                submission: submission.clone()
            },
            106
        )
        .is_err()
    );
    let first = c
        .handle_at(
            &node(),
            Request::CommitUpload {
                upload_id: upload_id.clone(),
            },
            107,
        )
        .unwrap();
    let repeated = c
        .handle_at(
            &node(),
            Request::CommitUpload {
                upload_id: upload_id.clone(),
            },
            108,
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(repeated).unwrap()
    );
    let receipt = c
        .handle_at(
            &node(),
            Request::Complete {
                submission: submission.clone(),
            },
            109,
        )
        .unwrap();
    drop(c);
    let mut c = Coordinator::open(cfg).unwrap();
    let repeated = c
        .handle_at(&node(), Request::Complete { submission }, 110)
        .unwrap();
    assert_eq!(
        serde_json::to_value(receipt).unwrap(),
        serde_json::to_value(repeated).unwrap()
    );
    let Response::Status {
        tasks, allocations, ..
    } = c
        .handle_at(&Principal::Operator, Request::Status { job_id: None }, 111)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        tasks[0].status,
        resource_manager::state::TaskStatus::Completed
    );
    assert_eq!(allocations[0]["phase"], "uncertain");
}

#[test]
fn four_thousand_task_soak_status_fits_bounded_transport_with_headroom() {
    let d = dir();
    let cfg = config(d.path());
    let mut c = Coordinator::open(cfg.clone()).unwrap();
    c.handle_at(&Principal::Operator, Request::PutPool { pool: pool() }, 100)
        .unwrap();
    let requests: Vec<_> = (0..4000)
        .map(|i| task(&format!("soak-task-{i:04}")))
        .collect();
    c.handle_at(
        &Principal::Operator,
        Request::Submit {
            job: JobSpec {
                job_id: "soak-experiment".into(),
                pool_id: "pool".into(),
                priority: 0,
                tasks: requests.clone(),
            },
        },
        100,
    )
    .unwrap();
    let mut connection = rusqlite::Connection::open(
        cfg.state_dir
            .join(resource_manager::state::DATABASE_FILENAME),
    )
    .unwrap();
    let tx = connection.transaction().unwrap();
    let mut r = report(101);
    for (i, request) in requests.iter().enumerate() {
        let id = format!("00000000-0000-4000-8000-{i:012}");
        tx.execute(
            "INSERT INTO assignments(assignment_id,task_id,generation) VALUES (?1,?2,1)",
            rusqlite::params![id, request.task_id],
        )
        .unwrap();
        tx.execute("UPDATE tasks SET status='completed',generation=1,assignment_id=?2,receipt_hash=?3 WHERE task_id=?1",rusqlite::params![request.task_id,id,"a".repeat(64)]).unwrap();
        let detail = "verified process and GPU release; immutable result receipt accepted; no retained family allocations; completed bounded worker generation";
        tx.execute("INSERT INTO reservations(assignment_id,node_id,phase,resources_json,observed_json,lease_sequence,lease_deadline_ms,epoch,detail) VALUES (?1,'node','released',?2,NULL,1,10000,?3,?4)",rusqlite::params![id,serde_json::to_string(&request.resources).unwrap(),i64::try_from(c.epoch()).unwrap(),detail]).unwrap();
        r.allocations.push(AllocationReport {
            assignment_id: id,
            generation: 1,
            phase: RemotePhase::Released,
            observed: None,
            detail: detail.into(),
        });
    }
    tx.commit().unwrap();
    drop(connection);
    c.handle_at(&node(), Request::Heartbeat { report: r }, 101)
        .unwrap();
    let status = c
        .handle_at(&Principal::Operator, Request::Status { job_id: None }, 102)
        .unwrap();
    let bytes = serde_json::to_vec(&status).unwrap();
    eprintln!("status_payload_4000_tasks_bytes={}", bytes.len());
    // Numeric budget fixed before evaluating: both clients cap status at 8 MiB;
    // the approved bounded soak shape must leave at least 1 MiB of headroom.
    assert!(
        bytes.len() < 7 * 1024 * 1024,
        "4000-task status needs pagination or a smaller approved run shape: {}",
        bytes.len()
    );
    let Response::Status {
        tasks,
        allocations,
        nodes,
        ..
    } = status
    else {
        panic!()
    };
    assert_eq!(tasks.len(), 4000);
    assert_eq!(allocations.len(), 4000);
    assert_eq!(
        nodes[0]["report"]["allocations"].as_array().unwrap().len(),
        4000
    );
}

#[test]
fn immutable_attempt_cap_counts_yields_and_cannot_be_bypassed_by_retry_or_restart() {
    let d = dir();
    let cfg = config(d.path());
    let mut c = Coordinator::open(cfg.clone()).unwrap();
    c.handle_at(&Principal::Operator, Request::PutPool { pool: pool() }, 100)
        .unwrap();
    let mut t = task("bounded");
    t.max_attempts = Some(2);
    c.handle_at(
        &Principal::Operator,
        Request::Submit {
            job: JobSpec {
                job_id: "bounded-job".into(),
                pool_id: "pool".into(),
                priority: 0,
                tasks: vec![t],
            },
        },
        100,
    )
    .unwrap();
    let first = offers(&mut c, 100).assignments.remove(0);
    authorize(&mut c, &first, 100);
    released_failure(&mut c, &first, 101, FailureKind::Yielded);
    // A lost failure ACK must not consume another attempt or extend the backoff.
    c.handle_at(
        &node(),
        Request::Fail {
            assignment_id: first.request.assignment_id,
            generation: 1,
            detail: "same yielded report after lost ACK".into(),
            failure_kind: FailureKind::Yielded,
        },
        500,
    )
    .unwrap();
    drop(c);
    let mut c = Coordinator::open(cfg.clone()).unwrap();
    assert!(offers(&mut c, 1100).assignments.is_empty());
    let second = offers(&mut c, 1101).assignments.remove(0);
    assert_eq!(second.generation, 2);
    // Reservation/preparation attempts also consume the cap even when no EXEC occurred.
    released_failure(&mut c, &second, 1102, FailureKind::Yielded);
    let Response::Status { tasks, .. } = c
        .handle_at(&Principal::Operator, Request::Status { job_id: None }, 1103)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        tasks[0].status,
        resource_manager::state::TaskStatus::NeedsReconciliation
    );
    assert_eq!(tasks[0].generation, 2);
    assert!(
        c.handle_at(
            &Principal::Operator,
            Request::Retry {
                task_id: "bounded".into(),
                confirm_side_effects_reconciled: true
            },
            1104
        )
        .is_err()
    );
    drop(c);
    let mut c = Coordinator::open(cfg).unwrap();
    assert!(offers(&mut c, 100000).assignments.is_empty());
}
#[test]
fn yield_delay_grows_without_consuming_execution_failure_budget_and_old_state_migrates() {
    let d = dir();
    let cfg = config(d.path());
    let mut c = Coordinator::open(cfg.clone()).unwrap();
    setup(&mut c, &["task"], 100);
    let first = offers(&mut c, 100).assignments.remove(0);
    released_failure(&mut c, &first, 101, FailureKind::Yielded);
    assert!(offers(&mut c, 1100).assignments.is_empty());
    let second = offers(&mut c, 1101).assignments.remove(0);
    released_failure(&mut c, &second, 1102, FailureKind::Yielded);
    assert!(offers(&mut c, 3101).assignments.is_empty());
    let third = offers(&mut c, 3102).assignments.remove(0);
    assert_eq!(third.generation, 3);
    released_failure(&mut c, &third, 3103, FailureKind::ExecutionFailure);
    drop(c);
    let connection = rusqlite::Connection::open(
        cfg.state_dir
            .join(resource_manager::state::DATABASE_FILENAME),
    )
    .unwrap();
    connection
        .execute_batch("ALTER TABLE retry_state DROP COLUMN yield_count")
        .unwrap();
    drop(connection);
    let mut c = Coordinator::open(cfg.clone()).unwrap();
    assert!(offers(&mut c, 4102).assignments.is_empty());
    assert_eq!(offers(&mut c, 4103).assignments[0].generation, 4);
    let connection = rusqlite::Connection::open(
        cfg.state_dir
            .join(resource_manager::state::DATABASE_FILENAME),
    )
    .unwrap();
    let (failures, yields): (i64, i64) = connection
        .query_row(
            "SELECT failures,yield_count FROM retry_state WHERE task_id='task'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((failures, yields), (1, 0));
}

#[test]
fn cpu_admission_cannot_reuse_zero_or_retained_gpu_capacity_without_gpu_evidence() {
    for gpu_budget in [0, 4096] {
        let d = dir();
        let mut c = Coordinator::open(config(d.path())).unwrap();
        c.handle_at(&Principal::Operator, Request::PutPool { pool: pool() }, 100)
            .unwrap();
        let mut gpu = task("gpu-first");
        gpu.resources.gpu_memory_mib.insert("GPU-test".into(), 512);
        c.handle_at(
            &Principal::Operator,
            Request::Submit {
                job: JobSpec {
                    job_id: "mixed-job".into(),
                    pool_id: "pool".into(),
                    priority: 0,
                    tasks: vec![gpu, task("cpu-second")],
                },
            },
            100,
        )
        .unwrap();
        let mut r = report(100);
        r.managed_budget
            .gpu_memory_mib
            .insert("GPU-test".into(), gpu_budget);
        r.gpu_expansion_allowed = false;
        let Response::Heartbeat { reply } = c
            .handle_at(&node(), Request::Heartbeat { report: r.clone() }, 100)
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(reply.assignments.len(), 1);
        assert_eq!(reply.assignments[0].request.task_id, "cpu-second");
        let Response::Status { tasks, .. } = c
            .handle_at(&Principal::Operator, Request::Status { job_id: None }, 100)
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            tasks
                .iter()
                .find(|t| t.task_id == "gpu-first")
                .unwrap()
                .generation,
            0
        );
        assert_eq!(
            tasks
                .iter()
                .find(|t| t.task_id == "gpu-first")
                .unwrap()
                .status,
            resource_manager::state::TaskStatus::Queued
        );
        let mut old = serde_json::to_value(&r).unwrap();
        old.as_object_mut().unwrap().remove("gpu_expansion_allowed");
        assert!(
            !serde_json::from_value::<NodeReport>(old)
                .unwrap()
                .gpu_expansion_allowed
        );
        r.observed_at_unix_ms = 101;
        r.managed_budget
            .gpu_memory_mib
            .insert("GPU-test".into(), 4096);
        r.gpu_expansion_allowed = true;
        let Response::Heartbeat { reply } = c
            .handle_at(&node(), Request::Heartbeat { report: r.clone() }, 101)
            .unwrap()
        else {
            panic!()
        };
        let offered = reply
            .assignments
            .iter()
            .find(|a| a.request.task_id == "gpu-first")
            .unwrap();
        // GPU evidence can disappear after reservation; the launch barrier must
        // recheck it before granting first execution authority.
        r.gpu_expansion_allowed = false;
        r.observed_at_unix_ms = 102;
        c.handle_at(&node(), Request::Heartbeat { report: r }, 102)
            .unwrap();
        assert!(
            c.handle_at(
                &node(),
                Request::Prepared {
                    coordinator_epoch: offered.coordinator_epoch,
                    assignment_id: offered.request.assignment_id.clone(),
                    generation: offered.generation,
                    record: prepared(offered),
                },
                102
            )
            .is_err()
        );
    }
}

fn report_reply(c: &mut Coordinator, r: &NodeReport, now: u64) -> HeartbeatReply {
    let Response::Heartbeat { reply } = c
        .handle_at(&node(), Request::Heartbeat { report: r.clone() }, now)
        .unwrap()
    else {
        panic!()
    };
    reply
}
fn gpu_task(id: &str, uuid: &str, cpu: u64) -> LaunchRequest {
    let mut request = task(id);
    request.resources.cpu_millicores = cpu;
    request.resources.gpu_memory_mib.insert(uuid.into(), 500);
    request
}
fn submit_tasks(
    c: &mut Coordinator,
    job_id: &str,
    priority: i32,
    tasks: Vec<LaunchRequest>,
    now: u64,
) {
    c.handle_at(
        &Principal::Operator,
        Request::Submit {
            job: JobSpec {
                job_id: job_id.into(),
                pool_id: "pool".into(),
                priority,
                tasks,
            },
        },
        now,
    )
    .unwrap();
}
fn wide_pool(c: &mut Coordinator, now: u64) {
    let mut p = pool();
    p.max_workers = 8;
    c.handle_at(&Principal::Operator, Request::PutPool { pool: p }, now)
        .unwrap();
}
fn allocation_report(a: &Assignment, phase: RemotePhase) -> AllocationReport {
    AllocationReport {
        assignment_id: a.request.assignment_id.clone(),
        generation: a.generation,
        phase,
        observed: Some(a.request.resources.clone()),
        detail: "scoped coordinator regression".into(),
    }
}

#[test]
fn unobservable_uuid_does_not_block_cpu_or_other_gpu_but_all_uncertain_charges_remain() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    wide_pool(&mut c, 100);
    submit_tasks(
        &mut c,
        "original",
        0,
        vec![gpu_task("original", "GPU-bad", 500)],
        100,
    );
    let mut r = report(100);
    r.launch_slots = 4;
    r.managed_budget.gpu_memory_mib =
        BTreeMap::from([("GPU-bad".into(), 1000), ("GPU-good".into(), 1000)]);
    let a = report_reply(&mut c, &r, 100).assignments.remove(0);
    authorize(&mut c, &a, 100);
    let mut cpu = task("cpu");
    cpu.resources.cpu_millicores = 500;
    submit_tasks(
        &mut c,
        "new",
        0,
        vec![
            cpu,
            gpu_task("good", "GPU-good", 500),
            gpu_task("bad", "GPU-bad", 500),
            task("excess-cpu"),
        ],
        101,
    );
    r.observed_at_unix_ms = 101;
    r.managed_budget.gpu_memory_mib.insert("GPU-bad".into(), 0);
    r.allocations = vec![allocation_report(&a, RemotePhase::Uncertain)];
    let reply = report_reply(&mut c, &r, 101);
    assert!(reply.uncertain.contains(&a.request.assignment_id));
    assert_eq!(
        reply.drain.as_slice(),
        std::slice::from_ref(&a.request.assignment_id)
    );
    assert_eq!(
        reply
            .assignments
            .iter()
            .map(|a| a.request.task_id.as_str())
            .collect::<Vec<_>>(),
        ["cpu", "good"]
    );
    // Both first authorizations include the uncertain CPU/RAM charge exactly once
    // and ignore only the unrelated GPU deficit, never its CPU/RAM reservation.
    for offered in &reply.assignments {
        assert_eq!(authorize(&mut c, offered, 101).sequence, 1);
    }
    let Response::Status {
        tasks, allocations, ..
    } = c
        .handle_at(&Principal::Operator, Request::Status { job_id: None }, 101)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(allocations.len(), 3);
    assert_eq!(
        allocations
            .iter()
            .filter(|a| a["phase"] == "uncertain")
            .count(),
        1
    );
    for id in ["bad", "excess-cpu"] {
        let task = tasks.iter().find(|t| t.task_id == id).unwrap();
        assert_eq!(task.generation, 0);
        assert_eq!(task.status, resource_manager::state::TaskStatus::Queued);
    }
}

#[test]
fn preemption_uses_requested_gpu_and_waits_for_release_with_unrelated_uncertainty() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    wide_pool(&mut c, 100);
    submit_tasks(
        &mut c,
        "initial",
        0,
        vec![
            gpu_task("uncertain", "GPU-bad", 500),
            gpu_task("low", "GPU-good", 1000),
        ],
        100,
    );
    let mut r = report(100);
    r.managed_budget.gpu_memory_mib =
        BTreeMap::from([("GPU-bad".into(), 1000), ("GPU-good".into(), 1000)]);
    let initial = report_reply(&mut c, &r, 100).assignments;
    let bad = initial
        .iter()
        .find(|a| a.request.task_id == "uncertain")
        .unwrap();
    let low = initial.iter().find(|a| a.request.task_id == "low").unwrap();
    authorize(&mut c, bad, 100);
    authorize(&mut c, low, 100);
    submit_tasks(
        &mut c,
        "urgent",
        100,
        vec![gpu_task("high", "GPU-good", 1000)],
        101,
    );
    r.observed_at_unix_ms = 101;
    r.managed_budget.gpu_memory_mib.insert("GPU-bad".into(), 0);
    r.allocations = vec![
        allocation_report(bad, RemotePhase::Uncertain),
        allocation_report(low, RemotePhase::Running),
    ];
    let requested = report_reply(&mut c, &r, 101);
    assert!(requested.assignments.is_empty());
    assert_eq!(requested.drain.len(), 2);
    assert!(requested.drain.contains(&low.request.assignment_id));
    assert!(requested.drain.contains(&bad.request.assignment_id));
    assert!(requested.uncertain.contains(&bad.request.assignment_id));
    r.allocations[1].phase = RemotePhase::Draining;
    r.observed_at_unix_ms = 102;
    assert!(report_reply(&mut c, &r, 102).assignments.is_empty());
    r.allocations[1].phase = RemotePhase::Released;
    r.observed_at_unix_ms = 103;
    let released = report_reply(&mut c, &r, 103);
    assert_eq!(released.assignments.len(), 1);
    assert_eq!(released.assignments[0].request.task_id, "high");
    assert_eq!(authorize(&mut c, &released.assignments[0], 103).sequence, 1);
}

#[test]
fn first_gpu_authorization_rechecks_requested_uuid_when_another_gpu_is_eligible() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    wide_pool(&mut c, 100);
    submit_tasks(
        &mut c,
        "gpu",
        0,
        vec![gpu_task("gpu", "GPU-requested", 500)],
        100,
    );
    let mut r = report(100);
    r.managed_budget.gpu_memory_mib =
        BTreeMap::from([("GPU-requested".into(), 1000), ("GPU-other".into(), 1000)]);
    let a = report_reply(&mut c, &r, 100).assignments.remove(0);
    r.managed_budget
        .gpu_memory_mib
        .insert("GPU-requested".into(), 0);
    r.observed_at_unix_ms = 101;
    assert!(r.gpu_expansion_allowed);
    report_reply(&mut c, &r, 101);
    let error = c
        .handle_at(
            &node(),
            Request::Prepared {
                assignment_id: a.request.assignment_id.clone(),
                generation: a.generation,
                coordinator_epoch: a.coordinator_epoch,
                record: prepared(&a),
            },
            101,
        )
        .unwrap_err();
    assert!(error.to_string().contains("per-device"));
    let Response::Status { allocations, .. } = c
        .handle_at(&Principal::Operator, Request::Status { job_id: None }, 101)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(allocations.len(), 1);
    assert_eq!(allocations[0]["phase"], "offered");
}

#[test]
fn gpu_guaranteed_capability_defaults_closed_without_disabling_cpu_guaranteed() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    let mut p = pool();
    p.class = AllocationClass::Guaranteed;
    c.handle_at(&Principal::Operator, Request::PutPool { pool: p }, 100)
        .unwrap();
    let mut gpu = gpu_task("gpu", "GPU-test", 1000);
    gpu.class = AllocationClass::Guaranteed;
    let mut cpu = task("cpu");
    cpu.class = AllocationClass::Guaranteed;
    submit_tasks(&mut c, "guaranteed", 0, vec![gpu, cpu], 100);
    let mut r = report(100);
    r.managed_budget
        .gpu_memory_mib
        .insert("GPU-test".into(), 1000);
    let mut old = serde_json::to_value(&r).unwrap();
    old.as_object_mut()
        .unwrap()
        .remove("gpu_guaranteed_allowed");
    r = serde_json::from_value(old).unwrap();
    assert!(!r.gpu_guaranteed_allowed);
    let reply = report_reply(&mut c, &r, 100);
    assert_eq!(reply.assignments.len(), 1);
    assert_eq!(reply.assignments[0].request.task_id, "cpu");
    authorize(&mut c, &reply.assignments[0], 100);
    r.allocations.push(allocation_report(
        &reply.assignments[0],
        RemotePhase::Running,
    ));
    r.observed_at_unix_ms = 101;
    r.gpu_guaranteed_allowed = true;
    let offered = report_reply(&mut c, &r, 101).assignments.remove(0);
    assert_eq!(offered.request.task_id, "gpu");
    // Capability revocation after placement still stops the launch barrier.
    r.observed_at_unix_ms = 102;
    r.gpu_guaranteed_allowed = false;
    report_reply(&mut c, &r, 102);
    assert!(
        c.handle_at(
            &node(),
            Request::Prepared {
                assignment_id: offered.request.assignment_id.clone(),
                generation: offered.generation,
                coordinator_epoch: offered.coordinator_epoch,
                record: prepared(&offered)
            },
            102
        )
        .is_err()
    );
}

#[test]
fn first_gpu_authorization_counts_pending_siblings_once_and_blocks_reduced_cpu_budget() {
    for reduce_budget in [false, true] {
        let d = dir();
        let mut c = Coordinator::open(config(d.path())).unwrap();
        wide_pool(&mut c, 100);
        submit_tasks(
            &mut c,
            "gpu",
            0,
            vec![
                gpu_task("a", "GPU-test", 1000),
                gpu_task("b", "GPU-test", 1000),
            ],
            100,
        );
        let mut r = report(100);
        r.managed_budget
            .gpu_memory_mib
            .insert("GPU-test".into(), 1000);
        let assignments = report_reply(&mut c, &r, 100).assignments;
        assert_eq!(assignments.len(), 2);
        if reduce_budget {
            r.managed_budget.cpu_millicores = 1000;
            r.observed_at_unix_ms = 101;
            report_reply(&mut c, &r, 101);
        }
        let a = &assignments[0];
        let response = c.handle_at(
            &node(),
            Request::Prepared {
                assignment_id: a.request.assignment_id.clone(),
                generation: a.generation,
                coordinator_epoch: a.coordinator_epoch,
                record: prepared(a),
            },
            101,
        );
        if reduce_budget {
            assert!(response.unwrap_err().to_string().contains("budget"));
        } else {
            assert!(matches!(
                response.unwrap(),
                Response::Lease {
                    lease: Lease { sequence: 1, .. }
                }
            ));
        }
    }
}

fn replay_open(c: &mut Coordinator, session: &str, now: u64) -> ReplayRecoverySnapshot {
    let Response::ReplayRecovery { snapshot } = c
        .handle_at(
            &node(),
            Request::OpenReplaySession {
                node_id: "node".into(),
                boot_id: "boot".into(),
                session_id: session.into(),
            },
            now,
        )
        .unwrap()
    else {
        panic!()
    };
    snapshot
}
fn in_session(session: &str, request: Request) -> Request {
    Request::NodeSession {
        session_id: session.into(),
        request: Box::new(request),
    }
}
fn replay_heartbeat(
    c: &mut Coordinator,
    session: &str,
    report: NodeReport,
    now: u64,
) -> HeartbeatReply {
    let Response::Heartbeat { reply } = c
        .handle_at(
            &node(),
            in_session(session, Request::Heartbeat { report }),
            now,
        )
        .unwrap()
    else {
        panic!()
    };
    reply
}
fn replay_allocation_report(a: &Assignment, phase: RemotePhase) -> AllocationReport {
    AllocationReport {
        assignment_id: a.request.assignment_id.clone(),
        generation: a.generation,
        phase,
        observed: Some(a.request.resources.clone()),
        detail: "independent recovery evidence fixture".into(),
    }
}
fn replay_database(path: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path.join("state/state.sqlite3")).unwrap()
}

#[test]
fn replay_coordinator_requires_strong_storage_before_creating_any_state() {
    let d = dir();
    let mut cfg = config(d.path());
    cfg.storage_profile = resource_manager::state::StorageProfile::BurstReplayDeleteExtra;
    assert!(!cfg.state_dir.exists());
    assert!(
        Coordinator::open(cfg.clone())
            .err()
            .unwrap()
            .to_string()
            .contains("coordinator authority requires durable local storage")
    );
    assert!(!cfg.state_dir.exists());
}

#[test]
fn replay_sessions_require_owning_certificate_and_reject_all_stale_or_unwrapped_operations() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    let first = uuid::Uuid::new_v4().to_string();
    let second = uuid::Uuid::new_v4().to_string();
    let open = Request::OpenReplaySession {
        node_id: "node".into(),
        boot_id: "boot".into(),
        session_id: first.clone(),
    };
    assert!(
        c.handle_at(&Principal::Operator, open.clone(), 100)
            .is_err()
    );
    assert!(
        c.handle_at(
            &Principal::Node {
                node_id: "other".into()
            },
            open.clone(),
            100
        )
        .is_err()
    );
    assert!(
        c.handle_at(
            &node(),
            Request::OpenReplaySession {
                node_id: "node".into(),
                boot_id: "boot".into(),
                session_id: "not-a-uuid".into()
            },
            100
        )
        .is_err()
    );
    replay_open(&mut c, &first, 100);
    assert!(
        c.handle_at(
            &node(),
            in_session(
                &first,
                Request::ReadArtifact {
                    sha256: "a".repeat(64),
                    offset: 0,
                    max_bytes: 1
                }
            ),
            100
        )
        .unwrap_err()
        .to_string()
        .contains("complete recovery heartbeat")
    );
    replay_heartbeat(&mut c, &first, report(100), 100);
    replay_open(&mut c, &second, 101);
    assert!(
        c.handle_at(&node(), open, 102)
            .unwrap_err()
            .to_string()
            .contains("retired")
    );
    let submission = ResultSubmission {
        task_id: "task".into(),
        assignment_id: "old".into(),
        generation: 1,
        result: json!({}),
        artifacts: vec![],
    };
    let requests = vec![
        Request::Heartbeat {
            report: report(102),
        },
        Request::Renew {
            assignment_id: "old".into(),
            generation: 1,
            coordinator_epoch: c.epoch(),
            previous_sequence: 1,
        },
        Request::Complete {
            submission: submission.clone(),
        },
        Request::PublishCheckpoint { submission },
        Request::Fail {
            assignment_id: "old".into(),
            generation: 1,
            detail: "old".into(),
            failure_kind: FailureKind::Yielded,
        },
        Request::BeginUpload {
            assignment_id: "old".into(),
            generation: 1,
            artifact: ArtifactRef {
                sha256: "a".repeat(64),
                size: 1,
            },
        },
        Request::UploadChunk {
            upload_id: "old".into(),
            offset: 0,
            data_hex: "00".into(),
        },
        Request::CommitUpload {
            upload_id: "old".into(),
        },
        Request::AbortUpload {
            upload_id: "old".into(),
        },
        Request::ReadArtifact {
            sha256: "a".repeat(64),
            offset: 0,
            max_bytes: 1,
        },
    ];
    for request in requests {
        assert!(
            c.handle_at(&node(), request.clone(), 102)
                .unwrap_err()
                .to_string()
                .contains("requires a current session wrapper")
        );
        assert!(
            c.handle_at(&node(), in_session(&first, request), 102)
                .unwrap_err()
                .to_string()
                .contains("stale replay node session")
        );
    }
    let heartbeat = Request::Heartbeat {
        report: report(102),
    };
    assert!(
        c.handle_at(
            &node(),
            in_session(&second, in_session(&second, heartbeat.clone())),
            102
        )
        .is_err()
    );
    assert!(
        c.handle_at(&Principal::Operator, in_session(&second, heartbeat), 102)
            .is_err()
    );
    let mut wrong_boot = report(102);
    wrong_boot.boot_id = "another-boot".into();
    assert!(
        c.handle_at(
            &node(),
            in_session(&second, Request::Heartbeat { report: wrong_boot }),
            102
        )
        .is_err()
    );
    assert!(matches!(
        c.handle_at(&Principal::Operator, Request::Status { job_id: None }, 102)
            .unwrap(),
        Response::Status { .. }
    ));
}

#[test]
fn replay_session_retry_does_not_refence_new_work_or_reset_durable_ready() {
    let d = dir();
    let cfg = config(d.path());
    let mut c = Coordinator::open(cfg.clone()).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    assert!(replay_open(&mut c, &session, 100).allocations.is_empty());
    replay_heartbeat(&mut c, &session, report(100), 100);
    setup(&mut c, &["task"], 101);
    let a = replay_heartbeat(&mut c, &session, report(101), 101)
        .assignments
        .remove(0);
    let snapshot = replay_open(&mut c, &session, 102);
    assert_eq!(snapshot.allocations.len(), 1);
    let Response::Lease { lease } = c
        .handle_at(
            &node(),
            in_session(
                &session,
                Request::Prepared {
                    assignment_id: a.request.assignment_id.clone(),
                    generation: a.generation,
                    coordinator_epoch: a.coordinator_epoch,
                    record: prepared(&a),
                },
            ),
            102,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(lease.sequence, 1);
    let db = replay_database(d.path());
    assert_eq!(
        db.query_row("SELECT ready FROM replay_node_sessions", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT value FROM distributed_meta WHERE key='schema'",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        2
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM allocation_fences", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        0
    );
    drop(db);
    drop(c);
    let mut c = Coordinator::open(cfg).unwrap();
    let db = replay_database(d.path());
    assert_eq!(
        db.query_row("SELECT ready FROM replay_node_sessions", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        1
    );
    let mut current = report(103);
    current.allocations = vec![replay_allocation_report(&a, RemotePhase::Running)];
    replay_heartbeat(&mut c, &session, current, 103);
    let epoch = c.epoch();
    c.handle_at(
        &node(),
        in_session(
            &session,
            Request::Renew {
                assignment_id: a.request.assignment_id,
                generation: a.generation,
                coordinator_epoch: epoch,
                previous_sequence: 1,
            },
        ),
        103,
    )
    .unwrap();
}

#[test]
fn replay_recovery_requires_every_retained_generation_even_unprepared_offers() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    setup(&mut c, &["prepared", "unprepared"], 100);
    let old = offers(&mut c, 100).assignments;
    authorize(&mut c, &old[0], 100);
    let session = uuid::Uuid::new_v4().to_string();
    let snapshot = replay_open(&mut c, &session, 101);
    assert_eq!(snapshot.allocations.len(), 2);
    let prepared_snapshot = snapshot
        .allocations
        .iter()
        .find(|item| item.assignment.request.assignment_id == old[0].request.assignment_id)
        .unwrap();
    assert_eq!(
        prepared_snapshot.prepared.as_ref().unwrap().identity,
        prepared(&old[0]).identity
    );
    assert_eq!(prepared_snapshot.previous_boot_id.as_deref(), Some("boot"));
    assert_eq!(prepared_snapshot.lease_sequence, 1);
    let unprepared = snapshot
        .allocations
        .iter()
        .find(|item| item.assignment.request.assignment_id == old[1].request.assignment_id)
        .unwrap();
    assert!(unprepared.prepared.is_none());
    assert_eq!(unprepared.lease_sequence, 0);
    for case in 0..3 {
        let mut incomplete = report(102);
        incomplete.allocations = old
            .iter()
            .map(|a| replay_allocation_report(a, RemotePhase::Uncertain))
            .collect();
        match case {
            0 => {
                incomplete.allocations.pop();
            }
            1 => incomplete.allocations[0].generation += 1,
            2 => incomplete.allocations[0].phase = RemotePhase::Running,
            _ => unreachable!(),
        }
        assert!(
            c.handle_at(
                &node(),
                in_session(&session, Request::Heartbeat { report: incomplete }),
                102
            )
            .unwrap_err()
            .to_string()
            .contains("every retained allocation")
        );
    }
    let db = replay_database(d.path());
    assert_eq!(
        db.query_row("SELECT ready FROM replay_node_sessions", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM reservations WHERE phase='uncertain'",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        2
    );
    c.handle_at(
        &Principal::Operator,
        Request::Submit {
            job: JobSpec {
                job_id: "another".into(),
                pool_id: "pool".into(),
                priority: 0,
                tasks: vec![task("new")],
            },
        },
        102,
    )
    .unwrap();
    let mut complete = report(103);
    complete.managed_budget.cpu_millicores = 8000;
    complete.allocations = old
        .iter()
        .map(|a| replay_allocation_report(a, RemotePhase::Uncertain))
        .collect();
    let reply = replay_heartbeat(&mut c, &session, complete, 103);
    assert!(reply.assignments.iter().all(|a| a.request.task_id != "new"));
    assert_eq!(reply.uncertain.len(), 2);
    assert_eq!(
        db.query_row("SELECT ready FROM replay_node_sessions", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM reservations WHERE phase!='released'",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        2
    );
}

#[test]
fn replay_placement_excludes_unsafe_guaranteed_and_durable_local_even_if_advertised() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    replay_open(&mut c, &session, 100);
    replay_heartbeat(&mut c, &session, report(100), 100);
    let mut unsafe_task = task("unsafe");
    unsafe_task.replay_safe = false;
    let mut durable_task = task("durable");
    durable_task
        .required_controls
        .push("storage.durable_local".into());
    let mut guaranteed_pool = pool();
    guaranteed_pool.pool_id = "guaranteed-pool".into();
    guaranteed_pool.class = AllocationClass::Guaranteed;
    let mut guaranteed = task("guaranteed");
    guaranteed.class = AllocationClass::Guaranteed;
    c.handle_at(&Principal::Operator, Request::PutPool { pool: pool() }, 100)
        .unwrap();
    c.handle_at(
        &Principal::Operator,
        Request::PutPool {
            pool: guaranteed_pool,
        },
        100,
    )
    .unwrap();
    for (id, pool_id, tasks) in [
        (
            "normal",
            "pool",
            vec![unsafe_task, durable_task, task("eligible")],
        ),
        ("strict", "guaranteed-pool", vec![guaranteed]),
    ] {
        c.handle_at(
            &Principal::Operator,
            Request::Submit {
                job: JobSpec {
                    job_id: id.into(),
                    pool_id: pool_id.into(),
                    priority: 0,
                    tasks,
                },
            },
            100,
        )
        .unwrap();
    }
    let mut capacity = report(101);
    capacity
        .available_controls
        .push("storage.durable_local".into());
    let reply = replay_heartbeat(&mut c, &session, capacity, 101);
    assert_eq!(
        reply
            .assignments
            .iter()
            .map(|a| a.request.task_id.as_str())
            .collect::<Vec<_>>(),
        ["eligible"]
    );
    let a = &reply.assignments[0];
    // Defensive authorization recheck, independently of the placement filter.
    let db = replay_database(d.path());
    db.execute("UPDATE task_specs SET request_json=json_set(request_json,'$.replay_safe',json('false')) WHERE task_id='eligible'", []).unwrap();
    assert!(
        c.handle_at(
            &node(),
            in_session(
                &session,
                Request::Prepared {
                    assignment_id: a.request.assignment_id.clone(),
                    generation: a.generation,
                    coordinator_epoch: a.coordinator_epoch,
                    record: prepared(a)
                }
            ),
            101
        )
        .unwrap_err()
        .to_string()
        .contains("replay-safe opportunistic")
    );
}

#[test]
fn replay_snapshot_retains_unrecognized_allocations_until_explicit_release() {
    let d = dir();
    let mut c = Coordinator::open(config(d.path())).unwrap();
    let orphan = AllocationReport {
        assignment_id: "unknown-attempt".into(),
        generation: 7,
        phase: RemotePhase::Uncertain,
        observed: None,
        detail: "state loss".into(),
    };
    let mut old = report(100);
    old.allocations.push(orphan.clone());
    c.handle_at(&node(), Request::Heartbeat { report: old }, 100)
        .unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let snapshot = replay_open(&mut c, &session, 101);
    assert_eq!(snapshot.unrecognized.len(), 1);
    assert_eq!(snapshot.unrecognized[0].generation, 7);
    setup(&mut c, &["task"], 101);
    let omitted = replay_heartbeat(&mut c, &session, report(102), 102);
    assert!(omitted.assignments.is_empty());
    assert_eq!(omitted.uncertain, ["unknown-attempt"]);
    let mut released = report(103);
    released.allocations.push(AllocationReport {
        phase: RemotePhase::Released,
        ..orphan
    });
    assert_eq!(
        replay_heartbeat(&mut c, &session, released, 103)
            .assignments
            .len(),
        1
    );
}

#[test]
fn replay_strong_checkpoint_and_result_commits_survive_node_loss_and_coordinator_restart() {
    let d = dir();
    let cfg = config(d.path());
    let mut c = Coordinator::open(cfg.clone()).unwrap();
    setup(&mut c, &["checkpoint", "result"], 100);
    let offers = offers(&mut c, 100).assignments;
    for a in &offers {
        authorize(&mut c, a, 100);
    }
    let cp_assignment = offers
        .iter()
        .find(|a| a.request.task_id == "checkpoint")
        .unwrap();
    let result_assignment = offers
        .iter()
        .find(|a| a.request.task_id == "result")
        .unwrap();
    let checkpoint = ResultSubmission {
        task_id: "checkpoint".into(),
        assignment_id: cp_assignment.request.assignment_id.clone(),
        generation: cp_assignment.generation,
        result: json!({"checkpoint_sequence":1,"position":37}),
        artifacts: vec![],
    };
    let result = ResultSubmission {
        task_id: "result".into(),
        assignment_id: result_assignment.request.assignment_id.clone(),
        generation: result_assignment.generation,
        result: json!({"value":1.2345678901234567}),
        artifacts: vec![],
    };
    let cp_receipt = c
        .handle_at(
            &node(),
            Request::PublishCheckpoint {
                submission: checkpoint.clone(),
            },
            100,
        )
        .unwrap();
    let result_receipt = c
        .handle_at(
            &node(),
            Request::Complete {
                submission: result.clone(),
            },
            100,
        )
        .unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let snapshot = replay_open(&mut c, &session, 101);
    assert_eq!(
        snapshot
            .allocations
            .iter()
            .find(|a| a.assignment.request.task_id == "checkpoint")
            .unwrap()
            .assignment
            .checkpoint
            .as_ref()
            .unwrap()
            .result["position"],
        37
    );
    let mut recovered = report(102);
    recovered.allocations = offers
        .iter()
        .map(|a| replay_allocation_report(a, RemotePhase::Uncertain))
        .collect();
    replay_heartbeat(&mut c, &session, recovered, 102);
    drop(c);
    let mut c = Coordinator::open(cfg).unwrap();
    let same_cp = c
        .handle_at(
            &node(),
            in_session(
                &session,
                Request::PublishCheckpoint {
                    submission: checkpoint.clone(),
                },
            ),
            103,
        )
        .unwrap();
    let same_result = c
        .handle_at(
            &node(),
            in_session(
                &session,
                Request::Complete {
                    submission: result.clone(),
                },
            ),
            103,
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(cp_receipt).unwrap(),
        serde_json::to_value(same_cp).unwrap()
    );
    assert_eq!(
        serde_json::to_value(result_receipt).unwrap(),
        serde_json::to_value(same_result).unwrap()
    );
    let mut replacement = checkpoint;
    replacement.result["position"] = json!(99);
    assert!(
        c.handle_at(
            &node(),
            in_session(
                &session,
                Request::PublishCheckpoint {
                    submission: replacement
                }
            ),
            103
        )
        .is_err()
    );
    let Response::Result { submission } = c
        .handle_at(
            &Principal::Operator,
            Request::GetResult {
                task_id: "result".into(),
            },
            103,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        serde_json::to_value(submission.unwrap()).unwrap(),
        serde_json::to_value(result).unwrap()
    );
}
