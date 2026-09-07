//! Bounded component-cost experiment. Does not claim whole-daemon overhead.
#![cfg(unix)]
use resource_manager::{execution_model::*, model::Resources, protocol::*, state::StateStore};
use rusqlite::params;
use std::{collections::BTreeMap, fs, time::Instant};

fn usage() -> (f64, i64) {
    let mut value: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut value) }, 0);
    (
        value.ru_utime.tv_sec as f64
            + value.ru_utime.tv_usec as f64 / 1e6
            + value.ru_stime.tv_sec as f64
            + value.ru_stime.tv_usec as f64 / 1e6,
        value.ru_maxrss,
    )
}

#[test]
#[ignore = "explicit bounded history-component measurement; creates no workloads"]
fn four_thousand_released_records_component_cost() {
    let temporary = tempfile::Builder::new()
        .prefix(".history-cost-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    let store = StateStore::open(temporary.path()).unwrap();
    let mut connection =
        rusqlite::Connection::open(temporary.path().join("state.sqlite3")).unwrap();
    connection
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    let transaction = connection.transaction().unwrap();
    for i in 0..4000 {
        let id = format!("measurement-{i:08}");
        let record=ExecutionRecord {
            task_id:id.clone(),assignment_id:id.clone(),generation:1,class:AllocationClass::Guaranteed,
            phase:ExecutionPhase::Released,resources:Resources{cpu_millicores:1000,ram_mib:2048,gpu_memory_mib:BTreeMap::new()},
            identity:None,backend:"rootless".into(),evidence:vec![],
            detail:"Synthetic released history for read/serialization cost only; no user process was launched".into(),
        };
        transaction
            .execute(
                "INSERT INTO tasks VALUES (?1,1,'completed',1,?1,'synthetic-receipt')",
                [&id],
            )
            .unwrap();
        transaction
            .execute("INSERT INTO assignments VALUES (?1,?1,1)", [&id])
            .unwrap();
        transaction
            .execute(
                "INSERT INTO executions VALUES (?1,'{}',?2)",
                params![id, serde_json::to_string(&record).unwrap()],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    fs::create_dir(temporary.path().join("attempts")).unwrap();
    let cpu_started = usage().0;
    let started = Instant::now();
    let mut rounds = vec![];
    let mut wire_bytes = 0;
    let revised = std::env::var_os("RESMGR_BENCH_REVISED_RELEASE_FILTERS").is_some();
    for index in 0..16 {
        let round = Instant::now();
        let cpu = usage().0;
        // Match the existing loop's record decoding, release-family checks and
        // receipt probes. History without directories is a filesystem lower
        // bound; TLS, observations, GPU sampling and live supervisors are excluded.
        std::hint::black_box(store.executions().unwrap());
        let records = store.executions().unwrap();
        for record in &records {
            if !revised {
                std::hint::black_box(store.managed_children(&record.assignment_id).unwrap());
                std::hint::black_box(store.managed_children(&record.assignment_id).unwrap());
            }
            let output = temporary
                .path()
                .join("attempts")
                .join(&record.assignment_id);
            for name in [
                "result.receipt.json",
                "spool-reclaimed.json",
                "failure.receipt.json",
                "supervisor-spec.json",
            ] {
                std::hint::black_box(output.join(name).exists());
            }
        }
        if index % 4 == 0 {
            std::hint::black_box(store.executions().unwrap());
            let allocations = store
                .executions()
                .unwrap()
                .into_iter()
                .map(|r| AllocationReport {
                    assignment_id: r.assignment_id,
                    generation: r.generation,
                    phase: RemotePhase::Released,
                    observed: None,
                    detail: r.detail,
                })
                .collect();
            let report = NodeReport {
                node_id: "component-measurement".into(),
                launch_slots: 0,
                boot_id: "synthetic".into(),
                observed_at_unix_ms: 1,
                managed_budget: Resources::default(),
                expansion_allowed: false,
                gpu_expansion_allowed: false,
                gpu_guaranteed_allowed: false,
                available_controls: vec![],
                allocations,
            };
            wire_bytes = serde_json::to_vec(&Request::Heartbeat { report })
                .unwrap()
                .len();
        }
        rounds.push(serde_json::json!({"wall_ms":round.elapsed().as_secs_f64()*1000.,"cpu_seconds":usage().0-cpu}));
    }
    let cpu = usage().0 - cpu_started;
    let rss = usage().1;
    let rss_bytes = if cfg!(target_os = "macos") {
        rss
    } else {
        rss * 1024
    };
    println!(
        "{}",
        serde_json::json!({"schema_version":1,"records":4000,"rounds":rounds,"elapsed_seconds":started.elapsed().as_secs_f64(),
        "component_cpu_seconds":cpu,"component_mean_cores_at_500ms_poll":cpu/8.,"component_mean_cores_at_500ms_sleep_after_work":cpu/(8.+started.elapsed().as_secs_f64()),"process_peak_rss_bytes":rss_bytes,"revised_release_filters":revised,
        "heartbeat_request_json_bytes":wire_bytes,"scope":"decode/query/receipt-probe/heartbeat-serialization component only; excludes network, telemetry and supervisor CPU",
        "whole_manager_target_status":"not_evaluated_by_component_measurement"})
    );
}
