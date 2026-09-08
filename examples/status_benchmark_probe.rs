//! Native SQLite fixture preparation and query-plan evidence for status_benchmark.py.
//! This helper only operates on an explicitly supplied, isolated benchmark state.
use anyhow::{Context, Result, ensure};
use cedegrid::{
    config::{RuntimeConfigKind, load_runtime},
    coordinator::Coordinator,
    execution_model::{AllocationClass, LaunchRequest},
    namespace::Namespace,
    pagination::{Collection, PageQuery},
    protocol::{CoordinatorConfig, JobSpec, PoolSpec},
};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use serde_json::{Value, json};
use std::{ffi::CStr, path::Path, time::Duration};

fn main() -> Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    ensure!(
        args.len() >= 3,
        "usage: status_benchmark_probe seed|explain DEPLOYMENT [TASKS]"
    );
    let config: CoordinatorConfig =
        load_runtime(Path::new(&args[2]), RuntimeConfigKind::Coordinator)?;
    ensure!(
        config.listen.ip().is_loopback(),
        "benchmark must use a loopback listener"
    );
    let output = match args[1].as_str() {
        "seed" => {
            ensure!(args.len() == 4, "seed requires TASKS");
            let count = args[3].parse::<usize>()?;
            ensure!(
                (1..=1_000_000).contains(&count),
                "task count must be in 1..=1000000"
            );
            seed(config, count)?
        }
        "explain" => {
            ensure!(args.len() == 3, "explain accepts only DEPLOYMENT");
            explain(&config)?
        }
        _ => anyhow::bail!("unknown benchmark operation"),
    };
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

fn seed(config: CoordinatorConfig, count: usize) -> Result<Value> {
    let started = std::time::Instant::now();
    ensure!(
        !config.state_dir.exists(),
        "seed refuses an existing state directory"
    );
    drop(Coordinator::open(config.clone())?);
    let _guard = Namespace::new(&config.state_dir)?.acquire(None, Duration::from_secs(30))?;
    let mut db = Connection::open_with_flags(
        config.state_dir.join(cedegrid::state::DATABASE_FILENAME),
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    db.pragma_update(None, "foreign_keys", true)?;
    db.pragma_update(None, "synchronous", "FULL")?;
    let page_size: i64 = db.pragma_query_value(None, "page_size", |row| row.get(0))?;
    let cache_pages = 16 * 1024 * 1024 / page_size;
    ensure!(cache_pages > 0, "invalid SQLite page size");
    // SQLite stores this default in the owned database header. The native
    // coordinator does not override cache_size; explain() verifies a fresh open.
    db.pragma_update(None, "default_cache_size", cache_pages)?;
    let pool = PoolSpec {
        pool_id: "benchmark-pool".into(),
        class: AllocationClass::Guaranteed,
        node_ids: vec!["benchmark-unconnected-node".into()],
        min_workers: 0,
        max_workers: 1,
    };
    db.execute(
        "INSERT INTO pools(pool_id,spec_json) VALUES (?1,?2)",
        params![pool.pool_id, serde_json::to_string(&pool)?],
    )?;
    for begin in (0..count).step_by(1000) {
        let end = (begin + 1000).min(count);
        let mut tasks = Vec::with_capacity(end - begin);
        for index in begin..end {
            let task: LaunchRequest = serde_json::from_value(json!({
                "task_id": format!("benchmark-task-{index:09}"), "assignment_id": "",
                "argv": ["/bin/true"], "cwd": config.state_dir,
                "resources": {"cpu_millicores":100,"ram_mib":8,"gpu_memory_mib":{}},
                "class":"guaranteed", "replay_safe":true, "no_escape":true,
                "single_process":true, "allow_fallback":true
            }))?;
            cedegrid::protocol::validate_launch_request(&task)?;
            tasks.push(task);
        }
        let job = JobSpec {
            job_id: format!("benchmark-job-{:06}", begin / 1000),
            pool_id: pool.pool_id.clone(),
            priority: 0,
            tasks,
        };
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO jobs(job_id,pool_id,priority,cancelled,submitted_ms,spec_json) VALUES (?1,?2,0,1,0,?3)", params![job.job_id, job.pool_id, serde_json::to_string(&job)?])?;
        {
            let mut task_stmt =
                tx.prepare("INSERT INTO tasks(task_id,replay_safe,status) VALUES (?1,1,'queued')")?;
            let mut spec_stmt = tx
                .prepare("INSERT INTO task_specs(task_id,job_id,request_json) VALUES (?1,?2,?3)")?;
            for task in &job.tasks {
                task_stmt.execute([&task.task_id])?;
                spec_stmt.execute(params![
                    task.task_id,
                    job.job_id,
                    serde_json::to_string(task)?
                ])?;
            }
        }
        tx.commit()?;
    }
    db.execute_batch("ANALYZE; PRAGMA wal_checkpoint(TRUNCATE);")?;
    let (tasks, specs): (i64, i64) = db.query_row(
        "SELECT (SELECT count(*) FROM tasks),(SELECT count(*) FROM task_specs)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        tasks == count as i64 && specs == tasks,
        "seeded task counts differ"
    );
    Ok(
        json!({"tasks":tasks,"jobs":count.div_ceil(1000),"cache_pages":cache_pages,"page_size":page_size,"cache_bytes":cache_pages*page_size,"sqlite_version":rusqlite::version(),"seed_seconds":started.elapsed().as_secs_f64(),"workloads_started":false,"seed_mode":"offline native SQLite; valid cancelled jobs and unconnected pool"}),
    )
}

unsafe extern "C" fn trace_statement(
    event: u32,
    context: *mut std::ffi::c_void,
    statement: *mut std::ffi::c_void,
    _detail: *mut std::ffi::c_void,
) -> std::ffi::c_int {
    if event != rusqlite::ffi::SQLITE_TRACE_STMT {
        return 0;
    }
    // SAFETY: SQLite invokes this synchronously while the boxed vector remains
    // live; statement is SQLite's sqlite3_stmt pointer for SQLITE_TRACE_STMT.
    unsafe {
        let sql = rusqlite::ffi::sqlite3_expanded_sql(statement.cast());
        if !sql.is_null() {
            let value = CStr::from_ptr(sql).to_string_lossy().into_owned();
            rusqlite::ffi::sqlite3_free(sql.cast());
            if value.len() <= 1024 * 1024 {
                (*context.cast::<Vec<String>>()).push(value);
            }
        }
    }
    0
}

fn explain(config: &CoordinatorConfig) -> Result<Value> {
    let _guard = Namespace::new(&config.state_dir)?.acquire(None, Duration::from_secs(30))?;
    let db = Connection::open_with_flags(
        config.state_dir.join(cedegrid::state::DATABASE_FILENAME),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let last_job: String = db.query_row("SELECT MAX(job_id) FROM jobs", [], |row| row.get(0))?;
    let mut reports = Vec::new();
    for job_id in [None, Some(last_job)] {
        let query = PageQuery {
            collection: Collection::Tasks,
            job_id,
            node_id: None,
            pool_id: None,
            limit: 100,
            cursor: None,
        };
        let mut statements = Box::<Vec<String>>::default();
        // SAFETY: the context address is stable and retained through the only
        // read call below; disable the callback before inspecting or freeing it.
        unsafe {
            ensure!(
                rusqlite::ffi::sqlite3_trace_v2(
                    db.handle(),
                    rusqlite::ffi::SQLITE_TRACE_STMT,
                    Some(trace_statement),
                    (&mut *statements as *mut Vec<String>).cast()
                ) == 0,
                "cannot enable SQLite trace"
            );
        }
        let page_result = cedegrid::pagination::read(&db, &query, 1, 1);
        unsafe {
            rusqlite::ffi::sqlite3_trace_v2(db.handle(), 0, None, std::ptr::null_mut());
        }
        let page = page_result?;
        ensure!(page.items.len() == 100, "first page must contain 100 tasks");
        let mut plans = Vec::new();
        for sql in statements.iter() {
            let mut stmt = db.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
            let plan = stmt.query_map([], |row| Ok(json!({"id":row.get::<_,i64>(0)?,"parent":row.get::<_,i64>(1)?,"detail":row.get::<_,String>(3)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
            plans.push(json!({"executed_sql":sql,"plan":plan}));
        }
        ensure!(
            plans.iter().any(|plan| plan["executed_sql"]
                .as_str()
                .is_some_and(|sql| sql.starts_with("SELECT s.sequence"))),
            "native pagination statement trace missing"
        );
        reports.push(json!({"query":query,"items":page.items.len(),"plans":plans}));
    }
    let cache: i64 = db.pragma_query_value(None, "cache_size", |row| row.get(0))?;
    let default_cache: i64 = db.pragma_query_value(None, "default_cache_size", |row| row.get(0))?;
    let page_size: i64 = db.pragma_query_value(None, "page_size", |row| row.get(0))?;
    let mmap: i64 = db.pragma_query_value(None, "mmap_size", |row| row.get(0))?;
    let cache_bytes = if cache < 0 {
        -cache * 1024
    } else {
        cache * page_size
    };
    ensure!(
        (1..=16 * 1024 * 1024).contains(&cache_bytes),
        "native SQLite cache exceeds 16 MiB"
    );
    let mut options = db.prepare("PRAGMA compile_options")?;
    let compile_options = options
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let task_count: i64 = db
        .query_row("SELECT count(*) FROM tasks", [], |row| row.get(0))
        .context("task count")?;
    Ok(
        json!({"sqlite_version":rusqlite::version(),"task_count":task_count,"cache_size":cache,"default_cache_size":default_cache,"page_size":page_size,"cache_bytes":cache_bytes,"mmap_size":mmap,"compile_options":compile_options,"queries":reports,"plan_source":"SQLite trace of actual cedegrid::pagination::read followed by native EXPLAIN QUERY PLAN"}),
    )
}
