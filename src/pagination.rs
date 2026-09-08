//! Finite, bounded live traversals. Cursors bind a high-water mark and coordinator epoch.
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const PAGE_BYTES: usize = 7 * 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Collection {
    Tasks,
    Jobs,
    Pools,
    Nodes,
    Allocations,
    PoolNodes,
    ReportedAllocations,
    UnrecognizedAllocations,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PageQuery {
    pub collection: Collection,
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub pool_id: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub cursor: Option<String>,
}
fn default_limit() -> u32 {
    100
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    pub items: Vec<Value>,
    pub next_cursor: Option<String>,
    pub coordinator_epoch: u64,
    pub observed_at_unix_ms: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u32,
    query: PageQuery,
    epoch: u64,
    high_water: i64,
    last: i64,
}

/// Called in the coordinator schema transaction, including during offline upgrades.
pub fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS status_sequences(sequence INTEGER PRIMARY KEY AUTOINCREMENT,collection TEXT NOT NULL,key1 TEXT NOT NULL,key2 TEXT NOT NULL DEFAULT '',UNIQUE(collection,key1,key2)) STRICT;
      CREATE INDEX IF NOT EXISTS status_sequence_scan ON status_sequences(collection,sequence);
      CREATE TABLE IF NOT EXISTS pool_members(pool_id TEXT NOT NULL,node_id TEXT NOT NULL,PRIMARY KEY(pool_id,node_id)) STRICT;
      CREATE INDEX IF NOT EXISTS pool_members_node ON pool_members(node_id,pool_id);
      CREATE TABLE IF NOT EXISTS reported_inventory(node_id TEXT NOT NULL,assignment_id TEXT NOT NULL,report_json TEXT NOT NULL,PRIMARY KEY(node_id,assignment_id)) STRICT;
      CREATE INDEX IF NOT EXISTS task_specs_job_sequence ON task_specs(job_id,sequence);
      CREATE INDEX IF NOT EXISTS reservations_node_assignment ON reservations(node_id,assignment_id);
      CREATE INDEX IF NOT EXISTS assignments_task_assignment ON assignments(task_id,assignment_id);
      CREATE TABLE IF NOT EXISTS upgrade_task_holds(task_id TEXT PRIMARY KEY REFERENCES tasks(task_id)) STRICT;")?;
    for (table, collection, key1, key2) in [
        ("jobs", "jobs", "job_id", None),
        ("pools", "pools", "pool_id", None),
        ("nodes", "nodes", "node_id", None),
        ("reservations", "allocations", "assignment_id", None),
        ("pool_members", "pool_nodes", "pool_id", Some("node_id")),
        (
            "reported_inventory",
            "reported_allocations",
            "node_id",
            Some("assignment_id"),
        ),
        (
            "unrecognized_allocations",
            "unrecognized_allocations",
            "node_id",
            Some("assignment_id"),
        ),
    ] {
        let new2 = key2.map(|k| format!("NEW.{k}")).unwrap_or("''".into());
        let old2 = key2.map(|k| format!("OLD.{k}")).unwrap_or("''".into());
        let select2 = key2.unwrap_or("''");
        db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS status_{table}_insert AFTER INSERT ON {table} BEGIN INSERT INTO status_sequences(collection,key1,key2) VALUES ('{collection}',NEW.{key1},{new2}); END;
            CREATE TRIGGER IF NOT EXISTS status_{table}_delete AFTER DELETE ON {table} BEGIN DELETE FROM status_sequences WHERE collection='{collection}' AND key1=OLD.{key1} AND key2={old2}; END;
            INSERT OR IGNORE INTO status_sequences(collection,key1,key2) SELECT '{collection}',{key1},{select2} FROM {table};"))?;
    }
    for (event, table, body) in [
        (
            "INSERT",
            "pools",
            "INSERT INTO pool_members(pool_id,node_id) SELECT NEW.pool_id,value FROM json_each(NEW.spec_json,'$.node_ids') GROUP BY value;",
        ),
        (
            "UPDATE",
            "pools",
            "DELETE FROM pool_members WHERE pool_id=NEW.pool_id AND node_id NOT IN (SELECT value FROM json_each(NEW.spec_json,'$.node_ids')); INSERT INTO pool_members(pool_id,node_id) SELECT NEW.pool_id,value FROM json_each(NEW.spec_json,'$.node_ids') j WHERE NOT EXISTS(SELECT 1 FROM pool_members m WHERE m.pool_id=NEW.pool_id AND m.node_id=j.value) GROUP BY value;",
        ),
        (
            "DELETE",
            "pools",
            "DELETE FROM pool_members WHERE pool_id=OLD.pool_id;",
        ),
        (
            "INSERT",
            "nodes",
            "INSERT INTO reported_inventory(node_id,assignment_id,report_json) SELECT NEW.node_id,json_extract(value,'$.assignment_id'),value FROM json_each(NEW.report_json,'$.allocations');",
        ),
        (
            "UPDATE",
            "nodes",
            "DELETE FROM reported_inventory WHERE node_id=NEW.node_id AND assignment_id NOT IN (SELECT json_extract(value,'$.assignment_id') FROM json_each(NEW.report_json,'$.allocations')); INSERT INTO reported_inventory(node_id,assignment_id,report_json) SELECT NEW.node_id,json_extract(value,'$.assignment_id'),value FROM json_each(NEW.report_json,'$.allocations') WHERE true ON CONFLICT(node_id,assignment_id) DO UPDATE SET report_json=excluded.report_json;",
        ),
        (
            "DELETE",
            "nodes",
            "DELETE FROM reported_inventory WHERE node_id=OLD.node_id;",
        ),
    ] {
        db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS status_members_{table}_{event} AFTER {event} ON {table} BEGIN {body} END;"))?;
    }
    db.execute_batch("INSERT OR IGNORE INTO pool_members SELECT p.pool_id,j.value FROM pools p,json_each(p.spec_json,'$.node_ids') j;
      INSERT OR IGNORE INTO reported_inventory SELECT n.node_id,json_extract(j.value,'$.assignment_id'),j.value FROM nodes n,json_each(n.report_json,'$.allocations') j;")?;
    Ok(())
}
fn name(collection: &Collection) -> &'static str {
    match collection {
        Collection::Tasks => "tasks",
        Collection::Jobs => "jobs",
        Collection::Pools => "pools",
        Collection::Nodes => "nodes",
        Collection::Allocations => "allocations",
        Collection::PoolNodes => "pool_nodes",
        Collection::ReportedAllocations => "reported_allocations",
        Collection::UnrecognizedAllocations => "unrecognized_allocations",
    }
}
pub fn read(db: &Connection, query: &PageQuery, epoch: u64, observed: u64) -> Result<Page> {
    ensure!(
        (1..=1000).contains(&query.limit),
        "ERR_CEDEGRID_ARGUMENT: page limit must be 1..1000"
    );
    for id in [&query.job_id, &query.node_id, &query.pool_id]
        .into_iter()
        .flatten()
    {
        ensure!(
            !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control),
            "ERR_CEDEGRID_ARGUMENT: invalid page filter"
        );
    }
    if query.collection == Collection::PoolNodes {
        ensure!(
            query.pool_id.is_some(),
            "ERR_CEDEGRID_ARGUMENT: pool_nodes requires pool_id"
        );
    }
    if matches!(
        query.collection,
        Collection::ReportedAllocations | Collection::UnrecognizedAllocations
    ) {
        ensure!(
            query.node_id.is_some(),
            "ERR_CEDEGRID_ARGUMENT: inventory requires node_id"
        );
    }
    if query.collection == Collection::UnrecognizedAllocations {
        ensure!(
            query.job_id.is_none(),
            "ERR_CEDEGRID_ARGUMENT: unrecognized ownership cannot be job scoped"
        );
    }
    let job_pool: Option<String> = if let Some(job) = &query.job_id {
        Some(
            db.query_row("SELECT pool_id FROM jobs WHERE job_id=?1", [job], |r| {
                r.get(0)
            })
            .optional()?
            .context("ERR_CEDEGRID_ARGUMENT: unknown job")?,
        )
    } else {
        None
    };
    if query.collection == Collection::PoolNodes && job_pool.is_some() {
        ensure!(
            job_pool == query.pool_id,
            "ERR_CEDEGRID_ARGUMENT: job does not reference pool"
        );
    }
    let mut bound_query = query.clone();
    bound_query.cursor = None;
    let (last, high_water) = if let Some(encoded) = &query.cursor {
        ensure!(
            encoded.len() <= 8192,
            "ERR_CEDEGRID_CURSOR: oversized cursor"
        );
        let decoded = hex::decode(encoded).context("ERR_CEDEGRID_CURSOR: invalid encoding")?;
        ensure!(
            decoded.len() <= 4096,
            "ERR_CEDEGRID_CURSOR: oversized cursor"
        );
        let cursor: Cursor =
            serde_json::from_slice(&decoded).context("ERR_CEDEGRID_CURSOR: invalid payload")?;
        ensure!(
            cursor.version == 1
                && cursor.query == bound_query
                && cursor.last > 0
                && cursor.last <= cursor.high_water,
            "ERR_CEDEGRID_CURSOR: filter or range mismatch"
        );
        ensure!(
            cursor.epoch == epoch,
            "ERR_CEDEGRID_CURSOR_EXPIRED: coordinator restarted"
        );
        (cursor.last, cursor.high_water)
    } else {
        let high = if query.collection == Collection::Tasks {
            db.query_row(
                "SELECT COALESCE(MAX(sequence),0) FROM task_specs",
                [],
                |r| r.get(0),
            )?
        } else {
            db.query_row(
                "SELECT COALESCE(MAX(sequence),0) FROM status_sequences WHERE collection=?1",
                [name(&query.collection)],
                |r| r.get(0),
            )?
        };
        (0, high)
    };
    let (from, sequence, key, value, mut filters) = match query.collection {
        Collection::Tasks => ("task_specs s JOIN tasks t USING(task_id)".to_owned(), "s.sequence", "t.task_id", "json_object('task_id',t.task_id,'job_id',s.job_id,'replay_safe',json(CASE t.replay_safe WHEN 1 THEN 'true' ELSE 'false' END),'status',t.status,'generation',t.generation,'assignment_id',t.assignment_id,'receipt_hash',t.receipt_hash)", String::new()),
        Collection::Jobs => ("status_sequences s JOIN jobs j ON j.job_id=s.key1".into(), "s.sequence", "j.job_id", "json_object('job_id',j.job_id,'pool_id',j.pool_id,'priority',j.priority,'cancelled',json(CASE j.cancelled WHEN 1 THEN 'true' ELSE 'false' END),'submitted_ms',j.submitted_ms,'task_count',(SELECT count(*) FROM task_specs t WHERE t.job_id=j.job_id))", " AND s.collection='jobs'".into()),
        Collection::Pools => ("status_sequences s JOIN pools p ON p.pool_id=s.key1".into(), "s.sequence", "p.pool_id", "json_set(json_remove(p.spec_json,'$.node_ids'),'$.node_count',(SELECT count(*) FROM pool_members m WHERE m.pool_id=p.pool_id))", " AND s.collection='pools'".into()),
        Collection::Nodes => ("status_sequences s JOIN nodes n ON n.node_id=s.key1".into(), "s.sequence", "n.node_id", "json_object('node_id',n.node_id,'report',json_remove(n.report_json,'$.allocations'),'received_ms',n.received_ms,'drain',json(CASE n.drain WHEN 1 THEN 'true' ELSE 'false' END),'reservation_count',(SELECT count(*) FROM reservations r JOIN assignments a USING(assignment_id) JOIN task_specs t USING(task_id) WHERE r.node_id=n.node_id AND (?3 IS NULL OR t.job_id=?3)),'reported_allocation_count',(SELECT count(*) FROM reported_inventory i LEFT JOIN assignments a ON a.assignment_id=i.assignment_id LEFT JOIN task_specs t USING(task_id) WHERE i.node_id=n.node_id AND (?3 IS NULL OR t.job_id=?3)),'unrecognized_allocation_count',CASE WHEN ?3 IS NULL THEN (SELECT count(*) FROM unrecognized_allocations u WHERE u.node_id=n.node_id) ELSE NULL END,'capacity_scope','node_global')", " AND s.collection='nodes'".into()),
        Collection::Allocations => ("status_sequences s JOIN reservations r ON r.assignment_id=s.key1 JOIN assignments a USING(assignment_id) JOIN task_specs t USING(task_id)".into(), "s.sequence", "r.assignment_id", "json_object('assignment_id',r.assignment_id,'task_id',a.task_id,'job_id',t.job_id,'node_id',r.node_id,'generation',a.generation,'phase',r.phase,'resources',json(r.resources_json),'lease_sequence',r.lease_sequence,'lease_deadline_ms',r.lease_deadline_ms,'epoch',r.epoch,'detail',r.detail)", " AND s.collection='allocations'".into()),
        Collection::PoolNodes => ("status_sequences s JOIN pool_members m ON m.pool_id=s.key1 AND m.node_id=s.key2".into(), "s.sequence", "m.node_id", "json_object('pool_id',m.pool_id,'node_id',m.node_id)", " AND s.collection='pool_nodes' AND m.pool_id=?5".into()),
        Collection::ReportedAllocations => ("status_sequences s JOIN reported_inventory i ON i.node_id=s.key1 AND i.assignment_id=s.key2 LEFT JOIN assignments a ON a.assignment_id=i.assignment_id LEFT JOIN task_specs t USING(task_id)".into(), "s.sequence", "i.assignment_id", "i.report_json", " AND s.collection='reported_allocations' AND i.node_id=?4".into()),
        Collection::UnrecognizedAllocations => ("status_sequences s JOIN unrecognized_allocations u ON u.node_id=s.key1 AND u.assignment_id=s.key2".into(), "s.sequence", "u.assignment_id", "u.report_json", " AND s.collection='unrecognized_allocations' AND u.node_id=?4".into()),
    };
    if query.job_id.is_some() {
        filters.push_str(match query.collection {
        Collection::Tasks => " AND s.job_id=?3", Collection::Jobs => " AND j.job_id=?3", Collection::Pools => " AND p.pool_id=(SELECT pool_id FROM jobs WHERE job_id=?3)",
        Collection::Nodes => " AND (EXISTS(SELECT 1 FROM pool_members m JOIN jobs j USING(pool_id) WHERE j.job_id=?3 AND m.node_id=n.node_id) OR EXISTS(SELECT 1 FROM reservations r JOIN assignments a USING(assignment_id) JOIN task_specs t USING(task_id) WHERE r.node_id=n.node_id AND t.job_id=?3))",
        Collection::Allocations | Collection::ReportedAllocations => " AND t.job_id=?3", _ => ""
    });
    }
    if query.node_id.is_some() {
        filters.push_str(match query.collection { Collection::Nodes => " AND n.node_id=?4", Collection::Allocations => " AND r.node_id=?4", Collection::PoolNodes => " AND m.node_id=?4", Collection::Tasks => " AND EXISTS(SELECT 1 FROM reservations r JOIN assignments a USING(assignment_id) WHERE a.task_id=t.task_id AND r.node_id=?4)", _ => "" });
    }
    if query.pool_id.is_some() {
        filters.push_str(match query.collection { Collection::Pools => " AND p.pool_id=?5", Collection::Jobs => " AND j.pool_id=?5", Collection::Tasks => " AND s.job_id IN (SELECT job_id FROM jobs WHERE pool_id=?5)", Collection::Nodes => " AND EXISTS(SELECT 1 FROM pool_members m WHERE m.node_id=n.node_id AND m.pool_id=?5)", Collection::Allocations => " AND t.job_id IN (SELECT job_id FROM jobs WHERE pool_id=?5)", _ => "" });
    }
    let sql = format!(
        "SELECT {sequence},{key},{value} FROM {from} WHERE {sequence}>?1 AND {sequence}<=?2 {filters} AND (?3 IS NULL OR ?3 IS NOT NULL) AND (?4 IS NULL OR ?4 IS NOT NULL) AND (?5 IS NULL OR ?5 IS NOT NULL) ORDER BY {sequence} LIMIT ?6"
    );
    let mut statement = db.prepare(&sql)?;
    let mut rows = statement.query(params![
        last,
        high_water,
        query.job_id,
        query.node_id,
        query.pool_id,
        query.limit + 1
    ])?;
    let mut page = Page {
        items: vec![],
        next_cursor: None,
        coordinator_epoch: epoch,
        observed_at_unix_ms: observed,
    };
    let mut last_emitted = last;
    let mut bytes = 0usize;
    while let Some(row) = rows.next()? {
        let seq: i64 = row.get(0)?;
        let key: String = row.get(1)?;
        let raw: String = row.get(2)?;
        if raw.len() + 17000 > PAGE_BYTES {
            bail!(
                "ERR_CEDEGRID_ITEM_TOO_LARGE: {} {key}",
                name(&query.collection)
            );
        }
        if page.items.len() == query.limit as usize || bytes + raw.len() + 17000 > PAGE_BYTES {
            let cursor = Cursor {
                version: 1,
                query: bound_query,
                epoch,
                high_water,
                last: last_emitted,
            };
            page.next_cursor = Some(hex::encode(serde_json::to_vec(&cursor)?));
            break;
        }
        let item: Value = crate::numeric::from_slice(raw.as_bytes())?;
        bytes += raw.len() + 1;
        page.items.push(item);
        last_emitted = seq;
    }
    ensure!(serde_json::to_vec(&json!({"kind":"status_page","items":page.items,"next_cursor":page.next_cursor,"coordinator_epoch":epoch,"observed_at_unix_ms":observed}))?.len() <= PAGE_BYTES, "ERR_CEDEGRID_ITEM_TOO_LARGE: response envelope");
    Ok(page)
}
