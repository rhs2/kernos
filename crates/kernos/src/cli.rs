//! The `kernos` command line of 02-KERNEL-API.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};

use kernos_core::bundle::sign_bundle;
use kernos_core::keys::{KeyPair, PublicKey};
use kernos_core::time::system_now_ms;

use crate::client::{Client, ClientError};
use crate::config::Config;
use crate::server;

/// Exit code for a failed command.
pub const EXIT_ERROR: i32 = 1;
/// Exit code for a usage or configuration problem.
pub const EXIT_USAGE: i32 = 2;

/// The `kernos` command line.
#[derive(Parser, Debug)]
#[command(name = "kernos", version, about = "Kernos kernel and control plane", long_about = None)]
pub struct Cli {
    /// Kernel URL for the remote commands.
    #[arg(
        long,
        global = true,
        env = "KERNOS_SERVER",
        default_value = "http://127.0.0.1:7401"
    )]
    pub server: String,
    /// Bearer token for the remote commands.
    #[arg(long, global = true, env = "KERNOS_TOKEN")]
    pub token: Option<String>,
    /// Print JSON instead of a readable rendering.
    #[arg(long, global = true)]
    pub json: bool,
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the kernel and control plane.
    Serve(ServeArgs),
    /// Ask a running kernel whether it is healthy.
    Health,
    /// Publisher keys.
    Keys {
        /// Key subcommand.
        #[command(subcommand)]
        command: KeysCommand,
    },
    /// Bundles.
    Bundle {
        /// Bundle subcommand.
        #[command(subcommand)]
        command: BundleCommand,
    },
    /// Policies.
    Policy {
        /// Policy subcommand.
        #[command(subcommand)]
        command: PolicyCommand,
    },
    /// Remits.
    Remit {
        /// Remit subcommand.
        #[command(subcommand)]
        command: RemitCommand,
    },
    /// Runs.
    Run {
        /// Run subcommand.
        #[command(subcommand)]
        command: RunCommand,
    },
    /// Approvals.
    Approvals {
        /// Approvals subcommand.
        #[command(subcommand)]
        command: ApprovalsCommand,
    },
}

/// `kernos serve`.
#[derive(Args, Debug)]
pub struct ServeArgs {
    /// Listen address.
    #[arg(long)]
    pub listen: Option<String>,
    /// Data directory.
    #[arg(long)]
    pub data: Option<PathBuf>,
    /// Configuration file.
    #[arg(long)]
    pub config: Option<PathBuf>,
}

/// `kernos keys ...`.
#[derive(Subcommand, Debug)]
pub enum KeysCommand {
    /// Generate an Ed25519 pair: `<out>.key` (0600) and `<out>.pub`.
    Generate {
        /// Base path of the two files.
        #[arg(long)]
        out: PathBuf,
    },
    /// Copy a public key into `KERNOS_DATA/keys/trusted`.
    Trust {
        /// The `.pub` file.
        file: PathBuf,
        /// Data directory (default `KERNOS_DATA` or `./kernos-data`).
        #[arg(long, env = "KERNOS_DATA")]
        data: Option<PathBuf>,
    },
}

/// `kernos bundle ...`.
#[derive(Subcommand, Debug)]
pub enum BundleCommand {
    /// Sign a bundle with a publisher key.
    Sign {
        /// The bundle JSON file.
        file: PathBuf,
        /// The `.key` file.
        #[arg(long)]
        key: PathBuf,
        /// Where to write the signature (default `<file>.sig.json`).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Validate a bundle offline, before signing it.
    Validate {
        /// The bundle JSON file.
        file: PathBuf,
    },
    /// Apply a signed bundle to the control plane.
    Apply {
        /// The bundle JSON file.
        file: PathBuf,
        /// The signature file (default `<file>.sig.json`).
        #[arg(long)]
        sig: Option<PathBuf>,
    },
    /// List bundles.
    List,
    /// Show one bundle.
    Show {
        /// Bundle id.
        id: String,
    },
}

/// `kernos policy ...`.
#[derive(Subcommand, Debug)]
pub enum PolicyCommand {
    /// Apply a policy version from a file.
    Apply {
        /// The policy text file.
        file: PathBuf,
        /// Policy name.
        #[arg(long)]
        name: String,
        /// Policy version.
        #[arg(long)]
        version: u64,
    },
    /// Parse a policy offline, before applying it.
    Check {
        /// The policy text file.
        file: PathBuf,
    },
    /// Report the decisions that flip between two policies over a corpus.
    Test {
        /// `name@version`.
        #[arg(long)]
        a: String,
        /// `name@version` or a policy file.
        #[arg(long)]
        b: String,
        /// A JSON Lines (or JSON array) file of `{action, run}` contexts.
        #[arg(long)]
        corpus: PathBuf,
    },
    /// List policies.
    List,
    /// Show a policy version's source.
    Show {
        /// `name@version`.
        spec: String,
    },
}

/// `kernos remit ...`.
#[derive(Subcommand, Debug)]
pub enum RemitCommand {
    /// Issue a remit.
    Issue(RemitIssueArgs),
    /// Derive a narrower remit.
    Derive {
        /// Parent remit id.
        id: String,
        /// Narrowing fields as a JSON object.
        #[arg(long)]
        body: String,
    },
    /// Show a remit.
    Show {
        /// Remit id.
        id: String,
    },
}

/// `kernos remit issue`.
#[derive(Args, Debug)]
pub struct RemitIssueArgs {
    /// Tool patterns, comma separated or repeated.
    #[arg(long, value_delimiter = ',', required = true)]
    pub tools: Vec<String>,
    /// Scope patterns.
    #[arg(long, value_delimiter = ',')]
    pub scopes: Vec<String>,
    /// Grants.
    #[arg(long, value_delimiter = ',')]
    pub grants: Vec<String>,
    /// Token ceiling.
    #[arg(long)]
    pub tokens: Option<u64>,
    /// Currency ceiling.
    #[arg(long)]
    pub usd: Option<f64>,
    /// Autonomy level.
    #[arg(long, default_value = "observe")]
    pub autonomy: String,
    /// Lifetime such as `24h` or seconds.
    #[arg(long, default_value = "24h")]
    pub ttl: String,
    /// Policy names.
    #[arg(long = "policy-set", value_delimiter = ',')]
    pub policy_set: Vec<String>,
    /// Requester id.
    #[arg(long = "requested-by")]
    pub requested_by: Option<String>,
    /// Requester role.
    #[arg(long)]
    pub role: Option<String>,
    /// Requester's manager.
    #[arg(long)]
    pub manager: Option<String>,
}

/// `kernos run ...`.
#[derive(Subcommand, Debug)]
pub enum RunCommand {
    /// Start a run.
    Start {
        /// `name@version` or a bundle id.
        #[arg(long)]
        bundle: String,
        /// Workflow name.
        #[arg(long)]
        workflow: String,
        /// Input JSON file (or inline JSON).
        #[arg(long)]
        input: String,
        /// Remit id.
        #[arg(long)]
        remit: String,
        /// Requester id.
        #[arg(long = "requested-by")]
        requested_by: Option<String>,
        /// Requester role.
        #[arg(long)]
        role: Option<String>,
        /// Requester's manager.
        #[arg(long)]
        manager: Option<String>,
    },
    /// Show a run's state.
    Show {
        /// Run id.
        id: String,
    },
    /// List runs.
    List {
        /// Only this state.
        #[arg(long)]
        state: Option<String>,
        /// Only this department.
        #[arg(long)]
        department: Option<String>,
        /// Page size.
        #[arg(long, default_value_t = 50)]
        limit: u64,
    },
    /// Print a run's events.
    Events {
        /// Run id.
        id: String,
        /// First sequence number.
        #[arg(long = "from-seq", default_value_t = 1)]
        from_seq: u64,
    },
    /// Replay and verify a run.
    Replay {
        /// Run id.
        id: String,
    },
    /// Abandon a run and schedule its compensations.
    Abandon {
        /// Run id.
        id: String,
        /// Why.
        #[arg(long)]
        reason: String,
        /// Who.
        #[arg(long, default_value = "operator")]
        actor: String,
    },
    /// Resume a parked run after the cause was repaired.
    Resume {
        /// Run id.
        id: String,
        /// Who.
        #[arg(long, default_value = "operator")]
        actor: String,
    },
    /// Export decided actions as a policy-test corpus (JSON Lines).
    Actions {
        /// A duration such as `30d`, or an RFC 3339 timestamp.
        #[arg(long)]
        since: Option<String>,
    },
}

/// `kernos approvals ...`.
#[derive(Subcommand, Debug)]
pub enum ApprovalsCommand {
    /// List approvals.
    List {
        /// Only this state (default pending).
        #[arg(long, default_value = "pending")]
        state: String,
        /// Only this approver, such as `role:finance_admin` or `user:u-tom`.
        #[arg(long)]
        approver: Option<String>,
    },
    /// Decide an approval.
    Decide {
        /// Approval id.
        id: String,
        /// Approve.
        #[arg(long, conflicts_with = "reject")]
        approve: bool,
        /// Reject.
        #[arg(long)]
        reject: bool,
        /// Acting user id.
        #[arg(long = "as")]
        actor: String,
        /// Acting role.
        #[arg(long)]
        role: String,
        /// The reason, at least 3 characters.
        #[arg(long)]
        reason: String,
    },
}

/// A command failure with the exit code it maps to.
#[derive(Debug)]
pub struct CommandError {
    /// Exit code.
    pub code: i32,
    /// The error as JSON, for `--json`.
    pub json: Value,
    /// The readable message.
    pub message: String,
}

impl From<ClientError> for CommandError {
    fn from(e: ClientError) -> Self {
        CommandError {
            code: EXIT_ERROR,
            json: e.to_json(),
            message: e.to_string(),
        }
    }
}

fn usage(message: impl Into<String>) -> CommandError {
    let message = message.into();
    CommandError {
        code: EXIT_USAGE,
        json: json!({"error": {"code": "usage", "message": message, "details": {}}}),
        message,
    }
}

fn failure(message: impl Into<String>) -> CommandError {
    let message = message.into();
    CommandError {
        code: EXIT_ERROR,
        json: json!({"error": {"code": "error", "message": message, "details": {}}}),
        message,
    }
}

/// Parses the arguments, runs the command and returns the process exit code.
pub fn run<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() { EXIT_USAGE } else { 0 };
        }
    };
    let json = cli.json;
    match execute(cli) {
        Ok(Output::Value(value)) => {
            emit(json, &value);
            0
        }
        Ok(Output::Lines(lines)) => {
            for line in lines {
                println!("{line}");
            }
            0
        }
        Ok(Output::Text(text)) => {
            println!("{text}");
            0
        }
        Err(e) => {
            if json {
                eprintln!(
                    "{}",
                    serde_json::to_string_pretty(&e.json).unwrap_or_default()
                );
            } else {
                eprintln!("error: {}", e.message);
            }
            e.code
        }
    }
}

/// What a command prints.
pub enum Output {
    /// A JSON value, rendered readably unless `--json`.
    Value(Value),
    /// Lines printed as they are (JSON Lines).
    Lines(Vec<String>),
    /// Plain text.
    Text(String),
}

/// Executes a parsed command.
pub fn execute(cli: Cli) -> Result<Output, CommandError> {
    let client = || Client::new(&cli.server, cli.token.clone());
    match cli.command {
        Command::Serve(args) => {
            let mut config =
                Config::load(args.config.as_deref()).map_err(|e| usage(e.to_string()))?;
            if let Some(listen) = args.listen {
                config.listen = listen;
            }
            if let Some(data) = args.data {
                config.data_dir = data;
            }
            if let Some(token) = cli.token.clone() {
                config.token = Some(token);
            }
            server::init_logging(&config.log_format);
            server::serve_until_interrupted(config).map_err(|e| failure(e.to_string()))?;
            Ok(Output::Text(String::new()))
        }
        Command::Health => {
            let report = client().get("/v1/health")?;
            if report.get("ok").and_then(Value::as_bool) != Some(true) {
                return Err(failure(format!("kernel at {} is not healthy", cli.server)));
            }
            Ok(Output::Value(report))
        }
        Command::Keys { command } => keys(command),
        Command::Bundle { command } => bundle(command, client),
        Command::Policy { command } => policy(command, client),
        Command::Remit { command } => remit(command, client),
        Command::Run { command } => run_cmd(command, client),
        Command::Approvals { command } => approvals(command, client),
    }
}

fn keys(command: KeysCommand) -> Result<Output, CommandError> {
    match command {
        KeysCommand::Generate { out } => {
            let pair = KeyPair::generate(system_now_ms());
            let private = with_extension(&out, "key");
            let public = with_extension(&out, "pub");
            pair.write_private(&private)
                .map_err(|e| failure(e.to_string()))?;
            pair.write_public(&public)
                .map_err(|e| failure(e.to_string()))?;
            Ok(Output::Value(json!({
                "key_id": pair.key_id,
                "private_key_file": private.display().to_string(),
                "public_key_file": public.display().to_string(),
                "public_key": pair.public().public_key_b64(),
            })))
        }
        KeysCommand::Trust { file, data } => {
            let key = PublicKey::load(&file).map_err(|e| failure(e.to_string()))?;
            let data = data.unwrap_or_else(|| PathBuf::from("./kernos-data"));
            let dir = data.join("keys").join("trusted");
            fs::create_dir_all(&dir)
                .map_err(|e| failure(format!("cannot create {}: {e}", dir.display())))?;
            let target = dir.join(format!("{}.pub", key.key_id));
            key.write(&target).map_err(|e| failure(e.to_string()))?;
            // No restart: the kernel reads this directory when it verifies a
            // signature, so the key is usable as soon as the file is there.
            Ok(Output::Value(
                json!({"key_id": key.key_id, "trusted_file": target.display().to_string()}),
            ))
        }
    }
}

fn with_extension(base: &Path, ext: &str) -> PathBuf {
    let mut name = base
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".{ext}"));
    base.with_file_name(name)
}

fn read_json_file(path: &Path) -> Result<Value, CommandError> {
    let text = fs::read_to_string(path)
        .map_err(|e| usage(format!("cannot read {}: {e}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|e| usage(format!("{} is not valid JSON: {e}", path.display())))
}

fn bundle(command: BundleCommand, client: impl Fn() -> Client) -> Result<Output, CommandError> {
    match command {
        BundleCommand::Sign { file, key, out } => {
            let bundle = read_json_file(&file)?;
            let pair = KeyPair::load(&key).map_err(|e| failure(e.to_string()))?;
            let signature = sign_bundle(&bundle, &pair);
            let out = out.unwrap_or_else(|| sig_path(&file));
            let value = serde_json::to_value(&signature).map_err(|e| failure(e.to_string()))?;
            fs::write(
                &out,
                serde_json::to_string_pretty(&value).unwrap_or_default(),
            )
            .map_err(|e| failure(format!("cannot write {}: {e}", out.display())))?;
            let mut shown = value;
            shown["file"] = json!(out.display().to_string());
            Ok(Output::Value(shown))
        }
        BundleCommand::Validate { file } => {
            let value = read_json_file(&file)?;
            let parsed = kernos_core::bundle::Bundle::new(value).map_err(|e| CommandError {
                code: EXIT_ERROR,
                json: json!({"error": {"code": "bundle_invalid", "message": e.message, "details": {"path": e.path}}}),
                message: format!("bundle is invalid at {}: {}", e.path, e.message),
            })?;
            let workflows = parsed.workflow_names();
            let steps: usize = workflows.iter().map(|w| parsed.steps(w).len()).sum();
            Ok(Output::Value(json!({
                "ok": true,
                "name": parsed.name(),
                "version": parsed.version(),
                "department": parsed.department(),
                "workflows": workflows.len(),
                "steps": steps,
            })))
        }
        BundleCommand::Apply { file, sig } => {
            let bundle = read_json_file(&file)?;
            let sig_file = sig.unwrap_or_else(|| sig_path(&file));
            if !sig_file.exists() {
                return Err(usage(format!(
                    "no signature file {}; sign the bundle first with kernos bundle sign",
                    sig_file.display()
                )));
            }
            let signature = read_json_file(&sig_file)?;
            let result = client().post(
                "/v1/bundles",
                &json!({"bundle": bundle, "signature": signature}),
            )?;
            Ok(Output::Value(result))
        }
        BundleCommand::List => Ok(Output::Value(client().get("/v1/bundles")?)),
        BundleCommand::Show { id } => {
            Ok(Output::Value(client().get(&format!("/v1/bundles/{id}"))?))
        }
    }
}

fn sig_path(file: &Path) -> PathBuf {
    let stem = file
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.strip_suffix(".json").unwrap_or(n).to_string())
        .unwrap_or_else(|| "bundle".into());
    file.with_file_name(format!("{stem}.sig.json"))
}

fn parse_spec(spec: &str) -> Result<(String, u64), CommandError> {
    let (name, version) = spec
        .rsplit_once('@')
        .ok_or_else(|| usage(format!("{spec} is not name@version")))?;
    let version = version
        .parse()
        .map_err(|_| usage(format!("{spec}: version must be an integer")))?;
    Ok((name.to_string(), version))
}

fn policy(command: PolicyCommand, client: impl Fn() -> Client) -> Result<Output, CommandError> {
    match command {
        PolicyCommand::Apply {
            file,
            name,
            version,
        } => {
            let source = fs::read_to_string(&file)
                .map_err(|e| usage(format!("cannot read {}: {e}", file.display())))?;
            Ok(Output::Value(client().post(
                "/v1/policies",
                &json!({"name": name, "version": version, "source": source}),
            )?))
        }
        PolicyCommand::Check { file } => {
            let source = fs::read_to_string(&file)
                .map_err(|e| usage(format!("cannot read {}: {e}", file.display())))?;
            let parsed = kernos_policy::parse(&source).map_err(|e| CommandError {
                code: EXIT_ERROR,
                json: json!({"error": {"code": "policy_invalid", "message": e.message,
                                       "details": {"line": e.line, "column": e.column}}}),
                message: format!(
                    "policy does not parse at line {}, column {}: {}",
                    e.line, e.column, e.message
                ),
            })?;
            Ok(Output::Value(json!({
                "ok": true,
                "name": parsed.name,
                "rules": parsed.rules.len(),
            })))
        }
        PolicyCommand::Test { a, b, corpus } => {
            let (name, version) = parse_spec(&a)?;
            let policy_a = json!({"name": name, "version": version});
            let policy_b = if Path::new(&b).exists() {
                let source =
                    fs::read_to_string(&b).map_err(|e| usage(format!("cannot read {b}: {e}")))?;
                json!({"source": source})
            } else {
                let (name, version) = parse_spec(&b)?;
                json!({"name": name, "version": version})
            };
            let text = fs::read_to_string(&corpus)
                .map_err(|e| usage(format!("cannot read {}: {e}", corpus.display())))?;
            let rows: Vec<Value> = if text.trim_start().starts_with('[') {
                serde_json::from_str(&text)
                    .map_err(|e| usage(format!("corpus is not a JSON array: {e}")))?
            } else {
                text.lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| {
                        serde_json::from_str(l)
                            .map_err(|e| usage(format!("corpus line is not JSON: {e}")))
                    })
                    .collect::<Result<_, _>>()?
            };
            Ok(Output::Value(client().post(
                "/v1/policies/test",
                &json!({"policy_a": policy_a, "policy_b": policy_b, "corpus": rows}),
            )?))
        }
        PolicyCommand::List => Ok(Output::Value(client().get("/v1/policies")?)),
        PolicyCommand::Show { spec } => {
            let (name, version) = parse_spec(&spec)?;
            Ok(Output::Value(
                client().get(&format!("/v1/policies/{name}/{version}"))?,
            ))
        }
    }
}

fn requested_by(
    id: Option<String>,
    role: Option<String>,
    manager: Option<String>,
) -> Option<Value> {
    id.map(|id| {
        let mut v = json!({"id": id});
        if let Some(role) = role {
            v["role"] = json!(role);
        }
        if let Some(manager) = manager {
            v["manager"] = json!(manager);
        }
        v
    })
}

fn remit(command: RemitCommand, client: impl Fn() -> Client) -> Result<Output, CommandError> {
    match command {
        RemitCommand::Issue(args) => {
            let ttl = kernos_policy::parse_duration(&args.ttl)
                .or_else(|| args.ttl.parse().ok())
                .ok_or_else(|| {
                    usage(format!("--ttl {} is not a duration such as 24h", args.ttl))
                })?;
            let mut body = json!({
                "tools": args.tools,
                "scopes": args.scopes,
                "grants": args.grants,
                "spend": {},
                "autonomy": args.autonomy,
                "ttl_seconds": ttl,
                "policy_set": args.policy_set,
            });
            if let Some(tokens) = args.tokens {
                body["spend"]["tokens"] = json!(tokens);
            }
            if let Some(usd) = args.usd {
                body["spend"]["usd"] = json!(usd);
            }
            if let Some(rb) = requested_by(args.requested_by, args.role, args.manager) {
                body["requested_by"] = rb;
            }
            Ok(Output::Value(client().post("/v1/remits", &body)?))
        }
        RemitCommand::Derive { id, body } => {
            let body: Value = serde_json::from_str(&body)
                .map_err(|e| usage(format!("--body is not JSON: {e}")))?;
            Ok(Output::Value(
                client().post(&format!("/v1/remits/{id}/derive"), &body)?,
            ))
        }
        RemitCommand::Show { id } => Ok(Output::Value(client().get(&format!("/v1/remits/{id}"))?)),
    }
}

fn run_cmd(command: RunCommand, client: impl Fn() -> Client) -> Result<Output, CommandError> {
    match command {
        RunCommand::Start {
            bundle,
            workflow,
            input,
            remit,
            requested_by: rb,
            role,
            manager,
        } => {
            let c = client();
            let bundle_id = if bundle.starts_with("bnd_") {
                bundle
            } else {
                let (name, version) = bundle
                    .rsplit_once('@')
                    .ok_or_else(|| usage("--bundle is name@version or a bundle id"))?;
                let bundles = c.get("/v1/bundles")?;
                bundles
                    .as_array()
                    .and_then(|list| {
                        list.iter()
                            .find(|b| b["name"] == name && b["version"] == version)
                            .and_then(|b| b["bundle_id"].as_str().map(str::to_string))
                    })
                    .ok_or_else(|| {
                        failure(format!("bundle {bundle} is not applied on the server"))
                    })?
            };
            let input_value: Value = if input.trim_start().starts_with('{') {
                serde_json::from_str(&input)
                    .map_err(|e| usage(format!("--input is not JSON: {e}")))?
            } else {
                read_json_file(Path::new(&input))?
            };
            let mut body = json!({"bundle_id": bundle_id, "workflow": workflow, "input": input_value, "remit_id": remit});
            if let Some(v) = requested_by(rb, role, manager) {
                body["requested_by"] = v;
            }
            Ok(Output::Value(c.post("/v1/runs", &body)?))
        }
        RunCommand::Show { id } => Ok(Output::Value(client().get(&format!("/v1/runs/{id}"))?)),
        RunCommand::List {
            state,
            department,
            limit,
        } => {
            let mut query = vec![format!("limit={limit}")];
            if let Some(s) = state {
                query.push(format!("state={s}"));
            }
            if let Some(d) = department {
                query.push(format!("department={d}"));
            }
            Ok(Output::Value(
                client().get(&format!("/v1/runs?{}", query.join("&")))?,
            ))
        }
        RunCommand::Events { id, from_seq } => {
            let c = client();
            let mut from = from_seq;
            let mut all = Vec::new();
            loop {
                let page = c.get(&format!("/v1/runs/{id}/events?from_seq={from}&limit=500"))?;
                if let Some(events) = page["events"].as_array() {
                    all.extend(events.iter().cloned());
                }
                match page["next_seq"].as_u64() {
                    Some(next) => from = next,
                    None => break,
                }
            }
            Ok(Output::Value(Value::Array(all)))
        }
        RunCommand::Replay { id } => Ok(Output::Value(
            client().post(&format!("/v1/runs/{id}/replay"), &json!({}))?,
        )),
        RunCommand::Abandon { id, reason, actor } => Ok(Output::Value(client().post(
            &format!("/v1/runs/{id}/abandon"),
            &json!({"reason": reason, "actor": {"id": actor}}),
        )?)),
        RunCommand::Resume { id, actor } => Ok(Output::Value(client().post(
            &format!("/v1/runs/{id}/resume"),
            &json!({"actor": {"id": actor}}),
        )?)),
        RunCommand::Actions { since } => {
            let path = match since {
                Some(s) => format!("/v1/actions?since={s}"),
                None => "/v1/actions".into(),
            };
            let rows = client().get(&path)?;
            let lines = rows
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|r| json!({"action": r["action"], "run": r["run"]}).to_string())
                        .collect()
                })
                .unwrap_or_default();
            Ok(Output::Lines(lines))
        }
    }
}

fn approvals(
    command: ApprovalsCommand,
    client: impl Fn() -> Client,
) -> Result<Output, CommandError> {
    match command {
        ApprovalsCommand::List { state, approver } => {
            let mut path = format!("/v1/approvals?state={state}");
            if let Some(a) = approver {
                path.push_str(&format!("&approver={a}"));
            }
            Ok(Output::Value(client().get(&path)?))
        }
        ApprovalsCommand::Decide {
            id,
            approve,
            reject,
            actor,
            role,
            reason,
        } => {
            if approve == reject {
                return Err(usage("give exactly one of --approve or --reject"));
            }
            let decision = if approve { "approved" } else { "rejected" };
            Ok(Output::Value(client().post(
                &format!("/v1/approvals/{id}"),
                &json!({"decision": decision, "actor": {"id": actor, "role": role}, "reason": reason}),
            )?))
        }
    }
}

/// Prints a value: pretty JSON with `--json`, otherwise a readable rendering.
pub fn emit(json: bool, value: &Value) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_default()
        );
    } else {
        println!("{}", render(value));
    }
}

/// Renders a value readably: a key/value list for objects, a table for lists
/// of objects, JSON for anything else.
pub fn render(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let width = map.keys().map(String::len).max().unwrap_or(0);
            map.iter()
                .map(|(k, v)| format!("{k:<width$}  {}", compact(v)))
                .collect::<Vec<_>>()
                .join("\n")
        }
        Value::Array(items) if items.iter().all(Value::is_object) && !items.is_empty() => {
            let columns: Vec<String> = items[0]
                .as_object()
                .map(|o| {
                    o.keys()
                        .filter(|k| !items[0][k.as_str()].is_object())
                        .take(8)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            let rows: Vec<Vec<String>> = items
                .iter()
                .map(|item| columns.iter().map(|c| compact(&item[c.as_str()])).collect())
                .collect();
            let widths: Vec<usize> = columns
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    rows.iter()
                        .map(|r| r[i].len())
                        .chain(std::iter::once(c.len()))
                        .max()
                        .unwrap_or(0)
                        .min(48)
                })
                .collect();
            let line = |cells: &[String]| {
                cells
                    .iter()
                    .enumerate()
                    .map(|(i, cell)| format!("{:<w$}", truncate(cell, widths[i]), w = widths[i]))
                    .collect::<Vec<_>>()
                    .join("  ")
            };
            let mut out = vec![line(&columns)];
            out.extend(rows.iter().map(|r| line(r)));
            out.join("\n")
        }
        Value::Array(items) if items.is_empty() => "(none)".into(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

fn compact(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "-".into(),
        other => other.to_string(),
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        let mut s: String = text.chars().take(width.saturating_sub(1)).collect();
        s.push('~');
        s
    }
}
