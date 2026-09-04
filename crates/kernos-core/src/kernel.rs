//! The kernel: every state-changing operation of 02-KERNEL-API, each performed
//! in one SQLite transaction that appends events and rewrites the materialised
//! rows from the folded state.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rand::Rng;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use kernos_policy::{
    evaluate, Approver, Decision, DecisionKind, Directory, EscalateTo, LoadedPolicy, ParseError,
};

use crate::bundle::{sign_bundle, verify_bundle_signature, Bundle, BundleError, BundleSignature};
use crate::canonical::{canonical_bytes, hash_value, sha256_hex};
use crate::clock::{Clock, SystemClock};
use crate::error::{KernelError, KernelResult};
use crate::events::{
    truncate_payload, Actor, ActorType, ApprovalDecided, ApprovalEscalated, ApprovalRequested,
    BudgetExceeded, BudgetSoftThreshold, BudgetSpec, CompensationCompleted, CompensationFailed,
    CompensationScheduled, DecisionActor, ErrorInfo, Event, EventKind, EventPayload, PolicyDecided,
    PolicyRef, RunAbandoned, RunCompleted, RunCreated, RunFailed, RunParked, RunResumed,
    StepCompleted, StepFailed, StepLeaseExpired, StepLeased, StepQuarantined, StepRetryScheduled,
    StepScheduled, StepWaitingApproval, UsageRecorded,
};
use crate::fold::{RunState, RunStatus, StepStatus};
use crate::ids::new_id;
use crate::keys::{KeyPair, PublicKey};
use crate::metrics::Metrics;
use crate::remit::{
    decode_token, encode_token, narrow, verify_token, Autonomy, DeriveRequest, IssueRequest,
    RemitPayload, TokenError, DEFAULT_TTL_SECONDS,
};
use crate::replay::{replay_run, ReplayReport};
use crate::schema::validate as validate_schema;
use crate::store::{
    self, ActionRow, ApprovalRow, BundleRow, LeaseRow, PolicyRow, RemitRow, RunFilter, RunRow,
};
use crate::template::resolve;
use crate::time::format_ms;

/// The engine version reported by `/v1/health`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Tunables of the kernel, all with the defaults of 02-KERNEL-API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelConfig {
    /// Lease TTL when a worker gives none.
    pub lease_ttl_default: u64,
    /// Lower clamp of a requested TTL.
    pub lease_ttl_min: u64,
    /// Upper clamp of a requested TTL.
    pub lease_ttl_max: u64,
    /// Soft budget threshold as a ratio of the ceiling.
    pub budget_soft_ratio: f64,
    /// Attempts before quarantine on non-deterministic failures.
    pub max_attempts_nondeterministic: u32,
    /// Attempts before quarantine on deterministic failures.
    pub max_attempts_deterministic: u32,
    /// Base of the retry backoff.
    pub retry_base_ms: u64,
    /// Cap of the retry backoff.
    pub retry_cap_ms: u64,
}

impl Default for KernelConfig {
    fn default() -> Self {
        KernelConfig {
            lease_ttl_default: 30,
            lease_ttl_min: 1,
            lease_ttl_max: 300,
            budget_soft_ratio: 0.8,
            max_attempts_nondeterministic: 5,
            max_attempts_deterministic: 3,
            retry_base_ms: 500,
            retry_cap_ms: 30_000,
        }
    }
}

/// What the trusted keys directory looked like when it was last read: one entry
/// per `*.pub` file, with its size and modification time. Comparing this is a
/// directory listing and a stat per file, which is cheap enough to do on every
/// signature check and is what makes trust immediate.
type TrustFingerprint = Vec<(String, u64, u64)>;

/// The trusted publisher keys, cached with the fingerprint they were read at.
#[derive(Debug, Default)]
struct TrustedKeys {
    /// Keys trusted in process by an embedder or a test. A directory scan never
    /// removes these, because no file backs them.
    pinned: Vec<PublicKey>,
    /// Keys read from `KERNOS_DATA/keys/trusted`.
    from_dir: Vec<PublicKey>,
    /// The fingerprint `from_dir` was read at.
    fingerprint: TrustFingerprint,
    /// False until the directory has been read once.
    loaded: bool,
}

/// The kernel. One instance owns the store; it is `Send + Sync` and shared
/// behind an `Arc` by the HTTP layer and the sweepers.
pub struct Kernel {
    conn: Mutex<Connection>,
    clock: Arc<dyn Clock>,
    key: KeyPair,
    /// Guarded separately from the store. A signature check may run inside a
    /// store transaction, so the order is always store then trusted, never the
    /// reverse, and neither is ever held while waiting for the other.
    trusted: Mutex<TrustedKeys>,
    trusted_dir: Option<PathBuf>,
    directory: Directory,
    config: KernelConfig,
    metrics: Metrics,
    data_dir: Option<PathBuf>,
    started_at_ms: i64,
}

impl std::fmt::Debug for Kernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kernel")
            .field("key_id", &self.key.key_id)
            .finish()
    }
}

fn internal(message: impl Into<String>) -> KernelError {
    KernelError::api(500, "internal", message)
}

fn invalid(message: impl Into<String>) -> KernelError {
    KernelError::unprocessable("invalid_request", message)
}

/// The `runs` block of the health report.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HealthRuns {
    /// Runs in `running`.
    pub running: u64,
    /// Runs in `parked`.
    pub parked: u64,
}

/// `GET /v1/health`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    /// Always true when the kernel answers.
    pub ok: bool,
    /// Engine version.
    pub version: String,
    /// Seconds since start.
    pub uptime_s: u64,
    /// Run counts.
    pub runs: HealthRuns,
}

/// Result of applying a bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleApplied {
    /// True when the bundle was new (201), false when identical content existed (200).
    #[serde(skip)]
    pub created: bool,
    /// Bundle id.
    pub bundle_id: String,
    /// Name.
    pub name: String,
    /// Version.
    pub version: String,
}

/// Result of applying a policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyApplied {
    /// True when new.
    #[serde(skip)]
    pub created: bool,
    /// Policy id.
    pub policy_id: String,
    /// Name.
    pub name: String,
    /// Version.
    pub version: u64,
}

/// One side of `POST /v1/policies/test`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicySelector {
    /// A stored version.
    Stored {
        /// Name.
        name: String,
        /// Version.
        version: u64,
    },
    /// Policy text not yet stored.
    Inline {
        /// The source.
        source: String,
    },
}

/// Result of a remit issue or derive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemitIssued {
    /// Remit id.
    pub remit_id: String,
    /// Parent, for derived remits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// The signed token.
    pub token: String,
    /// Expiry.
    pub expires_at: String,
}

/// Body of `POST /v1/runs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartRunRequest {
    /// Bundle id.
    pub bundle_id: String,
    /// Workflow name.
    pub workflow: String,
    /// Workflow input.
    #[serde(default)]
    pub input: Value,
    /// Remit id.
    pub remit_id: String,
    /// Requester; falls back to the remit's.
    #[serde(default)]
    pub requested_by: Option<Value>,
}

/// Response of `POST /v1/runs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStarted {
    /// Run id.
    pub run_id: String,
    /// Always `running`.
    pub state: RunStatus,
}

/// Body of `POST /v1/leases`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseRequest {
    /// Worker id.
    pub worker_id: String,
    /// Step kinds the worker can run; all four when omitted.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Requested TTL.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

/// A granted lease with its execution context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseGrant {
    /// Lease id.
    pub lease_id: String,
    /// Run.
    pub run_id: String,
    /// Step id.
    pub step: String,
    /// Attempt number.
    pub attempt: u32,
    /// Expiry.
    pub expires_at: String,
    /// Suggested heartbeat interval.
    pub heartbeat_seconds: u64,
    /// The step object from the bundle (or the constructed compensation step).
    pub step_def: Value,
    /// The context of 02-KERNEL-API.
    pub context: Value,
}

/// Usage reported at completion.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Usage {
    /// Tokens.
    #[serde(default)]
    pub tokens: u64,
    /// Currency.
    #[serde(default)]
    pub usd: f64,
}

/// Response of `POST /v1/leases/{id}/complete`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteResult {
    /// Run state after completion.
    pub run_state: RunStatus,
    /// The next step, if any.
    pub next_step: Option<String>,
}

/// Response of `POST /v1/leases/{id}/fail`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailResult {
    /// `retry_scheduled`, `quarantined` or `parked`.
    pub outcome: String,
    /// Delay before the retry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
}

/// Response of `POST /v1/leases/{id}/actions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionOutcome {
    /// Action id.
    pub action_id: String,
    /// The decision.
    pub decision: DecisionKind,
    /// The rule.
    pub rule: String,
    /// The approval, when required.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
}

/// Response of `POST /v1/approvals/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalOutcome {
    /// Run.
    pub run_id: String,
    /// Run state afterwards.
    pub run_state: RunStatus,
}

/// How an external event post authenticates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalAuth {
    /// `X-Kernos-Lease`.
    Lease(String),
    /// `X-Kernos-Remit`.
    Remit(String),
    /// Neither header.
    None,
}

/// Response of `POST /v1/runs/{id}/events`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Appended {
    /// Sequence number.
    pub seq: u64,
    /// Hash.
    pub hash: String,
}

/// Response of `GET /v1/runs/{id}/events`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventPage {
    /// Events.
    pub events: Vec<Event>,
    /// Next sequence to ask for, when more may exist.
    pub next_seq: Option<u64>,
}

/// Response of `GET /v1/runs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunPage {
    /// Runs.
    pub runs: Vec<RunState>,
    /// Cursor for the next page.
    pub next: Option<String>,
}

/// Builds the policy context of 04-POLICY for an action in a run.
pub fn policy_context(state: &RunState, remit: &RemitPayload, action: &Value) -> Value {
    let mut action = action.clone();
    if let Some(obj) = action.as_object_mut() {
        obj.entry("data_classes").or_insert_with(|| json!([]));
        obj.entry("paths").or_insert_with(|| json!([]));
    }
    json!({"action": action, "run": run_context(state, remit)})
}

/// The `run` half of the policy context.
pub fn run_context(state: &RunState, remit: &RemitPayload) -> Value {
    json!({
        "id": state.run_id,
        "department": state.department,
        "bundle": {"name": state.bundle.name, "version": state.bundle.version},
        "workflow": state.workflow,
        "remit": {"autonomy": remit.autonomy, "grants": remit.grants, "tools": remit.tools, "scopes": remit.scopes},
        "requested_by": state.requested_by,
    })
}

/// Loads the newest version of every named policy. Fails with
/// `422 policy_not_found` when one is missing.
pub fn load_policy_set(
    conn: &Connection,
    names: &[String],
) -> KernelResult<(Vec<LoadedPolicy>, Vec<PolicyRef>)> {
    let mut loaded = Vec::new();
    let mut refs = Vec::new();
    for name in names {
        let row = store::latest_policy(conn, name)?.ok_or_else(|| {
            KernelError::unprocessable("policy_not_found", format!("policy {name} is not loaded"))
                .with_details(json!({"policy": name}))
        })?;
        loaded.push(parse_policy_row(&row)?);
        refs.push(PolicyRef {
            name: row.name,
            version: row.version,
        });
    }
    Ok((loaded, refs))
}

/// Loads exact policy versions, for replay.
pub fn load_policy_refs(conn: &Connection, refs: &[PolicyRef]) -> KernelResult<Vec<LoadedPolicy>> {
    let mut loaded = Vec::new();
    for r in refs {
        let row = store::get_policy(conn, &r.name, r.version)?.ok_or_else(|| {
            KernelError::unprocessable(
                "policy_not_found",
                format!("policy {}@{} is not loaded", r.name, r.version),
            )
        })?;
        loaded.push(parse_policy_row(&row)?);
    }
    Ok(loaded)
}

fn parse_policy_row(row: &PolicyRow) -> KernelResult<LoadedPolicy> {
    kernos_policy::load(&row.name, row.version, &row.source).map_err(|e| {
        internal(format!(
            "stored policy {}@{} no longer parses: {e}",
            row.name, row.version
        ))
    })
}

fn policy_invalid(err: ParseError) -> KernelError {
    KernelError::unprocessable("policy_invalid", format!("policy does not parse: {err}"))
        .with_details(json!({
            "line": err.line,
            "column": err.column,
            "message": err.message,
        }))
}

/// A run loaded inside a transaction, with the helpers that append events and
/// keep the materialised rows equal to the fold.
struct RunTx<'a> {
    conn: &'a Connection,
    metrics: &'a Metrics,
    ts: String,
    run: RunRow,
    new_defs: BTreeMap<String, Value>,
}

impl<'a> RunTx<'a> {
    fn load(
        conn: &'a Connection,
        metrics: &'a Metrics,
        now_ms: i64,
        run_id: &str,
    ) -> KernelResult<Self> {
        let run = store::require_run(conn, run_id)?;
        Ok(RunTx {
            conn,
            metrics,
            ts: format_ms(now_ms),
            run,
            new_defs: BTreeMap::new(),
        })
    }

    fn state(&self) -> &RunState {
        &self.run.state
    }

    fn append<P: EventPayload>(&mut self, actor: Actor, payload: &P) -> KernelResult<Event> {
        self.append_raw(actor, P::KIND, payload.to_value())
    }

    fn append_raw(&mut self, actor: Actor, kind: EventKind, payload: Value) -> KernelResult<Event> {
        let seq = self.run.state.last_seq + 1;
        let event = Event::build(
            &self.run.state.run_id,
            seq,
            &self.ts,
            kind,
            actor,
            payload,
            &self.run.last_hash,
        );
        store::insert_event(self.conn, &event)?;
        self.run
            .state
            .apply(&event)
            .map_err(|e| internal(format!("fold rejected a kernel event: {e}")))?;
        self.run.last_hash = event.hash.clone();
        self.metrics.event(kind.as_str());
        Ok(event)
    }

    fn park(&mut self, reason: &str, detail: impl Into<String>) -> KernelResult<()> {
        self.append(
            Actor::kernel(),
            &RunParked {
                reason: reason.into(),
                detail: detail.into(),
            },
        )?;
        Ok(())
    }

    fn persist(&mut self) -> KernelResult<()> {
        self.run.updated_at = self.ts.clone();
        store::write_run(self.conn, &self.run)?;
        let run_id = self.run.state.run_id.clone();
        for step in &self.run.state.steps {
            store::write_step(self.conn, &run_id, step, self.new_defs.get(&step.id))?;
        }
        Ok(())
    }
}

impl Kernel {
    /// Opens the kernel on a data directory, creating the layout of
    /// 00-OVERVIEW: the database, the control-plane key on first start (0600),
    /// the trusted keys directory, and `directory.json` when present.
    pub fn open(
        data_dir: &Path,
        config: KernelConfig,
        clock: Arc<dyn Clock>,
    ) -> KernelResult<Kernel> {
        let keys_dir = data_dir.join("keys");
        let trusted_dir = keys_dir.join("trusted");
        fs::create_dir_all(&trusted_dir)?;
        let now = clock.now_ms();
        let key_path = keys_dir.join("control-plane.key");
        let pub_path = keys_dir.join("control-plane.pub");
        let key = if key_path.exists() {
            KeyPair::load(&key_path)?
        } else {
            let key = KeyPair::generate(now);
            key.write_private(&key_path)?;
            key
        };
        if !pub_path.exists() {
            key.write_public(&pub_path)?;
        }
        let trusted = PublicKey::load_dir(&trusted_dir);
        let directory_path = data_dir.join("directory.json");
        let directory = if directory_path.exists() {
            let text = fs::read_to_string(&directory_path)?;
            serde_json::from_str(&text).map_err(|e| {
                KernelError::bad_request(
                    "directory_invalid",
                    format!("{} does not parse: {e}", directory_path.display()),
                )
            })?
        } else {
            Directory::empty()
        };
        let conn = store::open(&data_dir.join("kernos.db"))?;
        tracing::info!(
            data_dir = %data_dir.display(),
            key_id = %key.key_id,
            trusted_keys = trusted.len(),
            directory_users = directory.users.len(),
            "kernel opened"
        );
        Ok(Kernel {
            conn: Mutex::new(conn),
            clock,
            key,
            trusted: Mutex::new(TrustedKeys::default()),
            trusted_dir: Some(trusted_dir),
            directory,
            config,
            metrics: Metrics::new(),
            data_dir: Some(data_dir.to_path_buf()),
            started_at_ms: now,
        })
    }

    /// An in-memory kernel with a fresh key, for tests and embedding.
    pub fn in_memory(config: KernelConfig, clock: Arc<dyn Clock>) -> KernelResult<Kernel> {
        let now = clock.now_ms();
        Ok(Kernel {
            conn: Mutex::new(store::open_in_memory()?),
            clock,
            key: KeyPair::generate(now),
            trusted: Mutex::new(TrustedKeys::default()),
            trusted_dir: None,
            directory: Directory::empty(),
            config,
            metrics: Metrics::new(),
            data_dir: None,
            started_at_ms: now,
        })
    }

    /// An in-memory kernel on the system clock with default configuration.
    pub fn ephemeral() -> KernelResult<Kernel> {
        Kernel::in_memory(KernelConfig::default(), Arc::new(SystemClock))
    }

    /// Trusts a publisher key for the life of this process, without writing a
    /// file. Keys installed in `KERNOS_DATA/keys/trusted` are picked up on their
    /// own and need no call.
    pub fn trust_key(&self, key: PublicKey) {
        let mut cache = self.trusted_cache();
        cache.pinned.push(key);
    }

    /// Replaces the reporting-line directory.
    pub fn set_directory(&mut self, directory: Directory) {
        self.directory = directory;
    }

    /// The control-plane key.
    pub fn key(&self) -> &KeyPair {
        &self.key
    }

    /// The publisher keys trusted right now, re-reading the trusted directory
    /// when it has changed since the last look.
    pub fn trusted_keys(&self) -> Vec<PublicKey> {
        let mut cache = self.trusted_cache();
        self.refresh_trusted(&mut cache);
        cache
            .pinned
            .iter()
            .chain(cache.from_dir.iter())
            .cloned()
            .collect()
    }

    /// The directory.
    pub fn directory(&self) -> &Directory {
        &self.directory
    }

    /// The configuration.
    pub fn config(&self) -> &KernelConfig {
        &self.config
    }

    /// The metrics.
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// The data directory, when opened on one.
    pub fn data_dir(&self) -> Option<&Path> {
        self.data_dir.as_deref()
    }

    /// The current time.
    pub fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    fn lock(&self) -> KernelResult<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| internal("store lock poisoned"))
    }

    /// Runs a read-only closure against the store.
    pub fn with_store<T>(&self, f: impl FnOnce(&Connection) -> KernelResult<T>) -> KernelResult<T> {
        let guard = self.lock()?;
        f(&guard)
    }

    /// The trusted key cache. Lock poisoning cannot lose the keys: the guard is
    /// taken from the poisoned lock rather than turning every later signature
    /// check into an error.
    fn trusted_cache(&self) -> std::sync::MutexGuard<'_, TrustedKeys> {
        match self.trusted.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Re-reads the trusted directory when its file set, sizes or modification
    /// times have changed, so a key trusted a moment ago is usable at once and a
    /// key removed a moment ago stops being accepted, with no restart.
    fn refresh_trusted(&self, cache: &mut TrustedKeys) {
        let Some(dir) = self.trusted_dir.as_deref() else {
            return;
        };
        let fingerprint = trust_fingerprint(dir);
        if cache.loaded && fingerprint == cache.fingerprint {
            return;
        }
        let keys = PublicKey::load_dir(dir);
        if cache.loaded {
            tracing::info!(
                dir = %dir.display(),
                trusted_keys = keys.len(),
                "trusted publisher keys reloaded"
            );
        }
        cache.from_dir = keys;
        cache.fingerprint = fingerprint;
        cache.loaded = true;
    }

    fn resolve_key(&self, key_id: &str) -> Option<PublicKey> {
        if key_id == self.key.key_id {
            return Some(self.key.public());
        }
        let mut cache = self.trusted_cache();
        self.refresh_trusted(&mut cache);
        cache
            .pinned
            .iter()
            .chain(cache.from_dir.iter())
            .find(|k| k.key_id == key_id)
            .cloned()
    }

    fn all_keys(&self) -> Vec<PublicKey> {
        let mut keys = vec![self.key.public()];
        keys.extend(self.trusted_keys());
        keys
    }

    // ------------------------------------------------------------ health

    /// `GET /v1/health`.
    pub fn health(&self) -> KernelResult<HealthReport> {
        let by_state = self.with_store(store::runs_by_state)?;
        Ok(HealthReport {
            ok: true,
            version: VERSION.to_string(),
            uptime_s: ((self.now_ms() - self.started_at_ms).max(0) / 1000) as u64,
            runs: HealthRuns {
                running: by_state.get("running").copied().unwrap_or(0),
                parked: by_state.get("parked").copied().unwrap_or(0),
            },
        })
    }

    /// `GET /v1/keys`.
    pub fn public_key_info(&self) -> Value {
        json!({"key_id": self.key.key_id, "algorithm": "ed25519", "public_key": self.key.public().public_key_b64()})
    }

    /// `GET /v1/metrics`.
    pub fn metrics_text(&self) -> KernelResult<String> {
        let (by_state, pending) = self
            .with_store(|c| Ok((store::runs_by_state(c)?, store::count_pending_approvals(c)?)))?;
        Ok(self.metrics.render(&by_state, pending))
    }

    // ------------------------------------------------------------ bundles

    /// `POST /v1/bundles`: verifies the signature, validates, stores.
    pub fn apply_bundle(
        &self,
        bundle: Value,
        signature: BundleSignature,
    ) -> KernelResult<BundleApplied> {
        let keys = self.all_keys();
        verify_bundle_signature(&bundle, &signature, &keys)
            .map_err(|reason| KernelError::unprocessable("bundle_signature_invalid", reason))?;
        let parsed = Bundle::new(bundle.clone()).map_err(|BundleError { path, message }| {
            KernelError::unprocessable("bundle_invalid", format!("{path}: {message}"))
                .with_details(json!({"path": path, "message": message}))
        })?;
        let sha = sha256_hex(&canonical_bytes(&bundle));
        let now = self.now_ms();
        let guard = self.lock()?;
        if let Some(existing) = store::find_bundle(&guard, parsed.name(), parsed.version())? {
            if existing.sha256 == sha {
                return Ok(BundleApplied {
                    created: false,
                    bundle_id: existing.bundle_id,
                    name: existing.name,
                    version: existing.version,
                });
            }
            return Err(KernelError::conflict(
                "bundle_version_exists",
                format!(
                    "{}@{} already exists with different content",
                    parsed.name(),
                    parsed.version()
                ),
            ));
        }
        let row = BundleRow {
            bundle_id: new_id("bnd", now),
            name: parsed.name().to_string(),
            version: parsed.version().to_string(),
            department: parsed.department().map(str::to_string),
            sha256: sha,
            bundle,
            signature: serde_json::to_value(&signature)?,
            created_at: format_ms(now),
        };
        store::insert_bundle(&guard, &row)?;
        tracing::info!(bundle_id = %row.bundle_id, name = %row.name, version = %row.version, "bundle applied");
        Ok(BundleApplied {
            created: true,
            bundle_id: row.bundle_id,
            name: row.name,
            version: row.version,
        })
    }

    /// Signs a bundle with the control plane's own key (used by the CLI when no
    /// publisher key is given and the server is local).
    pub fn sign_bundle(&self, bundle: &Value) -> BundleSignature {
        sign_bundle(bundle, &self.key)
    }

    /// `GET /v1/bundles`.
    pub fn list_bundles(&self) -> KernelResult<Vec<Value>> {
        let rows = self.with_store(store::list_bundles)?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let bundle = Bundle::from_stored(r.bundle);
                json!({
                    "bundle_id": r.bundle_id, "name": r.name, "version": r.version, "department": r.department,
                    "workflows": bundle.workflow_names(), "created_at": r.created_at,
                })
            })
            .collect())
    }

    /// `GET /v1/bundles/{id}`.
    pub fn get_bundle(&self, bundle_id: &str) -> KernelResult<Value> {
        let row = self
            .with_store(|c| store::get_bundle(c, bundle_id))?
            .ok_or_else(|| {
                KernelError::not_found(
                    "bundle_not_found",
                    format!("bundle {bundle_id} does not exist"),
                )
            })?;
        Ok(json!({
            "bundle_id": row.bundle_id, "name": row.name, "version": row.version, "department": row.department,
            "bundle": row.bundle, "signature": row.signature, "created_at": row.created_at,
        }))
    }

    /// Finds a bundle id by `name` and `version`.
    pub fn find_bundle(&self, name: &str, version: &str) -> KernelResult<Option<String>> {
        Ok(self
            .with_store(|c| store::find_bundle(c, name, version))?
            .map(|r| r.bundle_id))
    }

    // ------------------------------------------------------------ policies

    /// `POST /v1/policies`.
    pub fn apply_policy(
        &self,
        name: &str,
        version: u64,
        source: &str,
    ) -> KernelResult<PolicyApplied> {
        if name.is_empty() {
            return Err(invalid("policy name is required").with_details(json!({"field": "name"})));
        }
        kernos_policy::parse(source).map_err(policy_invalid)?;
        let now = self.now_ms();
        let guard = self.lock()?;
        if let Some(existing) = store::get_policy(&guard, name, version)? {
            if existing.source == source {
                return Ok(PolicyApplied {
                    created: false,
                    policy_id: existing.policy_id,
                    name: existing.name,
                    version: existing.version,
                });
            }
            return Err(KernelError::conflict(
                "policy_version_exists",
                format!("{name}@{version} already exists with different source"),
            ));
        }
        let row = PolicyRow {
            policy_id: new_id("pol", now),
            name: name.to_string(),
            version,
            source: source.to_string(),
            created_at: format_ms(now),
        };
        store::insert_policy(&guard, &row)?;
        tracing::info!(policy_id = %row.policy_id, name, version, "policy applied");
        Ok(PolicyApplied {
            created: true,
            policy_id: row.policy_id,
            name: row.name,
            version: row.version,
        })
    }

    /// `GET /v1/policies`.
    pub fn list_policies(&self) -> KernelResult<Vec<Value>> {
        let rows = self.with_store(store::list_policies)?;
        Ok(rows.iter().map(policy_summary).collect())
    }

    /// `GET /v1/policies/{name}`.
    pub fn policy_versions(&self, name: &str) -> KernelResult<Vec<Value>> {
        let rows = self.with_store(|c| store::policy_versions(c, name))?;
        if rows.is_empty() {
            return Err(KernelError::not_found(
                "policy_not_found",
                format!("policy {name} does not exist"),
            ));
        }
        Ok(rows.iter().map(policy_summary).collect())
    }

    /// `GET /v1/policies/{name}/{version}`.
    pub fn policy_source(&self, name: &str, version: u64) -> KernelResult<Value> {
        let row = self
            .with_store(|c| store::get_policy(c, name, version))?
            .ok_or_else(|| {
                KernelError::not_found(
                    "policy_not_found",
                    format!("policy {name}@{version} does not exist"),
                )
            })?;
        Ok(
            json!({"policy_id": row.policy_id, "name": row.name, "version": row.version, "source": row.source, "created_at": row.created_at}),
        )
    }

    fn select_policy(
        &self,
        conn: &Connection,
        selector: &PolicySelector,
    ) -> KernelResult<LoadedPolicy> {
        match selector {
            PolicySelector::Stored { name, version } => {
                let row = store::get_policy(conn, name, *version)?.ok_or_else(|| {
                    KernelError::not_found(
                        "policy_not_found",
                        format!("policy {name}@{version} does not exist"),
                    )
                })?;
                parse_policy_row(&row)
            }
            PolicySelector::Inline { source } => {
                let policy = kernos_policy::parse(source).map_err(policy_invalid)?;
                let name = policy.name.clone().unwrap_or_else(|| "draft".to_string());
                Ok(LoadedPolicy::new(name, "draft", policy))
            }
        }
    }

    /// `POST /v1/policies/test`.
    pub fn test_policies(
        &self,
        a: &PolicySelector,
        b: &PolicySelector,
        corpus: &[Value],
    ) -> KernelResult<Value> {
        let (pa, pb) = {
            let guard = self.lock()?;
            (
                self.select_policy(&guard, a)?,
                self.select_policy(&guard, b)?,
            )
        };
        let flips = kernos_policy::test_corpus(&[pa], &[pb], corpus);
        Ok(json!({"cases": corpus.len(), "flips": flips}))
    }

    // ------------------------------------------------------------ remits

    /// `POST /v1/remits`.
    pub fn issue_remit(&self, request: &IssueRequest) -> KernelResult<RemitIssued> {
        validate_patterns(&request.tools, "tools")?;
        validate_patterns(&request.scopes, "scopes")?;
        if let Some(rb) = &request.requested_by {
            validate_requested_by(rb)?;
        }
        let ttl = request.ttl_seconds.unwrap_or(DEFAULT_TTL_SECONDS);
        if ttl == 0 {
            return Err(invalid("ttl_seconds must be positive")
                .with_details(json!({"field": "ttl_seconds"})));
        }
        let now_ms = self.now_ms();
        let now_s = now_ms.div_euclid(1000);
        let payload = RemitPayload {
            rid: new_id("rem", now_ms),
            parent: None,
            run: None,
            iss: self.key.key_id.clone(),
            iat: now_s,
            nbf: now_s,
            exp: now_s.saturating_add(ttl as i64),
            tools: request.tools.clone(),
            scopes: request.scopes.clone(),
            grants: request.grants.clone(),
            spend: request.spend,
            autonomy: request.autonomy.unwrap_or(Autonomy::Observe),
            policy_set: request.policy_set.clone(),
            requested_by: request.requested_by.clone(),
        };
        self.store_remit(payload, None)
    }

    fn store_remit(
        &self,
        payload: RemitPayload,
        run_id: Option<String>,
    ) -> KernelResult<RemitIssued> {
        let token = encode_token(&payload, &self.key)?;
        let row = RemitRow {
            remit_id: payload.rid.clone(),
            parent_id: payload.parent.clone(),
            run_id,
            bound_run_id: None,
            expires_at_ms: payload.exp * 1000,
            created_at: format_ms(self.now_ms()),
            payload,
            token,
        };
        self.with_store(|c| store::insert_remit(c, &row))?;
        Ok(RemitIssued {
            remit_id: row.remit_id,
            parent_id: row.parent_id,
            token: row.token,
            expires_at: format_ms(row.expires_at_ms),
        })
    }

    /// `POST /v1/remits/{id}/derive`.
    pub fn derive_remit(
        &self,
        remit_id: &str,
        request: &DeriveRequest,
    ) -> KernelResult<RemitIssued> {
        if let Some(tools) = &request.tools {
            validate_patterns(tools, "tools")?;
        }
        if let Some(scopes) = &request.scopes {
            validate_patterns(scopes, "scopes")?;
        }
        let parent = self
            .with_store(|c| store::get_remit(c, remit_id))?
            .ok_or_else(|| {
                KernelError::not_found(
                    "remit_not_found",
                    format!("remit {remit_id} does not exist"),
                )
            })?;
        let now_ms = self.now_ms();
        let now_s = now_ms.div_euclid(1000);
        if now_s >= parent.payload.exp {
            return Err(KernelError::unprocessable(
                "remit_expired",
                format!("remit {remit_id} has expired"),
            ));
        }
        let narrowed = narrow(&parent.payload, request, now_s).map_err(|e| {
            KernelError::unprocessable("remit_widens", e.to_string())
                .with_details(json!({"field": e.field}))
        })?;
        let payload = RemitPayload {
            rid: new_id("rem", now_ms),
            parent: Some(parent.remit_id.clone()),
            run: None,
            iss: self.key.key_id.clone(),
            iat: now_s,
            nbf: narrowed.nbf,
            exp: narrowed.exp,
            tools: narrowed.tools,
            scopes: narrowed.scopes,
            grants: narrowed.grants,
            spend: narrowed.spend,
            autonomy: narrowed.autonomy,
            policy_set: narrowed.policy_set,
            requested_by: parent.payload.requested_by.clone(),
        };
        self.store_remit(payload, None)
    }

    /// `GET /v1/remits/{id}`: the payload without the signature plus ids.
    pub fn get_remit(&self, remit_id: &str) -> KernelResult<Value> {
        let row = self
            .with_store(|c| store::get_remit(c, remit_id))?
            .ok_or_else(|| {
                KernelError::not_found(
                    "remit_not_found",
                    format!("remit {remit_id} does not exist"),
                )
            })?;
        let mut value = serde_json::to_value(&row.payload)?;
        if let Some(obj) = value.as_object_mut() {
            obj.insert("remit_id".into(), json!(row.remit_id));
            obj.insert("parent_id".into(), json!(row.parent_id));
            obj.insert("run_id".into(), json!(row.run_id.or(row.bound_run_id)));
            obj.insert("expires_at".into(), json!(format_ms(row.expires_at_ms)));
            obj.insert("created_at".into(), json!(row.created_at));
        }
        Ok(value)
    }

    /// Verifies a token against the control-plane key and the clock.
    pub fn verify_remit_token(&self, token: &str) -> Result<RemitPayload, TokenError> {
        verify_token(
            token,
            |id| self.resolve_key(id),
            self.now_ms().div_euclid(1000),
        )
    }

    // ------------------------------------------------------------ runs

    /// `POST /v1/runs`.
    pub fn start_run(&self, request: &StartRunRequest) -> KernelResult<RunStarted> {
        let now_ms = self.now_ms();
        let now_s = now_ms.div_euclid(1000);
        let mut guard = self.lock()?;
        let tx = guard.transaction()?;
        let bundle_row = store::get_bundle(&tx, &request.bundle_id)?.ok_or_else(|| {
            KernelError::not_found(
                "bundle_not_found",
                format!("bundle {} does not exist", request.bundle_id),
            )
        })?;
        let bundle = Bundle::from_stored(bundle_row.bundle);
        if bundle.workflow(&request.workflow).is_none() {
            return Err(KernelError::not_found(
                "workflow_not_found",
                format!(
                    "bundle {} has no workflow {}",
                    bundle.name(),
                    request.workflow
                ),
            ));
        }
        validate_schema(&bundle.input_schema(&request.workflow), &request.input).map_err(|v| {
            KernelError::unprocessable("input_invalid", format!("input {}: {}", v.path, v.message))
                .with_details(json!({"path": v.path, "message": v.message}))
        })?;
        let remit = store::get_remit(&tx, &request.remit_id)?.ok_or_else(|| {
            KernelError::not_found(
                "remit_not_found",
                format!("remit {} does not exist", request.remit_id),
            )
        })?;
        if now_s >= remit.payload.exp {
            return Err(KernelError::unprocessable(
                "remit_expired",
                format!("remit {} has expired", remit.remit_id),
            ));
        }
        if remit.bound_run_id.is_some() || remit.payload.run.is_some() {
            return Err(KernelError::conflict(
                "remit_bound",
                format!("remit {} is already bound to a run", remit.remit_id),
            ));
        }
        for policy in bundle.policies() {
            if !remit.payload.policy_set.contains(&policy) {
                return Err(KernelError::unprocessable(
                    "remit_policy_set_missing",
                    format!("the remit's policy_set does not include {policy}, which the bundle requires"),
                )
                .with_details(json!({"policy": policy})));
            }
        }
        load_policy_set(&tx, &remit.payload.policy_set)?;
        let requested_by = match &request.requested_by {
            Some(rb) => {
                validate_requested_by(rb)?;
                rb.clone()
            }
            None => remit.payload.requested_by.clone().ok_or_else(|| {
                invalid("requested_by is required").with_details(json!({"field": "requested_by"}))
            })?,
        };

        let run_id = new_id("run", now_ms);
        let child = RemitPayload {
            rid: new_id("rem", now_ms),
            parent: Some(remit.remit_id.clone()),
            run: Some(run_id.clone()),
            iss: self.key.key_id.clone(),
            iat: now_s,
            nbf: now_s.max(remit.payload.nbf),
            exp: remit.payload.exp,
            tools: remit.payload.tools.clone(),
            scopes: remit.payload.scopes.clone(),
            grants: remit.payload.grants.clone(),
            spend: remit.payload.spend,
            autonomy: remit.payload.autonomy,
            policy_set: remit.payload.policy_set.clone(),
            requested_by: Some(requested_by.clone()),
        };
        let child_row = RemitRow {
            remit_id: child.rid.clone(),
            parent_id: child.parent.clone(),
            run_id: Some(run_id.clone()),
            bound_run_id: None,
            token: encode_token(&child, &self.key)?,
            expires_at_ms: child.exp * 1000,
            created_at: format_ms(now_ms),
            payload: child,
        };
        store::insert_remit(&tx, &child_row)?;
        store::bind_remit(&tx, &remit.remit_id, &run_id)?;

        let created = RunCreated {
            bundle_id: bundle_row.bundle_id.clone(),
            bundle_name: bundle.name().to_string(),
            bundle_version: bundle.version().to_string(),
            workflow: request.workflow.clone(),
            input: request.input.clone(),
            remit_id: remit.remit_id.clone(),
            requested_by,
            budget: BudgetSpec {
                tokens: remit.payload.spend.tokens,
                usd: remit.payload.spend.usd,
                soft_ratio: self.config.budget_soft_ratio,
            },
            department: bundle.department().map(str::to_string),
        };
        let ts = format_ms(now_ms);
        let first = Event::build(
            &run_id,
            1,
            &ts,
            EventKind::RunCreated,
            Actor::kernel(),
            created.to_value(),
            crate::events::ZERO_HASH,
        );
        store::insert_event(&tx, &first)?;
        self.metrics.event(first.kind.as_str());
        let state = RunState::from_created(&first).map_err(|e| internal(e.to_string()))?;
        let mut run = RunTx {
            conn: &tx,
            metrics: &self.metrics,
            ts: ts.clone(),
            run: RunRow {
                state,
                child_remit_id: child_row.remit_id.clone(),
                created_at: ts.clone(),
                updated_at: ts.clone(),
                last_hash: first.hash.clone(),
            },
            new_defs: BTreeMap::new(),
        };
        for (index, step) in bundle.steps(&request.workflow).into_iter().enumerate() {
            let id = step
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let kind = step
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            run.new_defs.insert(id.clone(), step.clone());
            run.append(
                Actor::kernel(),
                &StepScheduled {
                    step: id,
                    index: index as u32,
                    kind,
                },
            )?;
        }
        let state = run.state().state;
        run.persist()?;
        drop(run);
        tx.commit()?;
        tracing::info!(run_id = %run_id, bundle = %bundle.name(), workflow = %request.workflow, "run started");
        Ok(RunStarted { run_id, state })
    }

    /// `GET /v1/runs`.
    pub fn list_runs(&self, filter: &RunFilter) -> KernelResult<RunPage> {
        let rows = self.with_store(|c| store::list_runs(c, filter))?;
        let next = if rows.len() as u64 >= filter.limit.max(1) {
            rows.last().map(|r| r.state.run_id.clone())
        } else {
            None
        };
        Ok(RunPage {
            runs: rows.into_iter().map(|r| r.state).collect(),
            next,
        })
    }

    /// `GET /v1/runs/{id}`.
    pub fn get_run(&self, run_id: &str) -> KernelResult<RunState> {
        Ok(self.with_store(|c| store::require_run(c, run_id))?.state)
    }

    /// `GET /v1/runs/{id}/events`.
    pub fn run_events(&self, run_id: &str, from_seq: u64, limit: u64) -> KernelResult<EventPage> {
        let (row, events) = self.with_store(|c| {
            let row = store::require_run(c, run_id)?;
            let events = store::events_from(c, run_id, from_seq.max(1), limit.clamp(1, 5000))?;
            Ok((row, events))
        })?;
        let next_seq = match events.last() {
            Some(last) if last.seq < row.state.last_seq => Some(last.seq + 1),
            _ => None,
        };
        Ok(EventPage { events, next_seq })
    }

    /// All events of a run, for replay and tests.
    pub fn all_events(&self, run_id: &str) -> KernelResult<Vec<Event>> {
        self.with_store(|c| {
            store::require_run(c, run_id)?;
            store::all_events(c, run_id)
        })
    }

    /// `POST /v1/runs/{id}/events`: an external append under the permission
    /// rules of 01-EVENTS.
    pub fn post_external_event(
        &self,
        run_id: &str,
        kind: &str,
        payload: Value,
        actor: Value,
        auth: &ExternalAuth,
    ) -> KernelResult<Appended> {
        let kind = EventKind::parse(kind).ok_or_else(|| {
            KernelError::forbidden("event_not_permitted", format!("unknown event kind {kind}"))
        })?;
        if !kind.is_external() {
            return Err(KernelError::forbidden(
                "event_not_permitted",
                format!("{kind} may only be appended by the kernel"),
            ));
        }
        let actor: Actor = serde_json::from_value(actor).map_err(|e| {
            invalid(format!("actor must be {{type, id}}: {e}"))
                .with_details(json!({"field": "actor"}))
        })?;
        if !matches!(
            actor.kind,
            ActorType::Worker | ActorType::Gateway | ActorType::System | ActorType::User
        ) {
            return Err(KernelError::forbidden(
                "event_not_permitted",
                "external actors are worker, gateway, system or user",
            ));
        }
        let payload = truncate_payload(payload).ok_or_else(|| {
            KernelError::api(
                413,
                "payload_too_large",
                "payload exceeds 256 KiB and cannot be truncated",
            )
        })?;
        let now_ms = self.now_ms();
        let mut guard = self.lock()?;
        let tx = guard.transaction()?;
        let mut run = RunTx::load(&tx, &self.metrics, now_ms, run_id)?;
        match auth {
            ExternalAuth::Lease(lease_id) => {
                let lease = store::get_lease(&tx, lease_id)?;
                let ok = lease.as_ref().is_some_and(|l| {
                    l.run_id == run_id
                        && l.state == "active"
                        && l.expires_at_ms > now_ms
                        && payload
                            .get("step")
                            .and_then(Value::as_str)
                            .is_none_or(|s| s == l.step_id)
                });
                if !ok {
                    return Err(KernelError::forbidden(
                        "event_not_permitted",
                        "the lease is not live for this run and step",
                    ));
                }
            }
            ExternalAuth::Remit(token) => {
                let remit = self.verify_remit_token(token).map_err(|e| {
                    KernelError::forbidden(
                        "event_not_permitted",
                        format!("remit token refused: {}", e.reason()),
                    )
                    .with_details(json!({"reason": e.reason()}))
                })?;
                let valid_for_run = remit.run.as_deref() == Some(run_id)
                    || remit.rid == run.state().remit_id
                    || remit.rid == run.run.child_remit_id;
                if !valid_for_run {
                    return Err(KernelError::forbidden(
                        "event_not_permitted",
                        "the remit token is not valid for this run",
                    ));
                }
            }
            ExternalAuth::None => {
                return Err(KernelError::forbidden(
                    "event_not_permitted",
                    "present X-Kernos-Lease or X-Kernos-Remit",
                ))
            }
        }
        let event = run.append_raw(actor, kind, payload)?;
        run.persist()?;
        drop(run);
        tx.commit()?;
        Ok(Appended {
            seq: event.seq,
            hash: event.hash,
        })
    }

    /// `POST /v1/runs/{id}/replay`.
    pub fn replay(&self, run_id: &str) -> KernelResult<ReplayReport> {
        let guard = self.lock()?;
        let row = store::require_run(&guard, run_id)?;
        replay_run(&guard, &row)
    }

    /// `POST /v1/runs/{id}/abandon`: schedules compensations in reverse order.
    pub fn abandon(&self, run_id: &str, reason: &str, actor: Value) -> KernelResult<u64> {
        let now_ms = self.now_ms();
        let mut guard = self.lock()?;
        let tx = guard.transaction()?;
        let mut run = RunTx::load(&tx, &self.metrics, now_ms, run_id)?;
        if !matches!(
            run.state().state,
            RunStatus::Running | RunStatus::Parked | RunStatus::Failed
        ) {
            return Err(KernelError::conflict(
                "run_not_abandonable",
                format!(
                    "run {run_id} is {} and cannot be abandoned",
                    run.state().state.as_str()
                ),
            ));
        }
        if run.state().unwind_pending() {
            return Err(KernelError::conflict(
                "run_not_abandonable",
                format!("run {run_id} is already unwinding; wait for its compensations to finish"),
            ));
        }
        for lease in store::active_leases_of_run(&tx, run_id)? {
            store::end_lease(&tx, &lease.lease_id, "released", now_ms)?;
        }
        for approval in store::pending_in_run(&tx, run_id)? {
            store::decide_approval(
                &tx,
                &approval.approval_id,
                "cancelled",
                &json!(null),
                "run abandoned",
                &format_ms(now_ms),
            )?;
        }
        let actor_id = actor
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("operator")
            .to_string();
        let event_actor = Actor::user(actor_id);
        run.append(
            event_actor.clone(),
            &RunAbandoned {
                reason: reason.to_string(),
                actor,
            },
        )?;

        let state = run.state().clone();
        let context = template_context(&state);
        let mut next_index = state.steps.iter().map(|s| s.index + 1).max().unwrap_or(0);
        let mut scheduled = 0u64;
        let mut failed: Option<ErrorInfo> = None;
        let completed: Vec<_> = state
            .workflow_steps()
            .filter(|s| s.state == StepStatus::Completed)
            .cloned()
            .collect();
        for step in completed.iter().rev() {
            if state
                .compensations
                .iter()
                .any(|c| c.for_step == step.id && c.state != "failed")
            {
                continue;
            }
            let Some(def) = store::get_step(&tx, run_id, &step.id)?.map(|r| r.def) else {
                continue;
            };
            let Some(comp) = def.get("compensation").filter(|c| c.is_object()) else {
                continue;
            };
            let tool = comp
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let args = match resolve(comp.get("args").unwrap_or(&json!({})), &context) {
                Ok(args) => args,
                Err(e) => {
                    let error = ErrorInfo {
                        code: e.code.to_string(),
                        message: format!(
                            "compensation for {} could not be resolved: {}",
                            step.id, e.path
                        ),
                    };
                    run.append(
                        Actor::kernel(),
                        &CompensationFailed {
                            step: format!("comp_{}", step.id),
                            for_step: step.id.clone(),
                            error: error.clone(),
                        },
                    )?;
                    failed = Some(error);
                    continue;
                }
            };
            let comp_id = compensation_step_id(&state, &step.id);
            run.append(
                Actor::kernel(),
                &CompensationScheduled {
                    step: comp_id.clone(),
                    for_step: step.id.clone(),
                    tool: tool.clone(),
                    args: args.clone(),
                },
            )?;
            run.new_defs.insert(
                comp_id.clone(),
                json!({
                    "id": comp_id,
                    "kind": "compensation",
                    "tool": tool,
                    "args": args,
                    "for_step": step.id,
                    "idempotency_key": format!("comp:{run_id}:{}", step.id),
                    "timeout_seconds": def.get("timeout_seconds").cloned().unwrap_or(json!(crate::bundle::DEFAULT_TIMEOUT_SECONDS)),
                }),
            );
            run.append(
                Actor::kernel(),
                &StepScheduled {
                    step: comp_id,
                    index: next_index,
                    kind: "compensation".into(),
                },
            )?;
            next_index += 1;
            scheduled += 1;
        }
        if let Some(error) = failed {
            run.append(
                Actor::kernel(),
                &RunFailed {
                    error,
                    needs_human: true,
                },
            )?;
        }
        run.persist()?;
        drop(run);
        tx.commit()?;
        tracing::info!(run_id, reason, compensations = scheduled, "run abandoned");
        Ok(scheduled)
    }

    /// `POST /v1/runs/{id}/resume`.
    pub fn resume(&self, run_id: &str, actor: Value) -> KernelResult<RunStatus> {
        let now_ms = self.now_ms();
        let mut guard = self.lock()?;
        let tx = guard.transaction()?;
        let mut run = RunTx::load(&tx, &self.metrics, now_ms, run_id)?;
        if run.state().state != RunStatus::Parked {
            return Err(KernelError::conflict(
                "run_not_parked",
                format!(
                    "run {run_id} is {} and cannot be resumed",
                    run.state().state.as_str()
                ),
            ));
        }
        let reason = run.state().park_reason.clone().unwrap_or_default();
        if !matches!(
            reason.as_str(),
            "human" | "connector_quarantined" | "quarantine" | "refusal"
        ) {
            return Err(KernelError::conflict(
                "run_not_resumable",
                format!("run {run_id} is parked for {reason}; only human, connector_quarantined, quarantine and refusal parks resume"),
            ));
        }
        if !store::pending_in_run(&tx, run_id)?.is_empty() {
            return Err(KernelError::conflict(
                "approval_pending",
                format!("run {run_id} has a pending approval; decide it instead of resuming"),
            ));
        }
        let actor_id = actor
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("operator")
            .to_string();
        run.append(
            Actor::user(actor_id),
            &RunResumed {
                reason: "operator".into(),
                actor: Some(actor),
            },
        )?;
        let stuck: Vec<_> = run
            .state()
            .workflow_steps()
            .filter(|s| matches!(s.state, StepStatus::Quarantined | StepStatus::Failed))
            .map(|s| (s.id.clone(), s.index, s.kind.clone()))
            .collect();
        for (id, index, kind) in stuck {
            run.append(
                Actor::kernel(),
                &StepScheduled {
                    step: id,
                    index,
                    kind,
                },
            )?;
        }
        let state = run.state().state;
        run.persist()?;
        drop(run);
        tx.commit()?;
        tracing::info!(run_id, "run resumed");
        Ok(state)
    }

    /// `GET /v1/actions?since=`: the `{action, run}` corpus for policy testing.
    pub fn export_actions(&self, since_ms: i64) -> KernelResult<Vec<Value>> {
        let rows = self.with_store(|c| store::actions_since(c, &format_ms(since_ms)))?;
        Ok(rows
            .into_iter()
            .map(|r| json!({"action": r.action, "run": r.context, "decision": r.decision, "rule": r.rule, "action_id": r.action_id, "run_id": r.run_id}))
            .collect())
    }

    // ------------------------------------------------------------ leases

    /// `POST /v1/leases`: `None` when nothing is runnable.
    pub fn lease(&self, request: &LeaseRequest) -> KernelResult<Option<LeaseGrant>> {
        if request.worker_id.trim().is_empty() {
            return Err(
                invalid("worker_id is required").with_details(json!({"field": "worker_id"}))
            );
        }
        let kinds: Vec<String> = if request.kinds.is_empty() {
            ["model", "tool", "action", "compensation"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            request.kinds.clone()
        };
        let ttl = request
            .ttl_seconds
            .unwrap_or(self.config.lease_ttl_default)
            .clamp(
                self.config.lease_ttl_min,
                self.config.lease_ttl_max.max(self.config.lease_ttl_min),
            );
        let now_ms = self.now_ms();
        let mut guard = self.lock()?;
        let tx = guard.transaction()?;
        let Some((run_id, step_id)) = store::pick_runnable(&tx, &kinds, now_ms)? else {
            return Ok(None);
        };
        let mut run = RunTx::load(&tx, &self.metrics, now_ms, &run_id)?;
        let step = run
            .state()
            .step(&step_id)
            .cloned()
            .ok_or_else(|| internal(format!("runnable step {step_id} missing from state")))?;
        let step_row = store::get_step(&tx, &run_id, &step_id)?
            .ok_or_else(|| internal(format!("step {step_id} has no row")))?;
        let bundle_row = store::get_bundle(&tx, &run.state().bundle.id)?
            .ok_or_else(|| internal(format!("bundle {} missing", run.state().bundle.id)))?;
        let bundle = Bundle::from_stored(bundle_row.bundle);
        let child = store::get_remit(&tx, &run.run.child_remit_id)?
            .ok_or_else(|| internal("run-bound remit missing"))?;
        let lease_id = new_id("lse", now_ms);
        let attempt = step.attempts + 1;
        let expires_at_ms = now_ms + ttl as i64 * 1000;
        run.append(
            Actor::kernel(),
            &StepLeased {
                step: step_id.clone(),
                lease_id: lease_id.clone(),
                worker_id: request.worker_id.clone(),
                attempt,
                expires_at: format_ms(expires_at_ms),
            },
        )?;
        store::insert_lease(
            &tx,
            &LeaseRow {
                lease_id: lease_id.clone(),
                run_id: run_id.clone(),
                step_id: step_id.clone(),
                worker_id: request.worker_id.clone(),
                attempt,
                ttl_seconds: ttl,
                issued_at_ms: now_ms,
                expires_at_ms,
                state: "active".into(),
            },
        )?;
        let state = run.state().clone();
        let mut steps = serde_json::Map::new();
        for s in &state.steps {
            if let Some(output) = &s.output {
                steps.insert(s.id.clone(), json!({"output": output}));
            }
        }
        let approved: Vec<String> = store::approved_in_run(&tx, &run_id)?
            .into_iter()
            .map(|a| a.action_id)
            .collect();
        let prior: Vec<Value> = store::step_events(
            &tx,
            &run_id,
            &step_id,
            &[EventKind::ToolCalled, EventKind::ToolResult],
        )?
        .into_iter()
        .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
        .collect();
        let remit = &child.payload;
        let context = json!({
            "input": state.input,
            "steps": steps,
            "run": {
                "id": state.run_id,
                "bundle": {"name": state.bundle.name, "version": state.bundle.version},
                "workflow": state.workflow,
                "requested_by": state.requested_by,
                "department": state.department,
            },
            "remit_token": child.token,
            "remit": {"autonomy": remit.autonomy, "grants": remit.grants, "tools": remit.tools, "scopes": remit.scopes},
            "prompts": bundle.prompts(),
            "mock": bundle.mock(),
            "tools": bundle.tools(),
            "pacing": state.budget.soft_hit,
            "approved_actions": approved,
            "prior_events": prior,
        });
        run.persist()?;
        drop(run);
        tx.commit()?;
        self.metrics.step_leased();
        tracing::info!(run_id = %run_id, step = %step_id, lease_id = %lease_id, worker_id = %request.worker_id, attempt, "step leased");
        Ok(Some(LeaseGrant {
            lease_id,
            run_id,
            step: step_id,
            attempt,
            expires_at: format_ms(expires_at_ms),
            heartbeat_seconds: (ttl / 3).max(1),
            step_def: step_row.def,
            context,
        }))
    }

    /// Loads a lease that must be live; an expired one is swept on the spot.
    fn live_lease(&self, tx: &Connection, lease_id: &str, now_ms: i64) -> KernelResult<LeaseRow> {
        let lease = store::get_lease(tx, lease_id)?.ok_or_else(|| {
            KernelError::not_found(
                "lease_not_found",
                format!("lease {lease_id} does not exist"),
            )
        })?;
        if lease.state == "active" && lease.expires_at_ms <= now_ms {
            self.expire_lease(tx, &lease, now_ms)?;
            return Err(KernelError::gone(
                "lease_expired",
                format!("lease {lease_id} expired"),
            ));
        }
        if lease.state != "active" {
            return Err(KernelError::gone(
                "lease_expired",
                format!("lease {lease_id} is {}", lease.state),
            ));
        }
        Ok(lease)
    }

    fn expire_lease(&self, tx: &Connection, lease: &LeaseRow, now_ms: i64) -> KernelResult<()> {
        let mut run = RunTx::load(tx, &self.metrics, now_ms, &lease.run_id)?;
        let held = run
            .state()
            .step(&lease.step_id)
            .and_then(|s| s.lease.as_ref())
            .is_some_and(|l| l.lease_id == lease.lease_id);
        if held {
            run.append(
                Actor::system("lease-sweeper"),
                &StepLeaseExpired {
                    step: lease.step_id.clone(),
                    lease_id: lease.lease_id.clone(),
                    worker_id: lease.worker_id.clone(),
                },
            )?;
            run.persist()?;
        }
        store::end_lease(tx, &lease.lease_id, "expired", now_ms)?;
        self.metrics.lease_expired();
        tracing::info!(run_id = %lease.run_id, step = %lease.step_id, lease_id = %lease.lease_id, "lease expired");
        Ok(())
    }

    /// `POST /v1/leases/{id}/heartbeat`.
    pub fn heartbeat(&self, lease_id: &str) -> KernelResult<String> {
        let now_ms = self.now_ms();
        let mut guard = self.lock()?;
        let tx = guard.transaction()?;
        let result = self.live_lease(&tx, lease_id, now_ms);
        let lease = match result {
            Ok(lease) => lease,
            Err(e) => {
                tx.commit()?;
                return Err(e);
            }
        };
        let expires_at_ms = now_ms + lease.ttl_seconds as i64 * 1000;
        store::set_lease_expiry(&tx, lease_id, expires_at_ms)?;
        tx.commit()?;
        Ok(format_ms(expires_at_ms))
    }

    /// Loads a live lease and its run, checking the step is leased under it.
    fn leased_run<'a>(
        &'a self,
        tx: &'a Connection,
        lease_id: &str,
        now_ms: i64,
    ) -> KernelResult<(LeaseRow, RunTx<'a>)> {
        let lease = self.live_lease(tx, lease_id, now_ms)?;
        let run = RunTx::load(tx, &self.metrics, now_ms, &lease.run_id)?;
        let held = run.state().step(&lease.step_id).is_some_and(|s| {
            s.state == StepStatus::Leased
                && s.lease
                    .as_ref()
                    .is_some_and(|l| l.lease_id == lease.lease_id)
        });
        if !held {
            store::end_lease(tx, lease_id, "released", now_ms)?;
            return Err(KernelError::gone(
                "lease_expired",
                format!("lease {lease_id} no longer holds step {}", lease.step_id),
            ));
        }
        Ok((lease, run))
    }

    /// `POST /v1/leases/{id}/complete`.
    pub fn complete(
        &self,
        lease_id: &str,
        output: Value,
        usage: Option<Usage>,
    ) -> KernelResult<CompleteResult> {
        let now_ms = self.now_ms();
        let mut guard = self.lock()?;
        let tx = guard.transaction()?;
        let (lease, mut run) = match self.leased_run(&tx, lease_id, now_ms) {
            Ok(pair) => pair,
            Err(e) => {
                tx.commit()?;
                return Err(e);
            }
        };
        let step = run
            .state()
            .step(&lease.step_id)
            .cloned()
            .ok_or_else(|| internal("leased step missing"))?;
        let payload = truncate_payload(
            StepCompleted {
                step: step.id.clone(),
                lease_id: lease.lease_id.clone(),
                attempt: lease.attempt,
                output,
            }
            .to_value(),
        )
        .ok_or_else(|| {
            KernelError::api(
                413,
                "payload_too_large",
                "output exceeds 256 KiB and cannot be truncated",
            )
        })?;
        let stored_output = payload.get("output").cloned().unwrap_or(Value::Null);
        run.append_raw(
            Actor::worker(lease.worker_id.clone()),
            EventKind::StepCompleted,
            payload,
        )?;
        store::end_lease(&tx, lease_id, "completed", now_ms)?;
        self.metrics
            .step_latency((now_ms - lease.issued_at_ms).max(0) as f64 / 1000.0);

        let mut exceeded = false;
        if let Some(usage) = usage {
            let budget = run.state().budget.clone();
            let cumulative_tokens = budget.used_tokens + usage.tokens;
            let cumulative_usd = budget.used_usd + usage.usd;
            run.append(
                Actor::kernel(),
                &UsageRecorded {
                    step: step.id.clone(),
                    tokens: usage.tokens,
                    usd: usage.usd,
                    cumulative_tokens,
                    cumulative_usd,
                },
            )?;
            let department = run
                .state()
                .department
                .clone()
                .unwrap_or_else(|| "unknown".into());
            self.metrics.usage_usd(&department, usage.usd);
            if !budget.soft_hit {
                let usd_soft = budget
                    .ceiling_usd
                    .filter(|c| *c > 0.0)
                    .map(|c| cumulative_usd / c)
                    .filter(|ratio| *ratio >= budget.soft_ratio);
                let tokens_soft = budget
                    .ceiling_tokens
                    .filter(|c| *c > 0)
                    .map(|c| cumulative_tokens as f64 / c as f64)
                    .filter(|ratio| *ratio >= budget.soft_ratio);
                if usd_soft.is_some() || tokens_soft.is_some() {
                    run.append(
                        Actor::kernel(),
                        &BudgetSoftThreshold {
                            cumulative_usd: usd_soft.map(|_| cumulative_usd),
                            ceiling_usd: usd_soft.and(budget.ceiling_usd),
                            cumulative_tokens: tokens_soft.map(|_| cumulative_tokens),
                            ceiling_tokens: tokens_soft.and(budget.ceiling_tokens),
                            ratio: usd_soft.or(tokens_soft).unwrap_or(0.0),
                        },
                    )?;
                }
            }
            let usd_over = budget.ceiling_usd.is_some_and(|c| cumulative_usd > c);
            let tokens_over = budget.ceiling_tokens.is_some_and(|c| cumulative_tokens > c);
            exceeded = usd_over || tokens_over;
        }

        if step.kind == "compensation" {
            let for_step = store::get_step(&tx, &lease.run_id, &step.id)?
                .and_then(|r| {
                    r.def
                        .get("for_step")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            run.append(
                Actor::kernel(),
                &CompensationCompleted {
                    step: step.id.clone(),
                    for_step,
                    result: stored_output,
                },
            )?;
        } else {
            let remaining = run
                .state()
                .workflow_steps()
                .any(|s| s.state != StepStatus::Completed);
            if !remaining {
                run.append(
                    Actor::kernel(),
                    &RunCompleted {
                        output: stored_output,
                    },
                )?;
            } else if exceeded && !run.state().budget.exceeded {
                let budget = run.state().budget.clone();
                run.append(
                    Actor::kernel(),
                    &BudgetExceeded {
                        cumulative_usd: budget.ceiling_usd.map(|_| budget.used_usd),
                        ceiling_usd: budget.ceiling_usd,
                        cumulative_tokens: budget.ceiling_tokens.map(|_| budget.used_tokens),
                        ceiling_tokens: budget.ceiling_tokens,
                    },
                )?;
                run.park(
                    "budget",
                    format!("spend {} exceeds the ceiling", budget.used_usd),
                )?;
            }
        }
        let run_state = run.state().state;
        let next_step = run.state().next_step().map(|s| s.id.clone());
        run.persist()?;
        drop(run);
        tx.commit()?;
        tracing::info!(run_id = %lease.run_id, step = %step.id, lease_id, run_state = %run_state.as_str(), "step completed");
        Ok(CompleteResult {
            run_state,
            next_step,
        })
    }

    fn backoff_ms(&self, failed_attempts: u32) -> u64 {
        let exponent = failed_attempts.saturating_sub(1).min(30);
        let full = self
            .config
            .retry_base_ms
            .saturating_mul(1u64 << exponent)
            .min(self.config.retry_cap_ms);
        let half = full / 2;
        half + rand::thread_rng().gen_range(0..=half)
    }

    /// `POST /v1/leases/{id}/fail`.
    pub fn fail(
        &self,
        lease_id: &str,
        error: ErrorInfo,
        deterministic: bool,
    ) -> KernelResult<FailResult> {
        let now_ms = self.now_ms();
        let mut guard = self.lock()?;
        let tx = guard.transaction()?;
        let (lease, mut run) = match self.leased_run(&tx, lease_id, now_ms) {
            Ok(pair) => pair,
            Err(e) => {
                tx.commit()?;
                return Err(e);
            }
        };
        let step = run
            .state()
            .step(&lease.step_id)
            .cloned()
            .ok_or_else(|| internal("leased step missing"))?;
        run.append(
            Actor::worker(lease.worker_id.clone()),
            &StepFailed {
                step: step.id.clone(),
                lease_id: Some(lease.lease_id.clone()),
                attempt: lease.attempt,
                error: error.clone(),
                deterministic,
            },
        )?;
        store::end_lease(&tx, lease_id, "failed", now_ms)?;
        self.metrics
            .step_latency((now_ms - lease.issued_at_ms).max(0) as f64 / 1000.0);
        let attempts = step.attempts;
        let limit = if deterministic {
            self.config.max_attempts_deterministic
        } else {
            self.config.max_attempts_nondeterministic
        };
        let result = if error.code == "model_refused" && step.kind != "compensation" {
            run.park(
                "refusal",
                format!("step {} refused: {}", step.id, error.message),
            )?;
            FailResult {
                outcome: "parked".into(),
                delay_ms: None,
            }
        } else if attempts >= limit {
            run.append(
                Actor::kernel(),
                &StepQuarantined {
                    step: step.id.clone(),
                    reason: format!(
                        "{attempts} {} failures, last: {}",
                        if deterministic {
                            "deterministic"
                        } else {
                            "non-deterministic"
                        },
                        error.code
                    ),
                    attempts,
                },
            )?;
            if step.kind == "compensation" {
                let for_step = store::get_step(&tx, &lease.run_id, &step.id)?
                    .and_then(|r| {
                        r.def
                            .get("for_step")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                run.append(
                    Actor::kernel(),
                    &CompensationFailed {
                        step: step.id.clone(),
                        for_step,
                        error: error.clone(),
                    },
                )?;
                run.append(
                    Actor::kernel(),
                    &RunFailed {
                        error: ErrorInfo {
                            code: "compensation_failed".into(),
                            message: format!("compensation {} failed: {}", step.id, error.message),
                        },
                        needs_human: true,
                    },
                )?;
            } else {
                let reason = if error.code == "connector_quarantined" {
                    "connector_quarantined"
                } else {
                    "quarantine"
                };
                run.park(
                    reason,
                    format!("step {} quarantined after {attempts} attempts", step.id),
                )?;
            }
            FailResult {
                outcome: "quarantined".into(),
                delay_ms: None,
            }
        } else {
            let delay_ms = if deterministic {
                0
            } else {
                self.backoff_ms(attempts)
            };
            run.append(
                Actor::kernel(),
                &StepRetryScheduled {
                    step: step.id.clone(),
                    attempt: attempts + 1,
                    delay_ms,
                },
            )?;
            FailResult {
                outcome: "retry_scheduled".into(),
                delay_ms: Some(delay_ms),
            }
        };
        run.persist()?;
        drop(run);
        tx.commit()?;
        tracing::info!(run_id = %lease.run_id, step = %step.id, lease_id, outcome = %result.outcome, code = %error.code, "step failed");
        Ok(result)
    }

    /// `POST /v1/leases/{id}/actions`.
    pub fn propose_action(&self, lease_id: &str, action: Value) -> KernelResult<ActionOutcome> {
        if !action.is_object() || !action.get("kind").is_some_and(Value::is_string) {
            return Err(invalid("action must be an object with a string kind")
                .with_details(json!({"field": "action.kind"})));
        }
        let now_ms = self.now_ms();
        let mut guard = self.lock()?;
        let tx = guard.transaction()?;
        let (lease, mut run) = match self.leased_run(&tx, lease_id, now_ms) {
            Ok(pair) => pair,
            Err(e) => {
                tx.commit()?;
                return Err(e);
            }
        };
        let step_id = lease.step_id.clone();
        let run_id = lease.run_id.clone();
        let child = store::get_remit(&tx, &run.run.child_remit_id)?
            .ok_or_else(|| internal("run-bound remit missing"))?;
        let action_hash = hash_value(&action);
        let idempotency_key = action
            .get("idempotency_key")
            .and_then(Value::as_str)
            .map(str::to_string);
        let context = policy_context(run.state(), &child.payload, &action);
        let run_ctx = context.get("run").cloned().unwrap_or(Value::Null);

        let approved = store::approved_in_run(&tx, &run_id)?.into_iter().find(|a| {
            match (&idempotency_key, &a.idempotency_key) {
                (Some(k), Some(ak)) => k == ak,
                _ => a.action_hash == action_hash,
            }
        });
        if let Some(approval) = approved {
            let rule = format!("approved:{}", approval.approval_id);
            run.append(
                Actor::worker(lease.worker_id.clone()),
                &crate::events::ActionProposed {
                    action_id: approval.action_id.clone(),
                    step: step_id.clone(),
                    action: action.clone(),
                },
            )?;
            let decided = run.append(
                Actor::policy("approval"),
                &PolicyDecided {
                    action_id: approval.action_id.clone(),
                    decision: "allow".into(),
                    rule: rule.clone(),
                    policy: None,
                    policy_version: Value::Null,
                    approver: Value::Null,
                    sla_seconds: None,
                    escalate_to: Value::Null,
                    policy_set: None,
                },
            )?;
            store::upsert_action(
                &tx,
                &ActionRow {
                    action_id: approval.action_id.clone(),
                    run_id: run_id.clone(),
                    step_id: step_id.clone(),
                    seq: decided.seq,
                    action,
                    action_hash,
                    idempotency_key,
                    context: run_ctx,
                    decision: "allow".into(),
                    rule: rule.clone(),
                    approval_id: Some(approval.approval_id.clone()),
                    created_at: format_ms(now_ms),
                },
            )?;
            run.persist()?;
            drop(run);
            tx.commit()?;
            return Ok(ActionOutcome {
                action_id: approval.action_id,
                decision: DecisionKind::Allow,
                rule,
                approval_id: Some(approval.approval_id),
            });
        }

        let (policies, refs) = load_policy_set(&tx, &child.payload.policy_set)?;
        let decision: Decision = evaluate(&policies, &context);
        let action_id = new_id("act", now_ms);
        run.append(
            Actor::worker(lease.worker_id.clone()),
            &crate::events::ActionProposed {
                action_id: action_id.clone(),
                step: step_id.clone(),
                action: action.clone(),
            },
        )?;
        let policy_actor =
            Actor::policy(decision.policy.clone().unwrap_or_else(|| "default".into()));
        let decided = run.append(
            policy_actor,
            &PolicyDecided {
                action_id: action_id.clone(),
                decision: decision.decision.as_str().into(),
                rule: decision.rule.clone(),
                policy: decision.policy.clone(),
                policy_version: decision
                    .policy_version
                    .as_deref()
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|v| json!(v))
                    .unwrap_or(Value::Null),
                approver: serde_json::to_value(&decision.approver)?,
                sla_seconds: decision.sla_seconds,
                escalate_to: serde_json::to_value(&decision.escalate_to)?,
                policy_set: Some(refs),
            },
        )?;
        let mut approval_id = None;
        if decision.decision == DecisionKind::ApprovalRequired {
            let approver = decision
                .approver
                .clone()
                .unwrap_or_else(Approver::admin_fallback);
            let sla = decision
                .sla_seconds
                .unwrap_or(kernos_policy::DEFAULT_SLA_SECONDS);
            let escalate_to = decision
                .escalate_to
                .clone()
                .unwrap_or_else(EscalateTo::reporting_line);
            let id = new_id("apr", now_ms);
            let due_at_ms = now_ms + sla as i64 * 1000;
            run.append(
                Actor::kernel(),
                &ApprovalRequested {
                    approval_id: id.clone(),
                    action_id: action_id.clone(),
                    approver: serde_json::to_value(&approver)?,
                    sla_seconds: sla,
                    escalate_to: serde_json::to_value(&escalate_to)?,
                    due_at: format_ms(due_at_ms),
                },
            )?;
            run.append(
                Actor::kernel(),
                &StepWaitingApproval {
                    step: step_id.clone(),
                    action_id: action_id.clone(),
                    approval_id: id.clone(),
                },
            )?;
            run.park(
                "approval",
                format!("approval {id} pending for step {step_id}"),
            )?;
            store::end_lease(&tx, lease_id, "released", now_ms)?;
            store::insert_approval(
                &tx,
                &ApprovalRow {
                    approval_id: id.clone(),
                    run_id: run_id.clone(),
                    action_id: action_id.clone(),
                    step_id: step_id.clone(),
                    action: action.clone(),
                    action_hash: action_hash.clone(),
                    idempotency_key: idempotency_key.clone(),
                    approver: approver.clone(),
                    original_approver: approver,
                    escalate_to: serde_json::to_value(&escalate_to)?,
                    sla_seconds: sla,
                    state: "pending".into(),
                    requested_at: format_ms(now_ms),
                    due_at_ms,
                    escalations: 0,
                    parked_human: false,
                    decided_at: None,
                    actor: None,
                    reason: None,
                    decision: None,
                },
            )?;
            approval_id = Some(id);
        }
        store::upsert_action(
            &tx,
            &ActionRow {
                action_id: action_id.clone(),
                run_id: run_id.clone(),
                step_id: step_id.clone(),
                seq: decided.seq,
                action,
                action_hash,
                idempotency_key,
                context: run_ctx,
                decision: decision.decision.as_str().into(),
                rule: decision.rule.clone(),
                approval_id: approval_id.clone(),
                created_at: format_ms(now_ms),
            },
        )?;
        run.persist()?;
        drop(run);
        tx.commit()?;
        tracing::info!(run_id = %run_id, step = %step_id, lease_id, action_id = %action_id, decision = decision.decision.as_str(), rule = %decision.rule, "action decided");
        Ok(ActionOutcome {
            action_id,
            decision: decision.decision,
            rule: decision.rule,
            approval_id,
        })
    }

    // ------------------------------------------------------------ approvals

    /// `GET /v1/approvals`.
    pub fn list_approvals(
        &self,
        state: Option<&str>,
        approver: Option<&str>,
    ) -> KernelResult<Vec<Value>> {
        let rows = self.with_store(|c| store::list_approvals(c, state))?;
        let wanted = approver.map(parse_approver_query);
        Ok(rows
            .iter()
            .filter(|a| match &wanted {
                None => true,
                Some(w) => {
                    approver_matches(w, &a.approver) || approver_matches(w, &a.original_approver)
                }
            })
            .map(approval_json)
            .collect())
    }

    /// `GET /v1/approvals/{id}`.
    pub fn get_approval(&self, approval_id: &str) -> KernelResult<Value> {
        let row = self
            .with_store(|c| store::get_approval(c, approval_id))?
            .ok_or_else(|| {
                KernelError::not_found(
                    "approval_not_found",
                    format!("approval {approval_id} does not exist"),
                )
            })?;
        Ok(approval_json(&row))
    }

    /// `POST /v1/approvals/{id}`.
    pub fn decide_approval(
        &self,
        approval_id: &str,
        decision: &str,
        actor: &DecisionActor,
        reason: &str,
    ) -> KernelResult<ApprovalOutcome> {
        if !matches!(decision, "approved" | "rejected") {
            return Err(invalid("decision is approved or rejected")
                .with_details(json!({"field": "decision"})));
        }
        if reason.trim().chars().count() < 3 {
            return Err(KernelError::unprocessable(
                "reason_required",
                "a reason of at least 3 characters is required",
            )
            .with_details(json!({"field": "reason"})));
        }
        if actor.id.is_empty() {
            return Err(invalid("actor.id is required").with_details(json!({"field": "actor.id"})));
        }
        let now_ms = self.now_ms();
        let mut guard = self.lock()?;
        let tx = guard.transaction()?;
        let approval = store::get_approval(&tx, approval_id)?.ok_or_else(|| {
            KernelError::not_found(
                "approval_not_found",
                format!("approval {approval_id} does not exist"),
            )
        })?;
        if approval.state != "pending" {
            return Err(KernelError::conflict(
                "already_decided",
                format!("approval {approval_id} is already {}", approval.state),
            ));
        }
        let accepted = approval.approver.accepts(&actor.id, &actor.role)
            || approval.original_approver.accepts(&actor.id, &actor.role);
        if !accepted {
            return Err(KernelError::forbidden(
                "not_the_approver",
                format!(
                    "approval {approval_id} needs {}, not {} ({})",
                    approval.approver.to_query_string(),
                    actor.id,
                    actor.role
                ),
            ));
        }
        let mut run = RunTx::load(&tx, &self.metrics, now_ms, &approval.run_id)?;
        store::decide_approval(
            &tx,
            approval_id,
            decision,
            &serde_json::to_value(actor)?,
            reason,
            &format_ms(now_ms),
        )?;
        run.append(
            Actor::user(actor.id.clone()),
            &ApprovalDecided {
                approval_id: approval_id.to_string(),
                action_id: approval.action_id.clone(),
                decision: decision.to_string(),
                actor: actor.clone(),
                reason: reason.to_string(),
            },
        )?;
        let step = run.state().step(&approval.step_id).cloned();
        if decision == "approved" {
            if run.state().state == RunStatus::Parked {
                run.append(
                    Actor::kernel(),
                    &RunResumed {
                        reason: "approval".into(),
                        actor: None,
                    },
                )?;
            }
            if let Some(step) = step {
                run.append(
                    Actor::kernel(),
                    &StepScheduled {
                        step: step.id,
                        index: step.index,
                        kind: step.kind,
                    },
                )?;
            }
        } else {
            let error = ErrorInfo {
                code: "action_rejected".into(),
                message: format!("{} rejected: {reason}", actor.id),
            };
            if let Some(step) = step {
                run.append(
                    Actor::kernel(),
                    &StepFailed {
                        step: step.id,
                        lease_id: None,
                        attempt: step.attempts,
                        error: error.clone(),
                        deterministic: true,
                    },
                )?;
            }
            run.append(
                Actor::kernel(),
                &RunFailed {
                    error,
                    needs_human: false,
                },
            )?;
        }
        let run_state = run.state().state;
        run.persist()?;
        drop(run);
        tx.commit()?;
        tracing::info!(run_id = %approval.run_id, approval_id, decision, actor = %actor.id, "approval decided");
        Ok(ApprovalOutcome {
            run_id: approval.run_id,
            run_state,
        })
    }

    // ------------------------------------------------------------ sweepers

    /// Expires overdue leases; returns how many.
    pub fn sweep_leases(&self) -> KernelResult<usize> {
        let now_ms = self.now_ms();
        let mut guard = self.lock()?;
        let tx = guard.transaction()?;
        let expired = store::expired_leases(&tx, now_ms)?;
        for lease in &expired {
            self.expire_lease(&tx, lease, now_ms)?;
        }
        tx.commit()?;
        Ok(expired.len())
    }

    /// Escalates overdue approvals once, then parks the run for a human;
    /// returns how many approvals were touched.
    pub fn sweep_approvals(&self) -> KernelResult<usize> {
        let now_ms = self.now_ms();
        let mut guard = self.lock()?;
        let tx = guard.transaction()?;
        let overdue = store::overdue_approvals(&tx, now_ms)?;
        for approval in &overdue {
            let mut run = RunTx::load(&tx, &self.metrics, now_ms, &approval.run_id)?;
            if approval.escalations == 0 {
                let target: EscalateTo = serde_json::from_value(approval.escalate_to.clone())
                    .unwrap_or_else(|_| EscalateTo::reporting_line());
                let to = self
                    .directory
                    .resolve_escalation(&target, &approval.approver);
                let due_at_ms = now_ms + approval.sla_seconds as i64 * 1000;
                run.append(
                    Actor::system("approval-sweeper"),
                    &ApprovalEscalated {
                        approval_id: approval.approval_id.clone(),
                        from: serde_json::to_value(&approval.approver)?,
                        to: serde_json::to_value(&to)?,
                        reason: "sla_expired".into(),
                        due_at: Some(format_ms(due_at_ms)),
                    },
                )?;
                store::escalate_approval(&tx, &approval.approval_id, &to, due_at_ms)?;
                tracing::info!(run_id = %approval.run_id, approval_id = %approval.approval_id, to = %to.to_query_string(), "approval escalated");
            } else {
                run.append(
                    Actor::system("approval-sweeper"),
                    &RunParked {
                        reason: "human".into(),
                        detail: format!(
                            "approval {} unanswered after escalation to {}",
                            approval.approval_id,
                            approval.approver.to_query_string()
                        ),
                    },
                )?;
                store::park_approval(&tx, &approval.approval_id)?;
                tracing::warn!(run_id = %approval.run_id, approval_id = %approval.approval_id, "approval unanswered twice, parked for a human");
            }
            run.persist()?;
        }
        tx.commit()?;
        Ok(overdue.len())
    }
}

fn policy_summary(row: &PolicyRow) -> Value {
    json!({"policy_id": row.policy_id, "name": row.name, "version": row.version, "created_at": row.created_at})
}

fn approval_json(a: &ApprovalRow) -> Value {
    json!({
        "approval_id": a.approval_id, "run_id": a.run_id, "action_id": a.action_id, "step": a.step_id,
        "action": a.action, "approver": a.approver, "original_approver": a.original_approver,
        "escalate_to": a.escalate_to, "sla_seconds": a.sla_seconds, "state": a.state,
        "requested_at": a.requested_at, "due_at": format_ms(a.due_at_ms), "escalations": a.escalations,
        "decision": a.decision, "actor": a.actor, "reason": a.reason, "decided_at": a.decided_at,
    })
}

fn parse_approver_query(text: &str) -> Approver {
    match text.split_once(':') {
        Some(("role", value)) => Approver::role(value),
        Some(("user", value)) => Approver::user(value),
        _ => Approver::user(text),
    }
}

fn approver_matches(wanted: &Approver, actual: &Approver) -> bool {
    wanted.kind == actual.kind && wanted.value == actual.value
}

fn validate_patterns(patterns: &[String], field: &str) -> KernelResult<()> {
    for p in patterns {
        let body = p.strip_suffix('*').unwrap_or(p);
        if p.is_empty() || body.contains('*') || p.contains(char::is_whitespace) {
            return Err(invalid(format!(
                "{field} pattern {p:?} is not a tool id, scope or prefix glob"
            ))
            .with_details(json!({"field": field})));
        }
    }
    Ok(())
}

fn validate_requested_by(value: &Value) -> KernelResult<()> {
    if !value
        .get("id")
        .is_some_and(|id| id.as_str().is_some_and(|s| !s.is_empty()))
    {
        return Err(invalid("requested_by needs a string id")
            .with_details(json!({"field": "requested_by.id"})));
    }
    Ok(())
}

/// Lists the `*.pub` files of the trusted directory with their size and
/// modification time, sorted, so two readings can be compared cheaply. A
/// directory that cannot be read fingerprints as empty, which means an empty
/// trusted set rather than a stale one.
fn trust_fingerprint(dir: &Path) -> TrustFingerprint {
    let mut out = TrustFingerprint::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pub") {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let (modified, len) = match entry.metadata() {
            Ok(meta) => {
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                (modified, meta.len())
            }
            Err(_) => (0, 0),
        };
        out.push((name, modified, len));
    }
    out.sort();
    out
}

/// The templating context of 05-BUNDLE for a run.
pub fn template_context(state: &RunState) -> Value {
    let mut steps = serde_json::Map::new();
    for s in &state.steps {
        if let Some(output) = &s.output {
            steps.insert(s.id.clone(), json!({"output": output}));
        }
    }
    json!({
        "input": state.input,
        "steps": steps,
        "run": {"id": state.run_id, "workflow": state.workflow, "department": state.department, "requested_by": state.requested_by},
    })
}

fn compensation_step_id(state: &RunState, for_step: &str) -> String {
    let base = format!("comp_{for_step}");
    if state.step(&base).is_none() {
        return base;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}_{n}");
        if state.step(&candidate).is_none() {
            return candidate;
        }
        n += 1;
    }
}

/// Decodes a token without verifying it, for diagnostics in the CLI.
pub fn peek_token(token: &str) -> Result<RemitPayload, TokenError> {
    decode_token(token).map(|d| d.payload)
}
