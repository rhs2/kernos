//! The HTTP server: every endpoint of 02-KERNEL-API on one axum router, the
//! sweepers as background tasks, bearer authentication and Prometheus metrics.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use kernos_core::bundle::BundleSignature;
use kernos_core::clock::SystemClock;
use kernos_core::events::{DecisionActor, ErrorInfo};
use kernos_core::kernel::{ExternalAuth, LeaseRequest, PolicySelector, StartRunRequest, Usage};
use kernos_core::remit::{DeriveRequest, IssueRequest};
use kernos_core::store::RunFilter;
use kernos_core::time::{format_ms, parse_rfc3339};
use kernos_core::{Kernel, KernelError, KernelResult};
use kernos_policy::DecisionKind;

use crate::config::Config;

/// Shared state of every handler.
#[derive(Clone)]
pub struct AppState {
    /// The kernel.
    pub kernel: Arc<Kernel>,
    /// The bearer token, when configured.
    pub token: Option<String>,
}

/// A handler error: the kernel error rendered in the wire shape.
#[derive(Debug)]
pub struct ApiError(pub KernelError);

impl From<KernelError> for ApiError {
    fn from(e: KernelError) -> Self {
        ApiError(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        if status.is_server_error() {
            tracing::error!(error = %self.0, "internal error");
        }
        (status, Json(self.0.to_json())).into_response()
    }
}

/// A JSON body extractor whose rejection is the spec error shape
/// (`400 invalid_json`) instead of axum's plain text.
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(ApiError(KernelError::bad_request(
                "invalid_json",
                format!(
                    "request body is not the expected JSON: {}",
                    rejection.body_text()
                ),
            ))),
        }
    }
}

type ApiResult = Result<Response, ApiError>;

async fn blocking<T: Send + 'static>(
    kernel: Arc<Kernel>,
    f: impl FnOnce(&Kernel) -> KernelResult<T> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(move || f(&kernel))
        .await
        .map_err(|e| {
            ApiError(KernelError::api(
                500,
                "internal",
                format!("task failed: {e}"),
            ))
        })?
        .map_err(ApiError)
}

fn reply(status: StatusCode, value: impl serde::Serialize) -> ApiResult {
    Ok((status, Json(value)).into_response())
}

// ---------------------------------------------------------------- middleware

async fn auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Some(token) = &state.token {
        let presented = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim);
        if presented != Some(token.as_str()) {
            return ApiError(KernelError::api(
                401,
                "unauthorized",
                "a valid bearer token is required",
            ))
            .into_response();
        }
    }
    next.run(request).await
}

async fn log_requests(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let started = Instant::now();
    let response = next.run(request).await;
    tracing::info!(
        method = %method,
        path = %path,
        status = response.status().as_u16(),
        latency_ms = started.elapsed().as_millis() as u64,
        "request"
    );
    response
}

async fn not_found() -> Response {
    ApiError(KernelError::not_found("not_found", "no such route")).into_response()
}

// ---------------------------------------------------------------- health, keys, metrics

async fn health(State(state): State<AppState>) -> ApiResult {
    let report = blocking(state.kernel, |k| k.health()).await?;
    reply(StatusCode::OK, report)
}

async fn keys(State(state): State<AppState>) -> ApiResult {
    reply(StatusCode::OK, state.kernel.public_key_info())
}

async fn metrics(State(state): State<AppState>) -> ApiResult {
    let text = blocking(state.kernel, |k| k.metrics_text()).await?;
    Ok((
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        text,
    )
        .into_response())
}

// ---------------------------------------------------------------- bundles

#[derive(Deserialize)]
struct BundleBody {
    bundle: Value,
    signature: BundleSignature,
}

async fn apply_bundle(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<BundleBody>,
) -> ApiResult {
    let applied = blocking(state.kernel, move |k| {
        k.apply_bundle(body.bundle, body.signature)
    })
    .await?;
    let status = if applied.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    reply(status, applied)
}

async fn list_bundles(State(state): State<AppState>) -> ApiResult {
    reply(
        StatusCode::OK,
        blocking(state.kernel, |k| k.list_bundles()).await?,
    )
}

async fn get_bundle(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    reply(
        StatusCode::OK,
        blocking(state.kernel, move |k| k.get_bundle(&id)).await?,
    )
}

// ---------------------------------------------------------------- policies

#[derive(Deserialize)]
struct PolicyBody {
    name: String,
    version: u64,
    source: String,
}

async fn apply_policy(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<PolicyBody>,
) -> ApiResult {
    let applied = blocking(state.kernel, move |k| {
        k.apply_policy(&body.name, body.version, &body.source)
    })
    .await?;
    let status = if applied.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    reply(status, applied)
}

async fn list_policies(State(state): State<AppState>) -> ApiResult {
    reply(
        StatusCode::OK,
        blocking(state.kernel, |k| k.list_policies()).await?,
    )
}

async fn policy_versions(State(state): State<AppState>, Path(name): Path<String>) -> ApiResult {
    reply(
        StatusCode::OK,
        blocking(state.kernel, move |k| k.policy_versions(&name)).await?,
    )
}

async fn policy_source(
    State(state): State<AppState>,
    Path((name, version)): Path<(String, u64)>,
) -> ApiResult {
    reply(
        StatusCode::OK,
        blocking(state.kernel, move |k| k.policy_source(&name, version)).await?,
    )
}

#[derive(Deserialize)]
struct PolicyTestBody {
    policy_a: Value,
    policy_b: Value,
    #[serde(default)]
    corpus: Vec<Value>,
}

fn selector(value: &Value, field: &str) -> Result<PolicySelector, ApiError> {
    if let Some(source) = value.get("source").and_then(Value::as_str) {
        return Ok(PolicySelector::Inline {
            source: source.to_string(),
        });
    }
    let name = value.get("name").and_then(Value::as_str);
    let version = value.get("version").and_then(Value::as_u64);
    match (name, version) {
        (Some(name), Some(version)) => Ok(PolicySelector::Stored {
            name: name.to_string(),
            version,
        }),
        _ => Err(ApiError(
            KernelError::unprocessable(
                "invalid_request",
                format!("{field} must be {{name, version}} or {{source}}"),
            )
            .with_details(json!({"field": field})),
        )),
    }
}

async fn test_policies(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<PolicyTestBody>,
) -> ApiResult {
    let a = selector(&body.policy_a, "policy_a")?;
    let b = selector(&body.policy_b, "policy_b")?;
    let corpus = body.corpus;
    reply(
        StatusCode::OK,
        blocking(state.kernel, move |k| k.test_policies(&a, &b, &corpus)).await?,
    )
}

// ---------------------------------------------------------------- remits

async fn issue_remit(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<IssueRequest>,
) -> ApiResult {
    reply(
        StatusCode::CREATED,
        blocking(state.kernel, move |k| k.issue_remit(&body)).await?,
    )
}

async fn derive_remit(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<DeriveRequest>,
) -> ApiResult {
    reply(
        StatusCode::CREATED,
        blocking(state.kernel, move |k| k.derive_remit(&id, &body)).await?,
    )
}

async fn get_remit(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    reply(
        StatusCode::OK,
        blocking(state.kernel, move |k| k.get_remit(&id)).await?,
    )
}

// ---------------------------------------------------------------- runs

async fn start_run(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<StartRunRequest>,
) -> ApiResult {
    reply(
        StatusCode::CREATED,
        blocking(state.kernel, move |k| k.start_run(&body)).await?,
    )
}

#[derive(Deserialize)]
struct RunsQuery {
    state: Option<String>,
    department: Option<String>,
    limit: Option<u64>,
    after: Option<String>,
}

async fn list_runs(State(state): State<AppState>, Query(query): Query<RunsQuery>) -> ApiResult {
    let filter = RunFilter {
        state: query.state,
        department: query.department,
        limit: query.limit.unwrap_or(50).clamp(1, 500),
        after: query.after,
    };
    reply(
        StatusCode::OK,
        blocking(state.kernel, move |k| k.list_runs(&filter)).await?,
    )
}

async fn get_run(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    reply(
        StatusCode::OK,
        blocking(state.kernel, move |k| k.get_run(&id)).await?,
    )
}

#[derive(Deserialize)]
struct EventsQuery {
    from_seq: Option<u64>,
    limit: Option<u64>,
}

async fn run_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> ApiResult {
    let from = query.from_seq.unwrap_or(1);
    let limit = query.limit.unwrap_or(500);
    reply(
        StatusCode::OK,
        blocking(state.kernel, move |k| k.run_events(&id, from, limit)).await?,
    )
}

#[derive(Deserialize)]
struct ExternalEventBody {
    kind: String,
    #[serde(default)]
    payload: Value,
    actor: Value,
}

async fn post_event(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<ExternalEventBody>,
) -> ApiResult {
    let header_value = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
    };
    let auth = match (
        header_value("x-kernos-lease"),
        header_value("x-kernos-remit"),
    ) {
        (Some(lease), _) => ExternalAuth::Lease(lease),
        (None, Some(token)) => ExternalAuth::Remit(token),
        (None, None) => ExternalAuth::None,
    };
    let appended = blocking(state.kernel, move |k| {
        k.post_external_event(&id, &body.kind, body.payload, body.actor, &auth)
    })
    .await?;
    reply(StatusCode::CREATED, appended)
}

async fn replay(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    reply(
        StatusCode::OK,
        blocking(state.kernel, move |k| k.replay(&id)).await?,
    )
}

#[derive(Deserialize)]
struct AbandonBody {
    reason: String,
    #[serde(default)]
    actor: Value,
}

async fn abandon(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<AbandonBody>,
) -> ApiResult {
    let n = blocking(state.kernel, move |k| {
        k.abandon(&id, &body.reason, body.actor)
    })
    .await?;
    reply(StatusCode::ACCEPTED, json!({"compensations_scheduled": n}))
}

#[derive(Deserialize)]
struct ResumeBody {
    #[serde(default)]
    actor: Value,
}

async fn resume(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<ResumeBody>,
) -> ApiResult {
    let run_id = id.clone();
    let run_state = blocking(state.kernel, move |k| k.resume(&id, body.actor)).await?;
    reply(
        StatusCode::OK,
        json!({"run_id": run_id, "run_state": run_state}),
    )
}

#[derive(Deserialize)]
struct ActionsQuery {
    since: Option<String>,
}

async fn export_actions(
    State(state): State<AppState>,
    Query(query): Query<ActionsQuery>,
) -> ApiResult {
    let now = state.kernel.now_ms();
    let since_ms = match query.since.as_deref() {
        None => 0,
        Some(text) => match parse_rfc3339(text) {
            Some(ms) => ms,
            None => match kernos_policy::parse_duration(text) {
                Some(seconds) => now - seconds as i64 * 1000,
                None => {
                    return Err(ApiError(
                        KernelError::unprocessable(
                            "invalid_request",
                            "since must be RFC 3339 or a duration such as 30d",
                        )
                        .with_details(json!({"field": "since"})),
                    ))
                }
            },
        },
    };
    reply(
        StatusCode::OK,
        blocking(state.kernel, move |k| k.export_actions(since_ms)).await?,
    )
}

// ---------------------------------------------------------------- leases

async fn lease(State(state): State<AppState>, ApiJson(body): ApiJson<LeaseRequest>) -> ApiResult {
    match blocking(state.kernel, move |k| k.lease(&body)).await? {
        Some(grant) => reply(StatusCode::OK, grant),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

async fn heartbeat(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let expires_at = blocking(state.kernel, move |k| k.heartbeat(&id)).await?;
    reply(StatusCode::OK, json!({"expires_at": expires_at}))
}

#[derive(Deserialize)]
struct CompleteBody {
    #[serde(default)]
    output: Value,
    #[serde(default)]
    usage: Option<Usage>,
}

async fn complete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CompleteBody>,
) -> ApiResult {
    reply(
        StatusCode::OK,
        blocking(state.kernel, move |k| {
            k.complete(&id, body.output, body.usage)
        })
        .await?,
    )
}

#[derive(Deserialize)]
struct FailBody {
    error: ErrorInfo,
    #[serde(default)]
    deterministic: bool,
}

async fn fail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<FailBody>,
) -> ApiResult {
    reply(
        StatusCode::OK,
        blocking(state.kernel, move |k| {
            k.fail(&id, body.error, body.deterministic)
        })
        .await?,
    )
}

#[derive(Deserialize)]
struct ActionBody {
    action: Value,
}

async fn propose_action(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<ActionBody>,
) -> ApiResult {
    let outcome = blocking(state.kernel, move |k| k.propose_action(&id, body.action)).await?;
    if outcome.decision == DecisionKind::Deny {
        return Err(ApiError(
            KernelError::forbidden(
                "action_denied",
                format!("policy denied the action by rule {}", outcome.rule),
            )
            .with_details(
                json!({"action_id": outcome.action_id, "decision": "deny", "rule": outcome.rule}),
            ),
        ));
    }
    reply(StatusCode::OK, outcome)
}

// ---------------------------------------------------------------- approvals

#[derive(Deserialize)]
struct ApprovalsQuery {
    state: Option<String>,
    approver: Option<String>,
}

async fn list_approvals(
    State(state): State<AppState>,
    Query(query): Query<ApprovalsQuery>,
) -> ApiResult {
    reply(
        StatusCode::OK,
        blocking(state.kernel, move |k| {
            k.list_approvals(query.state.as_deref(), query.approver.as_deref())
        })
        .await?,
    )
}

async fn get_approval(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    reply(
        StatusCode::OK,
        blocking(state.kernel, move |k| k.get_approval(&id)).await?,
    )
}

#[derive(Deserialize)]
struct DecideBody {
    decision: String,
    actor: DecisionActor,
    #[serde(default)]
    reason: String,
}

async fn decide(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<DecideBody>,
) -> ApiResult {
    reply(
        StatusCode::OK,
        blocking(state.kernel, move |k| {
            k.decide_approval(&id, &body.decision, &body.actor, &body.reason)
        })
        .await?,
    )
}

/// Builds the router with every route of 02-KERNEL-API.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/keys", get(keys))
        .route("/v1/metrics", get(metrics))
        .route("/v1/bundles", post(apply_bundle).get(list_bundles))
        .route("/v1/bundles/{id}", get(get_bundle))
        .route("/v1/policies", post(apply_policy).get(list_policies))
        .route("/v1/policies/test", post(test_policies))
        .route("/v1/policies/{name}", get(policy_versions))
        .route("/v1/policies/{name}/{version}", get(policy_source))
        .route("/v1/remits", post(issue_remit))
        .route("/v1/remits/{id}", get(get_remit))
        .route("/v1/remits/{id}/derive", post(derive_remit))
        .route("/v1/runs", post(start_run).get(list_runs))
        .route("/v1/runs/{id}", get(get_run))
        .route("/v1/runs/{id}/events", get(run_events).post(post_event))
        .route("/v1/runs/{id}/replay", post(replay))
        .route("/v1/runs/{id}/abandon", post(abandon))
        .route("/v1/runs/{id}/resume", post(resume))
        .route("/v1/actions", get(export_actions))
        .route("/v1/leases", post(lease))
        .route("/v1/leases/{id}/heartbeat", post(heartbeat))
        .route("/v1/leases/{id}/complete", post(complete))
        .route("/v1/leases/{id}/fail", post(fail))
        .route("/v1/leases/{id}/actions", post(propose_action))
        .route("/v1/approvals", get(list_approvals))
        .route("/v1/approvals/{id}", get(get_approval).post(decide))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .layer(middleware::from_fn(log_requests))
        .with_state(state)
}

/// A failure to start the server.
#[derive(Debug, Error)]
pub enum ServerError {
    /// The kernel could not open its data directory.
    #[error("kernel: {0}")]
    Kernel(#[from] KernelError),
    /// The listener could not bind.
    #[error("cannot listen on {listen}: {source}")]
    Bind {
        /// The address.
        listen: String,
        /// The cause.
        source: std::io::Error,
    },
}

/// A running server: the bound address, the kernel, and a handle to stop it.
pub struct RunningServer {
    addr: SocketAddr,
    kernel: Arc<Kernel>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl RunningServer {
    /// The bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://host:port`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The kernel behind the server.
    pub fn kernel(&self) -> Arc<Kernel> {
        self.kernel.clone()
    }

    /// Stops the listener and the sweepers and waits for them.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        for task in self.tasks {
            let _ = task.await;
        }
    }
}

/// Opens the kernel, binds the listener and spawns the server and the sweepers.
pub async fn start(config: &Config) -> Result<RunningServer, ServerError> {
    let kernel = Arc::new(Kernel::open(
        &config.data_dir,
        config.kernel_config(),
        Arc::new(SystemClock),
    )?);
    let listener = TcpListener::bind(&config.listen)
        .await
        .map_err(|source| ServerError::Bind {
            listen: config.listen.clone(),
            source,
        })?;
    let addr = listener.local_addr().map_err(|source| ServerError::Bind {
        listen: config.listen.clone(),
        source,
    })?;
    let (shutdown, rx) = watch::channel(false);
    let state = AppState {
        kernel: kernel.clone(),
        token: config.token.clone(),
    };
    let app = router(state);
    let mut rx_http = rx.clone();
    let http = tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx_http.changed().await;
            })
            .await;
        if let Err(e) = result {
            tracing::error!(error = %e, "http server stopped");
        }
    });
    let lease_sweeper = spawn_sweeper(
        kernel.clone(),
        rx.clone(),
        Duration::from_millis(config.lease_sweep_interval_ms.max(10)),
        "lease-sweeper",
        |k| k.sweep_leases(),
    );
    let approval_sweeper = spawn_sweeper(
        kernel.clone(),
        rx,
        Duration::from_millis(config.approval_sweep_interval_ms.max(10)),
        "approval-sweeper",
        |k| k.sweep_approvals(),
    );
    tracing::info!(listen = %addr, data_dir = %config.data_dir.display(), auth = config.token.is_some(), "kernos listening");
    Ok(RunningServer {
        addr,
        kernel,
        shutdown,
        tasks: vec![http, lease_sweeper, approval_sweeper],
    })
}

fn spawn_sweeper(
    kernel: Arc<Kernel>,
    mut stop: watch::Receiver<bool>,
    every: Duration,
    name: &'static str,
    sweep: fn(&Kernel) -> KernelResult<usize>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let k = kernel.clone();
                    match tokio::task::spawn_blocking(move || sweep(&k)).await {
                        Ok(Ok(n)) if n > 0 => tracing::info!(sweeper = name, swept = n, "sweep"),
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => tracing::error!(sweeper = name, error = %e, "sweep failed"),
                        Err(e) => tracing::error!(sweeper = name, error = %e, "sweep task failed"),
                    }
                }
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

/// Installs the global tracing subscriber in the configured format. Safe to call
/// more than once: later calls are ignored.
pub fn init_logging(format: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_env("KERNOS_LOG_LEVEL")
        .or_else(|_| tracing_subscriber::EnvFilter::try_from_env("RUST_LOG"))
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let result = if format == "json" {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .with_current_span(false)
            .try_init()
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).try_init()
    };
    let _ = result;
}

/// Runs `kernos serve` until interrupted.
pub fn serve_until_interrupted(config: Config) -> Result<(), ServerError> {
    let runtime = tokio::runtime::Runtime::new().map_err(|source| ServerError::Bind {
        listen: config.listen.clone(),
        source,
    })?;
    runtime.block_on(async move {
        let server = start(&config).await?;
        println!(
            "kernos listening on {} (data {})",
            server.base_url(),
            config.data_dir.display()
        );
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down");
        server.shutdown().await;
        Ok(())
    })
}

/// A timestamp helper re-exported for the CLI.
pub fn now_text(kernel: &Kernel) -> String {
    format_ms(kernel.now_ms())
}
