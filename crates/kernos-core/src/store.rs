//! The SQLite store: schema, migrations and row-level queries.
//!
//! Every function takes a connection (or a transaction, which derefs to one) so
//! the kernel can group an operation's writes into one transaction. The
//! materialised `runs` and `steps` tables are written from the folded
//! [`RunState`] and must equal `fold` at every `seq`.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

use crate::error::{KernelError, KernelResult};
use crate::events::{Actor, Event, EventKind};
use crate::fold::{RunState, RunStatus, StepState, StepStatus};
use crate::remit::RemitPayload;
use crate::time::parse_rfc3339;
use kernos_policy::Approver;

/// The schema version this build writes.
pub const SCHEMA_VERSION: i64 = 2;

/// Opens (creating if needed) the database in WAL mode and applies migrations.
pub fn open(path: &Path) -> KernelResult<Connection> {
    let conn = Connection::open(path)?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// An in-memory database for tests.
pub fn open_in_memory() -> KernelResult<Connection> {
    let conn = Connection::open_in_memory()?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> KernelResult<()> {
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

const MIGRATIONS: &[&str] = &[
    // Version 1: the whole schema.
    "
    CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS events (
        run_id TEXT NOT NULL, seq INTEGER NOT NULL, ts TEXT NOT NULL, kind TEXT NOT NULL,
        actor TEXT NOT NULL, payload TEXT NOT NULL, prev_hash TEXT NOT NULL, hash TEXT NOT NULL,
        PRIMARY KEY (run_id, seq));
    CREATE INDEX IF NOT EXISTS events_run_kind ON events(run_id, kind);
    CREATE TABLE IF NOT EXISTS runs (
        run_id TEXT PRIMARY KEY, state TEXT NOT NULL, department TEXT, bundle_id TEXT NOT NULL,
        bundle_name TEXT NOT NULL, bundle_version TEXT NOT NULL, workflow TEXT NOT NULL,
        remit_id TEXT NOT NULL, child_remit_id TEXT NOT NULL, requested_by TEXT NOT NULL,
        needs_human INTEGER NOT NULL DEFAULT 0, park_reason TEXT, last_seq INTEGER NOT NULL,
        last_hash TEXT NOT NULL, state_json TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
    CREATE INDEX IF NOT EXISTS runs_state ON runs(state, created_at);
    CREATE TABLE IF NOT EXISTS steps (
        run_id TEXT NOT NULL, step_id TEXT NOT NULL, idx INTEGER NOT NULL, kind TEXT NOT NULL,
        state TEXT NOT NULL, attempts INTEGER NOT NULL, lease_id TEXT, not_before_ms INTEGER,
        def TEXT NOT NULL, PRIMARY KEY (run_id, step_id));
    CREATE INDEX IF NOT EXISTS steps_state ON steps(state, kind);
    CREATE TABLE IF NOT EXISTS leases (
        lease_id TEXT PRIMARY KEY, run_id TEXT NOT NULL, step_id TEXT NOT NULL, worker_id TEXT NOT NULL,
        attempt INTEGER NOT NULL, ttl_seconds INTEGER NOT NULL, issued_at_ms INTEGER NOT NULL,
        expires_at_ms INTEGER NOT NULL, state TEXT NOT NULL, ended_at_ms INTEGER);
    CREATE INDEX IF NOT EXISTS leases_state ON leases(state, expires_at_ms);
    CREATE TABLE IF NOT EXISTS remits (
        remit_id TEXT PRIMARY KEY, parent_id TEXT, run_id TEXT, bound_run_id TEXT, payload TEXT NOT NULL,
        token TEXT NOT NULL, expires_at_ms INTEGER NOT NULL, created_at TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS approvals (
        approval_id TEXT PRIMARY KEY, run_id TEXT NOT NULL, action_id TEXT NOT NULL, step_id TEXT NOT NULL,
        action TEXT NOT NULL, action_hash TEXT NOT NULL, idempotency_key TEXT, approver TEXT NOT NULL,
        original_approver TEXT NOT NULL, escalate_to TEXT NOT NULL, sla_seconds INTEGER NOT NULL,
        state TEXT NOT NULL, requested_at TEXT NOT NULL, due_at_ms INTEGER NOT NULL,
        escalations INTEGER NOT NULL DEFAULT 0, parked_human INTEGER NOT NULL DEFAULT 0,
        decided_at TEXT, actor TEXT, reason TEXT, decision TEXT);
    CREATE INDEX IF NOT EXISTS approvals_state ON approvals(state, due_at_ms);
    CREATE TABLE IF NOT EXISTS bundles (
        bundle_id TEXT PRIMARY KEY, name TEXT NOT NULL, version TEXT NOT NULL, department TEXT,
        sha256 TEXT NOT NULL, bundle TEXT NOT NULL, signature TEXT NOT NULL, created_at TEXT NOT NULL,
        UNIQUE (name, version));
    CREATE TABLE IF NOT EXISTS policies (
        policy_id TEXT PRIMARY KEY, name TEXT NOT NULL, version INTEGER NOT NULL, source TEXT NOT NULL,
        created_at TEXT NOT NULL, UNIQUE (name, version));
    CREATE TABLE IF NOT EXISTS actions (
        action_id TEXT PRIMARY KEY, run_id TEXT NOT NULL, step_id TEXT NOT NULL, seq INTEGER NOT NULL,
        action TEXT NOT NULL, action_hash TEXT NOT NULL, idempotency_key TEXT, context TEXT NOT NULL,
        decision TEXT NOT NULL, rule TEXT NOT NULL, approval_id TEXT, created_at TEXT NOT NULL);
    CREATE INDEX IF NOT EXISTS actions_created ON actions(created_at);
    ",
    // Version 2: `abandoning` marks a run whose compensations are still
    // unwinding. Such a run stays `running` so its compensation steps are
    // leasable, and the scheduler needs the flag to tell the two apart.
    "ALTER TABLE runs ADD COLUMN abandoning INTEGER NOT NULL DEFAULT 0;",
];

/// Applies every migration newer than the recorded schema version.
pub fn migrate(conn: &Connection) -> KernelResult<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);")?;
    let current: Option<i64> = conn
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .optional()?
        .flatten();
    let current = current.unwrap_or(0);
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let version = i as i64 + 1;
        if version > current {
            conn.execute_batch(sql)?;
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                params![version],
            )?;
        }
    }
    Ok(())
}

/// The recorded schema version.
pub fn schema_version(conn: &Connection) -> KernelResult<i64> {
    Ok(conn
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| {
            r.get::<_, Option<i64>>(0)
        })?
        .unwrap_or(0))
}

fn json_col<T: serde::de::DeserializeOwned>(row: &Row<'_>, index: usize) -> rusqlite::Result<T> {
    let text: String = row.get(index)?;
    serde_json::from_str(&text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn opt_json_col(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<Value>> {
    let text: Option<String> = row.get(index)?;
    match text {
        None => Ok(None),
        Some(t) => serde_json::from_str(&t).map(Some).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        }),
    }
}

// ---------------------------------------------------------------- events

/// Appends one already-hashed event.
pub fn insert_event(conn: &Connection, event: &Event) -> KernelResult<()> {
    conn.execute(
        "INSERT INTO events (run_id, seq, ts, kind, actor, payload, prev_hash, hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            event.run_id,
            event.seq as i64,
            event.ts,
            event.kind.as_str(),
            serde_json::to_string(&event.actor)?,
            serde_json::to_string(&event.payload)?,
            event.prev_hash,
            event.hash
        ],
    )?;
    Ok(())
}

fn event_from_row(row: &Row<'_>) -> rusqlite::Result<Event> {
    let kind_text: String = row.get(3)?;
    let kind = EventKind::parse(&kind_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            format!("unknown kind {kind_text}").into(),
        )
    })?;
    let actor: Actor = json_col(row, 4)?;
    Ok(Event {
        schema: crate::events::SCHEMA.to_string(),
        run_id: row.get(0)?,
        seq: row.get::<_, i64>(1)? as u64,
        ts: row.get(2)?,
        kind,
        actor,
        payload: json_col(row, 5)?,
        prev_hash: row.get(6)?,
        hash: row.get(7)?,
    })
}

const EVENT_COLUMNS: &str = "run_id, seq, ts, kind, actor, payload, prev_hash, hash";

/// Events of a run from `from_seq`, at most `limit`.
pub fn events_from(
    conn: &Connection,
    run_id: &str,
    from_seq: u64,
    limit: u64,
) -> KernelResult<Vec<Event>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {EVENT_COLUMNS} FROM events WHERE run_id = ?1 AND seq >= ?2 ORDER BY seq LIMIT ?3"
    ))?;
    let rows = stmt.query_map(
        params![run_id, from_seq as i64, limit as i64],
        event_from_row,
    )?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Every event of a run.
pub fn all_events(conn: &Connection, run_id: &str) -> KernelResult<Vec<Event>> {
    events_from(conn, run_id, 1, i64::MAX as u64)
}

/// Events of a run for one step, restricted to the given kinds.
pub fn step_events(
    conn: &Connection,
    run_id: &str,
    step: &str,
    kinds: &[EventKind],
) -> KernelResult<Vec<Event>> {
    let all = all_events(conn, run_id)?;
    Ok(all
        .into_iter()
        .filter(|e| kinds.contains(&e.kind) && e.step() == Some(step))
        .collect())
}

// ---------------------------------------------------------------- runs

/// Everything the `runs` row carries beyond the folded state.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    /// The folded state as materialised.
    pub state: RunState,
    /// The run-bound child remit handed to workers.
    pub child_remit_id: String,
    /// Creation timestamp.
    pub created_at: String,
    /// Last update timestamp.
    pub updated_at: String,
    /// Hash of the last event, the `prev_hash` of the next.
    pub last_hash: String,
}

/// Writes (inserts or replaces) the materialised run row from a folded state.
pub fn write_run(conn: &Connection, row: &RunRow) -> KernelResult<()> {
    let s = &row.state;
    conn.execute(
        "INSERT INTO runs (run_id, state, department, bundle_id, bundle_name, bundle_version, workflow, remit_id,
            child_remit_id, requested_by, needs_human, park_reason, last_seq, last_hash, state_json, created_at, updated_at,
            abandoning)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
         ON CONFLICT(run_id) DO UPDATE SET state = excluded.state, needs_human = excluded.needs_human,
            park_reason = excluded.park_reason, last_seq = excluded.last_seq, last_hash = excluded.last_hash,
            state_json = excluded.state_json, updated_at = excluded.updated_at, abandoning = excluded.abandoning",
        params![
            s.run_id,
            s.state.as_str(),
            s.department,
            s.bundle.id,
            s.bundle.name,
            s.bundle.version,
            s.workflow,
            s.remit_id,
            row.child_remit_id,
            serde_json::to_string(&s.requested_by)?,
            s.needs_human as i64,
            s.park_reason,
            s.last_seq as i64,
            row.last_hash,
            serde_json::to_string(s)?,
            row.created_at,
            row.updated_at,
            s.abandoning as i64
        ],
    )?;
    Ok(())
}

fn run_row_from_row(row: &Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        state: json_col(row, 0)?,
        child_remit_id: row.get(1)?,
        created_at: row.get(2)?,
        updated_at: row.get(3)?,
        last_hash: row.get(4)?,
    })
}

const RUN_COLUMNS: &str = "state_json, child_remit_id, created_at, updated_at, last_hash";

/// Loads a run row.
pub fn get_run(conn: &Connection, run_id: &str) -> KernelResult<Option<RunRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {RUN_COLUMNS} FROM runs WHERE run_id = ?1"),
            params![run_id],
            run_row_from_row,
        )
        .optional()?)
}

/// Loads a run row or fails with `404 run_not_found`.
pub fn require_run(conn: &Connection, run_id: &str) -> KernelResult<RunRow> {
    get_run(conn, run_id)?.ok_or_else(|| {
        KernelError::not_found("run_not_found", format!("run {run_id} does not exist"))
    })
}

/// Filters for listing runs.
#[derive(Debug, Clone, Default)]
pub struct RunFilter {
    /// Only this state.
    pub state: Option<String>,
    /// Only this department.
    pub department: Option<String>,
    /// Page size.
    pub limit: u64,
    /// Runs after this one in creation order.
    pub after: Option<String>,
}

/// Lists runs in creation order with keyset pagination.
pub fn list_runs(conn: &Connection, filter: &RunFilter) -> KernelResult<Vec<RunRow>> {
    let mut sql = format!("SELECT {RUN_COLUMNS} FROM runs WHERE 1 = 1");
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(state) = &filter.state {
        values.push(Box::new(state.clone()));
        sql.push_str(&format!(" AND state = ?{}", values.len()));
    }
    if let Some(department) = &filter.department {
        values.push(Box::new(department.clone()));
        sql.push_str(&format!(" AND department = ?{}", values.len()));
    }
    if let Some(after) = &filter.after {
        if let Some(anchor) = get_run(conn, after)? {
            values.push(Box::new(anchor.created_at));
            let created_idx = values.len();
            values.push(Box::new(after.clone()));
            sql.push_str(&format!(
                " AND (created_at > ?{created_idx} OR (created_at = ?{created_idx} AND run_id > ?{}))",
                values.len()
            ));
        }
    }
    values.push(Box::new(filter.limit.max(1) as i64));
    sql.push_str(&format!(
        " ORDER BY created_at, run_id LIMIT ?{}",
        values.len()
    ));
    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(|v| v.as_ref()).collect();
    let rows = stmt.query_map(refs.as_slice(), run_row_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Runs counted by state.
pub fn runs_by_state(conn: &Connection) -> KernelResult<BTreeMap<String, u64>> {
    let mut stmt = conn.prepare_cached("SELECT state, COUNT(*) FROM runs GROUP BY state")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
    })?;
    Ok(rows.collect::<Result<BTreeMap<_, _>, _>>()?)
}

// ---------------------------------------------------------------- steps

/// A materialised step row.
#[derive(Debug, Clone, PartialEq)]
pub struct StepRow {
    /// Step id.
    pub step_id: String,
    /// Position.
    pub idx: u32,
    /// Kind.
    pub kind: String,
    /// State.
    pub state: StepStatus,
    /// Attempts.
    pub attempts: u32,
    /// Live lease id.
    pub lease_id: Option<String>,
    /// Earliest re-lease time.
    pub not_before_ms: Option<i64>,
    /// The step definition (bundle step object, or the constructed compensation).
    pub def: Value,
}

/// Writes a step's state columns; inserts the row with its definition when new.
/// Returns an error when a new step has no definition to store.
pub fn write_step(
    conn: &Connection,
    run_id: &str,
    step: &StepState,
    def: Option<&Value>,
) -> KernelResult<()> {
    let not_before_ms = step.not_before.as_deref().and_then(parse_rfc3339);
    let lease_id = step.lease.as_ref().map(|l| l.lease_id.clone());
    let updated = conn.execute(
        "UPDATE steps SET idx = ?3, kind = ?4, state = ?5, attempts = ?6, lease_id = ?7, not_before_ms = ?8
         WHERE run_id = ?1 AND step_id = ?2",
        params![
            run_id,
            step.id,
            step.index as i64,
            step.kind,
            step.state.as_str(),
            step.attempts as i64,
            lease_id,
            not_before_ms
        ],
    )?;
    if updated == 0 {
        let def = def.ok_or_else(|| {
            KernelError::api(
                500,
                "internal",
                format!("step {} of run {run_id} has no definition", step.id),
            )
        })?;
        conn.execute(
            "INSERT INTO steps (run_id, step_id, idx, kind, state, attempts, lease_id, not_before_ms, def)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                run_id,
                step.id,
                step.index as i64,
                step.kind,
                step.state.as_str(),
                step.attempts as i64,
                lease_id,
                not_before_ms,
                serde_json::to_string(def)?
            ],
        )?;
    }
    Ok(())
}

fn step_row_from_row(row: &Row<'_>) -> rusqlite::Result<StepRow> {
    let state_text: String = row.get(3)?;
    let state = match state_text.as_str() {
        "scheduled" => StepStatus::Scheduled,
        "leased" => StepStatus::Leased,
        "completed" => StepStatus::Completed,
        "failed" => StepStatus::Failed,
        "quarantined" => StepStatus::Quarantined,
        "waiting_approval" => StepStatus::WaitingApproval,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                format!("unknown step state {other}").into(),
            ))
        }
    };
    Ok(StepRow {
        step_id: row.get(0)?,
        idx: row.get::<_, i64>(1)? as u32,
        kind: row.get(2)?,
        state,
        attempts: row.get::<_, i64>(4)? as u32,
        lease_id: row.get(5)?,
        not_before_ms: row.get(6)?,
        def: json_col(row, 7)?,
    })
}

const STEP_COLUMNS: &str = "step_id, idx, kind, state, attempts, lease_id, not_before_ms, def";

/// The materialised steps of a run in order.
pub fn steps_of_run(conn: &Connection, run_id: &str) -> KernelResult<Vec<StepRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {STEP_COLUMNS} FROM steps WHERE run_id = ?1 ORDER BY idx"
    ))?;
    let rows = stmt.query_map(params![run_id], step_row_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// One step row.
pub fn get_step(conn: &Connection, run_id: &str, step_id: &str) -> KernelResult<Option<StepRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {STEP_COLUMNS} FROM steps WHERE run_id = ?1 AND step_id = ?2"),
            params![run_id, step_id],
            step_row_from_row,
        )
        .optional()?)
}

/// The oldest runnable step whose kind is in `kinds`: a scheduled step whose
/// `not_before` has passed, in a running run with every earlier workflow step
/// completed, or a compensation step of an abandoned run with every earlier
/// compensation completed. Ordered by run creation, then step index.
pub fn pick_runnable(
    conn: &Connection,
    kinds: &[String],
    now_ms: i64,
) -> KernelResult<Option<(String, String)>> {
    if kinds.is_empty() {
        return Ok(None);
    }
    let placeholders: Vec<String> = (0..kinds.len()).map(|i| format!("?{}", i + 2)).collect();
    // Runs are taken in insertion order (`rowid`): two runs created in the same
    // millisecond have no meaningful order by `created_at`, and the random tail
    // of their ULIDs is not creation order, so ordering by id would make "the
    // oldest runnable step" a coin toss.
    //
    // A run being unwound stays `running`, so the `abandoning` flag is what
    // decides whether its workflow steps or its compensations are eligible:
    // while an unwind is under way the workflow is finished with, and only
    // compensations run, in their own order.
    let sql = format!(
        "SELECT s.run_id, s.step_id FROM steps s JOIN runs r ON r.run_id = s.run_id
         WHERE s.state = 'scheduled' AND (s.not_before_ms IS NULL OR s.not_before_ms <= ?1)
           AND s.kind IN ({})
           AND r.state = 'running'
           AND (
             (r.abandoning = 0 AND s.kind != 'compensation' AND NOT EXISTS (
                SELECT 1 FROM steps e WHERE e.run_id = s.run_id AND e.kind != 'compensation'
                  AND e.idx < s.idx AND e.state != 'completed'))
             OR
             (r.abandoning = 1 AND s.kind = 'compensation' AND NOT EXISTS (
                SELECT 1 FROM steps e WHERE e.run_id = s.run_id AND e.kind = 'compensation'
                  AND e.idx < s.idx AND e.state != 'completed'))
           )
         ORDER BY r.rowid, s.idx LIMIT 1",
        placeholders.join(", ")
    );
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(now_ms)];
    for kind in kinds {
        values.push(Box::new(kind.clone()));
    }
    let refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(|v| v.as_ref()).collect();
    Ok(conn
        .query_row(&sql, refs.as_slice(), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .optional()?)
}

// ---------------------------------------------------------------- leases

/// A lease row. `state` is `active`, `expired`, `released`, `completed` or `failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRow {
    /// Lease id.
    pub lease_id: String,
    /// Run.
    pub run_id: String,
    /// Step.
    pub step_id: String,
    /// Holder.
    pub worker_id: String,
    /// Attempt number.
    pub attempt: u32,
    /// TTL used for heartbeats.
    pub ttl_seconds: u64,
    /// Issue time.
    pub issued_at_ms: i64,
    /// Current expiry.
    pub expires_at_ms: i64,
    /// State.
    pub state: String,
}

/// Inserts a lease.
pub fn insert_lease(conn: &Connection, lease: &LeaseRow) -> KernelResult<()> {
    conn.execute(
        "INSERT INTO leases (lease_id, run_id, step_id, worker_id, attempt, ttl_seconds, issued_at_ms, expires_at_ms, state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            lease.lease_id,
            lease.run_id,
            lease.step_id,
            lease.worker_id,
            lease.attempt as i64,
            lease.ttl_seconds as i64,
            lease.issued_at_ms,
            lease.expires_at_ms,
            lease.state
        ],
    )?;
    Ok(())
}

fn lease_from_row(row: &Row<'_>) -> rusqlite::Result<LeaseRow> {
    Ok(LeaseRow {
        lease_id: row.get(0)?,
        run_id: row.get(1)?,
        step_id: row.get(2)?,
        worker_id: row.get(3)?,
        attempt: row.get::<_, i64>(4)? as u32,
        ttl_seconds: row.get::<_, i64>(5)? as u64,
        issued_at_ms: row.get(6)?,
        expires_at_ms: row.get(7)?,
        state: row.get(8)?,
    })
}

const LEASE_COLUMNS: &str = "lease_id, run_id, step_id, worker_id, attempt, ttl_seconds, issued_at_ms, expires_at_ms, state";

/// One lease.
pub fn get_lease(conn: &Connection, lease_id: &str) -> KernelResult<Option<LeaseRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {LEASE_COLUMNS} FROM leases WHERE lease_id = ?1"),
            params![lease_id],
            lease_from_row,
        )
        .optional()?)
}

/// Extends a lease's expiry.
pub fn set_lease_expiry(conn: &Connection, lease_id: &str, expires_at_ms: i64) -> KernelResult<()> {
    conn.execute(
        "UPDATE leases SET expires_at_ms = ?2 WHERE lease_id = ?1",
        params![lease_id, expires_at_ms],
    )?;
    Ok(())
}

/// Ends a lease with a final state.
pub fn end_lease(conn: &Connection, lease_id: &str, state: &str, now_ms: i64) -> KernelResult<()> {
    conn.execute(
        "UPDATE leases SET state = ?2, ended_at_ms = ?3 WHERE lease_id = ?1",
        params![lease_id, state, now_ms],
    )?;
    Ok(())
}

/// Active leases that have expired.
pub fn expired_leases(conn: &Connection, now_ms: i64) -> KernelResult<Vec<LeaseRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {LEASE_COLUMNS} FROM leases WHERE state = 'active' AND expires_at_ms <= ?1 ORDER BY expires_at_ms"
    ))?;
    let rows = stmt.query_map(params![now_ms], lease_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Active leases of a run.
pub fn active_leases_of_run(conn: &Connection, run_id: &str) -> KernelResult<Vec<LeaseRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {LEASE_COLUMNS} FROM leases WHERE state = 'active' AND run_id = ?1"
    ))?;
    let rows = stmt.query_map(params![run_id], lease_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

// ---------------------------------------------------------------- remits

/// A stored remit.
#[derive(Debug, Clone, PartialEq)]
pub struct RemitRow {
    /// Remit id.
    pub remit_id: String,
    /// Parent remit.
    pub parent_id: Option<String>,
    /// The run this remit is bound to (child remits).
    pub run_id: Option<String>,
    /// The run this (parent) remit was used to start.
    pub bound_run_id: Option<String>,
    /// The signed payload.
    pub payload: RemitPayload,
    /// The token.
    pub token: String,
    /// Expiry.
    pub expires_at_ms: i64,
    /// Creation timestamp.
    pub created_at: String,
}

/// Inserts a remit.
pub fn insert_remit(conn: &Connection, remit: &RemitRow) -> KernelResult<()> {
    conn.execute(
        "INSERT INTO remits (remit_id, parent_id, run_id, bound_run_id, payload, token, expires_at_ms, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            remit.remit_id,
            remit.parent_id,
            remit.run_id,
            remit.bound_run_id,
            serde_json::to_string(&remit.payload)?,
            remit.token,
            remit.expires_at_ms,
            remit.created_at
        ],
    )?;
    Ok(())
}

fn remit_from_row(row: &Row<'_>) -> rusqlite::Result<RemitRow> {
    Ok(RemitRow {
        remit_id: row.get(0)?,
        parent_id: row.get(1)?,
        run_id: row.get(2)?,
        bound_run_id: row.get(3)?,
        payload: json_col(row, 4)?,
        token: row.get(5)?,
        expires_at_ms: row.get(6)?,
        created_at: row.get(7)?,
    })
}

/// One remit.
pub fn get_remit(conn: &Connection, remit_id: &str) -> KernelResult<Option<RemitRow>> {
    Ok(conn
        .query_row(
            "SELECT remit_id, parent_id, run_id, bound_run_id, payload, token, expires_at_ms, created_at FROM remits WHERE remit_id = ?1",
            params![remit_id],
            remit_from_row,
        )
        .optional()?)
}

/// Marks a remit as bound to a run.
pub fn bind_remit(conn: &Connection, remit_id: &str, run_id: &str) -> KernelResult<()> {
    conn.execute(
        "UPDATE remits SET bound_run_id = ?2 WHERE remit_id = ?1",
        params![remit_id, run_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------- approvals

/// A stored approval.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRow {
    /// Approval id.
    pub approval_id: String,
    /// Run.
    pub run_id: String,
    /// Action.
    pub action_id: String,
    /// Step that proposed the action.
    pub step_id: String,
    /// The action object.
    pub action: Value,
    /// SHA-256 of the canonical action.
    pub action_hash: String,
    /// The action's idempotency key.
    pub idempotency_key: Option<String>,
    /// The current approver (after escalation).
    pub approver: Approver,
    /// The approver first requested.
    pub original_approver: Approver,
    /// Escalation target.
    pub escalate_to: Value,
    /// SLA.
    pub sla_seconds: u64,
    /// `pending`, `approved` or `rejected`.
    pub state: String,
    /// When requested.
    pub requested_at: String,
    /// Current deadline.
    pub due_at_ms: i64,
    /// Escalations so far.
    pub escalations: u32,
    /// The run was parked for a human after the second expiry.
    pub parked_human: bool,
    /// When decided.
    pub decided_at: Option<String>,
    /// Who decided.
    pub actor: Option<Value>,
    /// Why.
    pub reason: Option<String>,
    /// `approved` or `rejected`.
    pub decision: Option<String>,
}

/// Inserts an approval.
pub fn insert_approval(conn: &Connection, a: &ApprovalRow) -> KernelResult<()> {
    conn.execute(
        "INSERT INTO approvals (approval_id, run_id, action_id, step_id, action, action_hash, idempotency_key, approver,
            original_approver, escalate_to, sla_seconds, state, requested_at, due_at_ms, escalations, parked_human)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            a.approval_id,
            a.run_id,
            a.action_id,
            a.step_id,
            serde_json::to_string(&a.action)?,
            a.action_hash,
            a.idempotency_key,
            serde_json::to_string(&a.approver)?,
            serde_json::to_string(&a.original_approver)?,
            serde_json::to_string(&a.escalate_to)?,
            a.sla_seconds as i64,
            a.state,
            a.requested_at,
            a.due_at_ms,
            a.escalations as i64,
            a.parked_human as i64
        ],
    )?;
    Ok(())
}

fn approval_from_row(row: &Row<'_>) -> rusqlite::Result<ApprovalRow> {
    Ok(ApprovalRow {
        approval_id: row.get(0)?,
        run_id: row.get(1)?,
        action_id: row.get(2)?,
        step_id: row.get(3)?,
        action: json_col(row, 4)?,
        action_hash: row.get(5)?,
        idempotency_key: row.get(6)?,
        approver: json_col(row, 7)?,
        original_approver: json_col(row, 8)?,
        escalate_to: json_col(row, 9)?,
        sla_seconds: row.get::<_, i64>(10)? as u64,
        state: row.get(11)?,
        requested_at: row.get(12)?,
        due_at_ms: row.get(13)?,
        escalations: row.get::<_, i64>(14)? as u32,
        parked_human: row.get::<_, i64>(15)? != 0,
        decided_at: row.get(16)?,
        actor: opt_json_col(row, 17)?,
        reason: row.get(18)?,
        decision: row.get(19)?,
    })
}

const APPROVAL_COLUMNS: &str = "approval_id, run_id, action_id, step_id, action, action_hash, idempotency_key, approver,
    original_approver, escalate_to, sla_seconds, state, requested_at, due_at_ms, escalations, parked_human,
    decided_at, actor, reason, decision";

/// One approval.
pub fn get_approval(conn: &Connection, approval_id: &str) -> KernelResult<Option<ApprovalRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {APPROVAL_COLUMNS} FROM approvals WHERE approval_id = ?1"),
            params![approval_id],
            approval_from_row,
        )
        .optional()?)
}

/// Approvals, optionally filtered by state, ordered by request time.
pub fn list_approvals(conn: &Connection, state: Option<&str>) -> KernelResult<Vec<ApprovalRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {APPROVAL_COLUMNS} FROM approvals WHERE (?1 IS NULL OR state = ?1) ORDER BY requested_at, approval_id"
    ))?;
    let rows = stmt.query_map(params![state], approval_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Pending approvals past their deadline that have not yet parked for a human.
pub fn overdue_approvals(conn: &Connection, now_ms: i64) -> KernelResult<Vec<ApprovalRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {APPROVAL_COLUMNS} FROM approvals WHERE state = 'pending' AND parked_human = 0 AND due_at_ms <= ?1 ORDER BY due_at_ms"
    ))?;
    let rows = stmt.query_map(params![now_ms], approval_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Records an escalation: new approver, extended deadline, counter.
pub fn escalate_approval(
    conn: &Connection,
    approval_id: &str,
    approver: &Approver,
    due_at_ms: i64,
) -> KernelResult<()> {
    conn.execute(
        "UPDATE approvals SET approver = ?2, due_at_ms = ?3, escalations = escalations + 1 WHERE approval_id = ?1",
        params![approval_id, serde_json::to_string(approver)?, due_at_ms],
    )?;
    Ok(())
}

/// Marks an approval as parked for a human.
pub fn park_approval(conn: &Connection, approval_id: &str) -> KernelResult<()> {
    conn.execute(
        "UPDATE approvals SET parked_human = 1 WHERE approval_id = ?1",
        params![approval_id],
    )?;
    Ok(())
}

/// Records a decision.
pub fn decide_approval(
    conn: &Connection,
    approval_id: &str,
    decision: &str,
    actor: &Value,
    reason: &str,
    decided_at: &str,
) -> KernelResult<()> {
    conn.execute(
        "UPDATE approvals SET state = ?2, decision = ?2, actor = ?3, reason = ?4, decided_at = ?5 WHERE approval_id = ?1",
        params![approval_id, decision, serde_json::to_string(actor)?, reason, decided_at],
    )?;
    Ok(())
}

/// Approved approvals of a run.
pub fn approved_in_run(conn: &Connection, run_id: &str) -> KernelResult<Vec<ApprovalRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {APPROVAL_COLUMNS} FROM approvals WHERE run_id = ?1 AND state = 'approved' ORDER BY requested_at"
    ))?;
    let rows = stmt.query_map(params![run_id], approval_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Pending approvals of a run.
pub fn pending_in_run(conn: &Connection, run_id: &str) -> KernelResult<Vec<ApprovalRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {APPROVAL_COLUMNS} FROM approvals WHERE run_id = ?1 AND state = 'pending'"
    ))?;
    let rows = stmt.query_map(params![run_id], approval_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Number of pending approvals.
pub fn count_pending_approvals(conn: &Connection) -> KernelResult<u64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM approvals WHERE state = 'pending'",
        [],
        |r| r.get::<_, i64>(0),
    )? as u64)
}

// ---------------------------------------------------------------- bundles

/// A stored bundle.
#[derive(Debug, Clone, PartialEq)]
pub struct BundleRow {
    /// Bundle id.
    pub bundle_id: String,
    /// Name.
    pub name: String,
    /// Version.
    pub version: String,
    /// Department.
    pub department: Option<String>,
    /// SHA-256 of the canonical bundle.
    pub sha256: String,
    /// The bundle.
    pub bundle: Value,
    /// The signature that admitted it.
    pub signature: Value,
    /// Creation timestamp.
    pub created_at: String,
}

/// Inserts a bundle.
pub fn insert_bundle(conn: &Connection, b: &BundleRow) -> KernelResult<()> {
    conn.execute(
        "INSERT INTO bundles (bundle_id, name, version, department, sha256, bundle, signature, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            b.bundle_id,
            b.name,
            b.version,
            b.department,
            b.sha256,
            serde_json::to_string(&b.bundle)?,
            serde_json::to_string(&b.signature)?,
            b.created_at
        ],
    )?;
    Ok(())
}

fn bundle_from_row(row: &Row<'_>) -> rusqlite::Result<BundleRow> {
    Ok(BundleRow {
        bundle_id: row.get(0)?,
        name: row.get(1)?,
        version: row.get(2)?,
        department: row.get(3)?,
        sha256: row.get(4)?,
        bundle: json_col(row, 5)?,
        signature: json_col(row, 6)?,
        created_at: row.get(7)?,
    })
}

const BUNDLE_COLUMNS: &str =
    "bundle_id, name, version, department, sha256, bundle, signature, created_at";

/// One bundle by id.
pub fn get_bundle(conn: &Connection, bundle_id: &str) -> KernelResult<Option<BundleRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {BUNDLE_COLUMNS} FROM bundles WHERE bundle_id = ?1"),
            params![bundle_id],
            bundle_from_row,
        )
        .optional()?)
}

/// One bundle by name and version.
pub fn find_bundle(
    conn: &Connection,
    name: &str,
    version: &str,
) -> KernelResult<Option<BundleRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {BUNDLE_COLUMNS} FROM bundles WHERE name = ?1 AND version = ?2"),
            params![name, version],
            bundle_from_row,
        )
        .optional()?)
}

/// Every bundle in creation order.
pub fn list_bundles(conn: &Connection) -> KernelResult<Vec<BundleRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {BUNDLE_COLUMNS} FROM bundles ORDER BY created_at, bundle_id"
    ))?;
    let rows = stmt.query_map([], bundle_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

// ---------------------------------------------------------------- policies

/// A stored policy version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyRow {
    /// Policy id.
    pub policy_id: String,
    /// Name.
    pub name: String,
    /// Version.
    pub version: u64,
    /// Source text.
    pub source: String,
    /// Creation timestamp.
    pub created_at: String,
}

/// Inserts a policy version.
pub fn insert_policy(conn: &Connection, p: &PolicyRow) -> KernelResult<()> {
    conn.execute(
        "INSERT INTO policies (policy_id, name, version, source, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![p.policy_id, p.name, p.version as i64, p.source, p.created_at],
    )?;
    Ok(())
}

fn policy_from_row(row: &Row<'_>) -> rusqlite::Result<PolicyRow> {
    Ok(PolicyRow {
        policy_id: row.get(0)?,
        name: row.get(1)?,
        version: row.get::<_, i64>(2)? as u64,
        source: row.get(3)?,
        created_at: row.get(4)?,
    })
}

const POLICY_COLUMNS: &str = "policy_id, name, version, source, created_at";

/// One policy version.
pub fn get_policy(conn: &Connection, name: &str, version: u64) -> KernelResult<Option<PolicyRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {POLICY_COLUMNS} FROM policies WHERE name = ?1 AND version = ?2"),
            params![name, version as i64],
            policy_from_row,
        )
        .optional()?)
}

/// The newest version of a policy.
pub fn latest_policy(conn: &Connection, name: &str) -> KernelResult<Option<PolicyRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {POLICY_COLUMNS} FROM policies WHERE name = ?1 ORDER BY version DESC LIMIT 1"),
            params![name],
            policy_from_row,
        )
        .optional()?)
}

/// Every version of a policy, oldest first.
pub fn policy_versions(conn: &Connection, name: &str) -> KernelResult<Vec<PolicyRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {POLICY_COLUMNS} FROM policies WHERE name = ?1 ORDER BY version"
    ))?;
    let rows = stmt.query_map(params![name], policy_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Every policy version.
pub fn list_policies(conn: &Connection) -> KernelResult<Vec<PolicyRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {POLICY_COLUMNS} FROM policies ORDER BY name, version"
    ))?;
    let rows = stmt.query_map([], policy_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

// ---------------------------------------------------------------- actions

/// A stored action proposal with the context it was decided in, so a corpus can
/// be exported for policy testing.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionRow {
    /// Action id.
    pub action_id: String,
    /// Run.
    pub run_id: String,
    /// Step.
    pub step_id: String,
    /// Sequence of the `policy.decided` event.
    pub seq: u64,
    /// The action.
    pub action: Value,
    /// SHA-256 of the canonical action.
    pub action_hash: String,
    /// Idempotency key.
    pub idempotency_key: Option<String>,
    /// The `run` half of the policy context.
    pub context: Value,
    /// The decision.
    pub decision: String,
    /// The rule.
    pub rule: String,
    /// The approval, when one was requested.
    pub approval_id: Option<String>,
    /// Creation timestamp.
    pub created_at: String,
}

/// Inserts an action; a re-proposal of an approved action replaces the row.
pub fn upsert_action(conn: &Connection, a: &ActionRow) -> KernelResult<()> {
    conn.execute(
        "INSERT INTO actions (action_id, run_id, step_id, seq, action, action_hash, idempotency_key, context, decision, rule, approval_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(action_id) DO UPDATE SET seq = excluded.seq, decision = excluded.decision, rule = excluded.rule",
        params![
            a.action_id,
            a.run_id,
            a.step_id,
            a.seq as i64,
            serde_json::to_string(&a.action)?,
            a.action_hash,
            a.idempotency_key,
            serde_json::to_string(&a.context)?,
            a.decision,
            a.rule,
            a.approval_id,
            a.created_at
        ],
    )?;
    Ok(())
}

/// Actions decided at or after a timestamp, oldest first.
pub fn actions_since(conn: &Connection, since: &str) -> KernelResult<Vec<ActionRow>> {
    let mut stmt = conn.prepare(
        "SELECT action_id, run_id, step_id, seq, action, action_hash, idempotency_key, context, decision, rule, approval_id, created_at
         FROM actions WHERE created_at >= ?1 ORDER BY created_at, action_id",
    )?;
    let rows = stmt.query_map(params![since], |row| {
        Ok(ActionRow {
            action_id: row.get(0)?,
            run_id: row.get(1)?,
            step_id: row.get(2)?,
            seq: row.get::<_, i64>(3)? as u64,
            action: json_col(row, 4)?,
            action_hash: row.get(5)?,
            idempotency_key: row.get(6)?,
            context: json_col(row, 7)?,
            decision: row.get(8)?,
            rule: row.get(9)?,
            approval_id: row.get(10)?,
            created_at: row.get(11)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Whether a run's materialised state has the given status; a small helper for
/// tests and sweepers.
pub fn run_status(conn: &Connection, run_id: &str) -> KernelResult<Option<RunStatus>> {
    let text: Option<String> = conn
        .query_row(
            "SELECT state FROM runs WHERE run_id = ?1",
            params![run_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(text.and_then(|t| RunStatus::parse(&t)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_idempotent_and_versioned() {
        let conn = open_in_memory().expect("open");
        assert_eq!(schema_version(&conn).expect("version"), SCHEMA_VERSION);
        migrate(&conn).expect("again");
        assert_eq!(schema_version(&conn).expect("version"), SCHEMA_VERSION);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .expect("count");
        assert_eq!(
            count, SCHEMA_VERSION,
            "one row per applied migration, applied once"
        );
        // Version 2 added the flag the scheduler reads to tell a run that is
        // unwinding from one still working through its workflow.
        let abandoning: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('runs') WHERE name = 'abandoning'",
                [],
                |r| r.get(0),
            )
            .expect("column");
        assert_eq!(abandoning, 1);
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .expect("prepare")
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        for expected in [
            "events",
            "runs",
            "steps",
            "leases",
            "remits",
            "approvals",
            "bundles",
            "policies",
            "actions",
            "schema_version",
        ] {
            assert!(tables.contains(&expected.to_string()), "{expected}");
        }
    }

    #[test]
    fn wal_mode_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = open(&dir.path().join("kernos.db")).expect("open");
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .expect("pragma");
        assert_eq!(mode.to_lowercase(), "wal");
    }
}
