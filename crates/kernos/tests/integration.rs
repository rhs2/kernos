//! Integration tests: the server started in-process on an ephemeral port with a
//! temporary data directory, exercised over HTTP exactly as the specs describe,
//! on the fictional Halcyon Provisions bundle.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::{json, Value};
use tempfile::TempDir;

use kernos::cli::{execute, Cli, Output};
use kernos::config::Config;
use kernos::server::{start, RunningServer};
use kernos_core::bundle::sign_bundle;
use kernos_core::keys::KeyPair;
use kernos_core::Kernel;

const FINANCE_DEFAULT: &str = r#"
# finance-default v1
policy "finance-default"

require approval when
  action.kind == "payment.issue" and action.amount >= 5000
  -> approver: role("finance_admin"), sla: 4h, escalate_to: reporting_line

require approval when
  action.writes_to_system_of_record and run.remit.autonomy == "supervised"
  -> approver: run.requested_by.manager, sla: 24h

require approval when
  action.kind == "code.merge" and action.touches_path("infra/**")
  -> approver: role("platform_owner"), sla: 8h

deny when
  action.touches_data_class("personal") and not run.remit.grants("pii")

allow when
  action.kind == "invoice.read"
"#;

const FINANCE_FAST: &str = r#"
policy "finance-fast"

require approval when
  action.kind == "payment.issue" and action.amount >= 5000
  -> approver: run.requested_by.manager, sla: 1s, escalate_to: reporting_line
"#;

fn bundle() -> Value {
    json!({
        "format": "kernos.bundle/1",
        "name": "halcyon.finance.invoice_intake",
        "version": "1.0.0",
        "department": "finance",
        "description": "Invoice intake, coding and posting for Halcyon Provisions.",
        "policies": ["finance-default"],
        "tools": [
            {"id": "ledger.post_entry", "description": "Post a journal entry", "writes": true},
            {"id": "ledger.void_entry", "description": "Void a posted entry", "writes": true},
            {"id": "ledger.archive", "description": "Archive an invoice", "writes": true},
            {"id": "ledger.unarchive", "description": "Restore an archived invoice", "writes": true},
            {"id": "ledger.lookup_vendor", "description": "Find a vendor by name", "writes": false}
        ],
        "prompts": {
            "extract": {"system": "You extract fields from supplier invoices for Halcyon Provisions. Answer only with the JSON the schema asks for.", "user": "Invoice text:\n{{input.text}}"},
            "code": {"system": "You assign a general-ledger account to an invoice line.", "user": "Vendor: {{steps.extract.output.vendor}}\nAccounts: {{input.accounts}}"}
        },
        "mock": {
            "extract": {"vendor": "Northwind Dairy", "invoice_id": "{{input.invoice_id}}", "total": "{{input.total}}", "currency": "USD", "description": "Milk delivery"},
            "code": {"account": "5100", "confidence": 0.93}
        },
        "workflows": {
            "intake": {
                "description": "One invoice from text to posted entry.",
                "input_schema": {"type": "object", "required": ["invoice_id", "text", "total"],
                                 "properties": {"invoice_id": {"type": "string"}, "text": {"type": "string"}, "total": {"type": "number"}, "accounts": {"type": "array"}}},
                "steps": [
                    {"id": "extract", "kind": "model", "tier": "standard", "effort": "low", "prompt": "extract",
                     "output_schema": {"type": "object", "required": ["vendor", "invoice_id", "total", "currency"],
                                       "properties": {"vendor": {"type": "string"}, "invoice_id": {"type": "string"}, "total": {"type": "number"}, "currency": {"type": "string"}, "description": {"type": "string"}}},
                     "on_refusal": "park", "max_output_tokens": 1024},
                    {"id": "code", "kind": "model", "tier": "cheap", "effort": "low", "prompt": "code",
                     "output_schema": {"type": "object", "required": ["account", "confidence"], "properties": {"account": {"type": "string"}, "confidence": {"type": "number"}}},
                     "escalate": {"when_confidence_below": 0.7, "to_tier": "standard"}},
                    {"id": "propose_payment", "kind": "action",
                     "action": {"kind": "payment.issue", "amount": {"$ref": "steps.extract.output.total"}, "currency": {"$ref": "steps.extract.output.currency"},
                                "writes_to_system_of_record": true, "target": "ledger", "data_classes": [], "paths": [],
                                "idempotency_key": {"$ref": "input.invoice_id"}, "summary": "Pay invoice {{input.invoice_id}} to {{steps.extract.output.vendor}}"}},
                    {"id": "post", "kind": "tool", "tool": "ledger.post_entry",
                     "args": {"invoice_id": {"$ref": "input.invoice_id"}, "vendor": {"$ref": "steps.extract.output.vendor"}, "account": {"$ref": "steps.code.output.account"}, "amount": {"$ref": "steps.extract.output.total"}},
                     "idempotency_key": {"$ref": "input.invoice_id"},
                     "compensation": {"tool": "ledger.void_entry", "args": {"entry_id": {"$ref": "steps.post.output.entry_id"}, "reason": "run abandoned"}}},
                    {"id": "archive", "kind": "tool", "tool": "ledger.archive",
                     "args": {"invoice_id": {"$ref": "input.invoice_id"}}, "idempotency_key": "arch-{{input.invoice_id}}",
                     "compensation": {"tool": "ledger.unarchive", "args": {"archive_id": {"$ref": "steps.archive.output.archive_id"}}}},
                    {"id": "finish", "kind": "tool", "tool": "ledger.lookup_vendor", "args": {"name": {"$ref": "steps.extract.output.vendor"}}}
                ]
            }
        }
    })
}

struct TestServer {
    runtime: tokio::runtime::Runtime,
    server: Option<RunningServer>,
    base: String,
    dir: TempDir,
    publisher: KeyPair,
    agent: ureq::Agent,
    token: Option<String>,
}

impl TestServer {
    fn start(token: Option<&str>) -> TestServer {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = dir.path().join("kernos-data");
        std::fs::create_dir_all(data.join("keys").join("trusted")).expect("mkdir");
        std::fs::write(
            data.join("directory.json"),
            json!({"users": {"u-tom": {"role": "finance_admin", "manager": "u-cfo"}, "u-ana": {"role": "ap_clerk", "manager": "u-tom"}}}).to_string(),
        )
        .expect("directory");
        // Generate and trust the publisher key through the CLI, offline.
        let base = dir.path().join("publisher");
        assert_eq!(
            kernos::cli::run([
                "kernos",
                "keys",
                "generate",
                "--out",
                base.to_str().unwrap()
            ]),
            0
        );
        assert!(base.with_extension("key").exists());
        assert!(base.with_extension("pub").exists());
        assert_eq!(
            kernos::cli::run([
                "kernos",
                "keys",
                "trust",
                base.with_extension("pub").to_str().unwrap(),
                "--data",
                data.to_str().unwrap()
            ]),
            0
        );
        let publisher = KeyPair::load(&base.with_extension("key")).expect("publisher key");
        assert!(data
            .join("keys")
            .join("trusted")
            .join(format!("{}.pub", publisher.key_id))
            .exists());

        let config = Config {
            listen: "127.0.0.1:0".into(),
            data_dir: data,
            token: token.map(str::to_string),
            lease_sweep_interval_ms: 100,
            approval_sweep_interval_ms: 100,
            ..Config::default()
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let server = runtime.block_on(start(&config)).expect("start");
        let base = server.base_url();
        let agent_config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .build();
        TestServer {
            runtime,
            server: Some(server),
            base,
            dir,
            publisher,
            agent: ureq::Agent::new_with_config(agent_config),
            token: token.map(str::to_string),
        }
    }

    fn kernel(&self) -> Arc<Kernel> {
        self.server.as_ref().expect("server").kernel()
    }

    fn data_dir(&self) -> PathBuf {
        self.dir.path().join("kernos-data")
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        headers: &[(&str, &str)],
    ) -> (u16, Value) {
        let url = format!("{}{}", self.base, path);
        let auth = self.token.as_ref().map(|t| format!("Bearer {t}"));
        let result = if method == "GET" {
            let mut request = self.agent.get(&url);
            if let Some(a) = &auth {
                request = request.header("Authorization", a);
            }
            for (k, v) in headers {
                request = request.header(*k, *v);
            }
            request.call()
        } else {
            let mut request = self.agent.post(&url);
            if let Some(a) = &auth {
                request = request.header("Authorization", a);
            }
            for (k, v) in headers {
                request = request.header(*k, *v);
            }
            let empty = json!({});
            request.send_json(body.unwrap_or(&empty))
        };
        let mut response = result.expect("http request");
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().unwrap_or_default();
        let value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        (status, value)
    }

    fn get(&self, path: &str) -> (u16, Value) {
        self.call("GET", path, None, &[])
    }

    fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.call("POST", path, Some(&body), &[])
    }

    fn apply_bundle(&self) -> String {
        let b = bundle();
        let sig = sign_bundle(&b, &self.publisher);
        let (status, body) = self.post("/v1/bundles", json!({"bundle": b, "signature": sig}));
        assert_eq!(status, 201, "{body}");
        body["bundle_id"].as_str().expect("bundle_id").to_string()
    }

    fn apply_policy(&self, name: &str, version: u64, source: &str) {
        let (status, body) = self.post(
            "/v1/policies",
            json!({"name": name, "version": version, "source": source}),
        );
        assert_eq!(status, 201, "{body}");
        assert_eq!(body["name"], name);
        assert_eq!(body["version"], version);
    }

    fn setup(&self) -> String {
        let id = self.apply_bundle();
        self.apply_policy("finance-default", 1, FINANCE_DEFAULT);
        id
    }

    fn remit(&self, autonomy: &str, usd: Option<f64>, policy_set: &[&str]) -> (String, String) {
        let mut spend = json!({});
        if let Some(u) = usd {
            spend["usd"] = json!(u);
        }
        let (status, body) = self.post(
            "/v1/remits",
            json!({"tools": ["ledger.*", "http.get"], "scopes": ["sql:table:invoices", "sql:table:ledger_entries", "http:host:api.halcyon.example"],
                   "grants": [], "spend": spend, "autonomy": autonomy, "ttl_seconds": 86400, "policy_set": policy_set,
                   "requested_by": {"id": "u-ana", "role": "ap_clerk", "manager": "u-tom"}}),
        );
        assert_eq!(status, 201, "{body}");
        (
            body["remit_id"].as_str().unwrap().to_string(),
            body["token"].as_str().unwrap().to_string(),
        )
    }

    fn start_run(&self, bundle_id: &str, remit_id: &str, total: f64) -> String {
        let (status, body) = self.post(
            "/v1/runs",
            json!({"bundle_id": bundle_id, "workflow": "intake", "input": {"invoice_id": "inv-1001", "text": "Milk delivery 2026-09", "total": total, "accounts": ["5100", "5200"]},
                   "remit_id": remit_id, "requested_by": {"id": "u-ana", "role": "ap_clerk", "manager": "u-tom"}}),
        );
        assert_eq!(status, 201, "{body}");
        assert_eq!(body["state"], "running");
        body["run_id"].as_str().unwrap().to_string()
    }

    fn lease(&self, worker: &str, ttl: u64) -> Option<Value> {
        let (status, body) = self.post("/v1/leases", json!({"worker_id": worker, "kinds": ["model", "tool", "action", "compensation"], "ttl_seconds": ttl}));
        match status {
            204 => None,
            200 => Some(body),
            other => panic!("lease returned {other}: {body}"),
        }
    }

    fn must_lease(&self, worker: &str) -> Value {
        self.lease(worker, 30).expect("a runnable step")
    }

    fn complete(&self, lease_id: &str, output: Value, usage: Option<Value>) -> (u16, Value) {
        let mut body = json!({"output": output});
        if let Some(u) = usage {
            body["usage"] = u;
        }
        self.post(&format!("/v1/leases/{lease_id}/complete"), body)
    }

    fn run(&self, run_id: &str) -> Value {
        let (status, body) = self.get(&format!("/v1/runs/{run_id}"));
        assert_eq!(status, 200, "{body}");
        body
    }

    fn events(&self, run_id: &str) -> Vec<Value> {
        let (status, body) = self.get(&format!("/v1/runs/{run_id}/events?from_seq=1&limit=500"));
        assert_eq!(status, 200, "{body}");
        body["events"].as_array().cloned().unwrap_or_default()
    }

    fn kinds(&self, run_id: &str) -> Vec<String> {
        self.events(run_id)
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_string())
            .collect()
    }

    /// Drives the run with a simple worker until it parks, ends or a step needs
    /// a decision this driver does not make.
    fn drive(&self, run_id: &str, worker: &str) {
        while let Some(lease) = self.lease(worker, 30) {
            assert_eq!(lease["run_id"], run_id);
            let lease_id = lease["lease_id"].as_str().unwrap();
            let step = lease["step"].as_str().unwrap();
            match lease["step_def"]["kind"].as_str().unwrap() {
                "model" => {
                    let output = if step == "extract" {
                        json!({"vendor": "Northwind Dairy", "invoice_id": "inv-1001", "total": lease["context"]["input"]["total"], "currency": "USD"})
                    } else {
                        json!({"account": "5100", "confidence": 0.93})
                    };
                    let (status, _) =
                        self.complete(lease_id, output, Some(json!({"tokens": 120, "usd": 0.001})));
                    assert_eq!(status, 200);
                }
                "action" => {
                    let action = json!({"kind": "payment.issue", "amount": lease["context"]["steps"]["extract"]["output"]["total"], "currency": "USD",
                                        "writes_to_system_of_record": true, "target": "ledger", "data_classes": [], "paths": [],
                                        "idempotency_key": "inv-1001", "summary": "Pay invoice inv-1001 to Northwind Dairy"});
                    let (status, body) = self.post(
                        &format!("/v1/leases/{lease_id}/actions"),
                        json!({"action": action}),
                    );
                    match status {
                        200 if body["decision"] == "allow" => {
                            self.complete(lease_id, json!({"action_id": body["action_id"], "decision": "allow", "rule": body["rule"]}), None);
                        }
                        200 => break,
                        403 => {
                            self.post(&format!("/v1/leases/{lease_id}/fail"), json!({"error": {"code": "action_denied", "message": "denied"}, "deterministic": true}));
                            break;
                        }
                        other => panic!("action returned {other}: {body}"),
                    }
                }
                _ => {
                    let output = match step {
                        "post" => json!({"entry_id": 7, "posted_at": "2026-09-04T12:00:00.000Z"}),
                        "archive" => json!({"archive_id": 99}),
                        _ => json!({"rows": []}),
                    };
                    let (status, _) = self.complete(lease_id, output, None);
                    assert_eq!(status, 200);
                }
            }
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            self.runtime.block_on(server.shutdown());
        }
    }
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    condition()
}

#[test]
fn health_keys_and_auth() {
    let s = TestServer::start(None);
    let (status, body) = s.get("/v1/health");
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true);
    assert_eq!(body["version"], "0.1.0");
    assert_eq!(body["runs"]["running"], 0);
    let (status, keys) = s.get("/v1/keys");
    assert_eq!(status, 200);
    assert_eq!(keys["algorithm"], "ed25519");
    assert!(keys["key_id"].as_str().unwrap().starts_with("key_"));
    assert_eq!(keys["public_key"].as_str().unwrap().len(), 43);
    let (status, body) = s.get("/v1/nope");
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found");
    let (status, body) = s.call("POST", "/v1/policies", Some(&json!("not an object")), &[]);
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_json");
    assert!(s.data_dir().join("keys").join("control-plane.key").exists());
    assert!(s.data_dir().join("keys").join("control-plane.pub").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(s.data_dir().join("keys").join("control-plane.key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    drop(s);

    let secured = TestServer::start(Some("s3cret"));
    let (status, body) = secured.get("/v1/health");
    assert_eq!(status, 200, "{body}");
    let (status, body) = secured.call(
        "GET",
        "/v1/health",
        None,
        &[("Authorization", "Bearer wrong")],
    );
    // The helper adds the right token first; the explicit wrong header is a second value.
    assert!(status == 200 || status == 401, "{body}");
    let open = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build();
    let agent = ureq::Agent::new_with_config(open);
    let mut resp = agent
        .get(format!("{}/v1/health", secured.base))
        .call()
        .expect("call");
    assert_eq!(resp.status().as_u16(), 401);
    let text = resp.body_mut().read_to_string().unwrap();
    assert!(text.contains("unauthorized"));
}

#[test]
fn bundles_are_verified_artefacts() {
    let s = TestServer::start(None);
    let b = bundle();
    let stranger = KeyPair::generate(1);
    let (status, body) = s.post(
        "/v1/bundles",
        json!({"bundle": b, "signature": sign_bundle(&b, &stranger)}),
    );
    assert_eq!(status, 422);
    assert_eq!(body["error"]["code"], "bundle_signature_invalid");
    let good = sign_bundle(&b, &s.publisher);
    let mut tampered = b.clone();
    tampered["description"] = json!("Invoice intake, coding and posting for Halcyon Provisions?");
    let (status, body) = s.post(
        "/v1/bundles",
        json!({"bundle": tampered, "signature": good}),
    );
    assert_eq!(status, 422);
    assert_eq!(body["error"]["code"], "bundle_signature_invalid");
    let (status, body) = s.post(
        "/v1/bundles",
        json!({"bundle": b, "signature": {"key_id": "key_nobody", "signature": "AAAA"}}),
    );
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["code"], "bundle_signature_invalid");

    let bundle_id = s.apply_bundle();
    let (status, body) = s.post("/v1/bundles", json!({"bundle": b, "signature": good}));
    assert_eq!(status, 200);
    assert_eq!(body["bundle_id"], bundle_id);
    let mut other = b.clone();
    other["description"] = json!("Another description, same version.");
    let (status, body) = s.post(
        "/v1/bundles",
        json!({"bundle": other, "signature": sign_bundle(&other, &s.publisher)}),
    );
    assert_eq!(status, 409);
    assert_eq!(body["error"]["code"], "bundle_version_exists");
    let mut invalid = b.clone();
    invalid["version"] = json!("1.1.0");
    invalid["workflows"]["intake"]["steps"][3]
        .as_object_mut()
        .unwrap()
        .remove("idempotency_key");
    let (status, body) = s.post(
        "/v1/bundles",
        json!({"bundle": invalid, "signature": sign_bundle(&invalid, &s.publisher)}),
    );
    assert_eq!(status, 422);
    assert_eq!(body["error"]["code"], "bundle_invalid");
    assert_eq!(
        body["error"]["details"]["path"],
        "workflows.intake.steps.3.idempotency_key"
    );

    let (status, list) = s.get("/v1/bundles");
    assert_eq!(status, 200);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["department"], "finance");
    assert_eq!(list[0]["workflows"], json!(["intake"]));
    let (status, shown) = s.get(&format!("/v1/bundles/{bundle_id}"));
    assert_eq!(status, 200);
    assert_eq!(shown["bundle"]["name"], "halcyon.finance.invoice_intake");
    assert_eq!(shown["signature"]["key_id"], s.publisher.key_id);
    let (status, body) = s.get("/v1/bundles/bnd_missing");
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "bundle_not_found");
}

#[test]
fn policies_apply_and_test() {
    let s = TestServer::start(None);
    let (status, body) = s.post("/v1/policies", json!({"name": "broken", "version": 1, "source": "require approval when\n  action.kind == \"x\" ->"}));
    assert_eq!(status, 422);
    assert_eq!(body["error"]["code"], "policy_invalid");
    assert_eq!(body["error"]["details"]["line"], 2);
    assert!(body["error"]["details"]["column"].as_u64().unwrap() > 0);
    assert!(body["error"]["details"]["message"]
        .as_str()
        .unwrap()
        .contains("expected"));
    s.apply_policy("finance-default", 1, FINANCE_DEFAULT);
    let (status, body) = s.post(
        "/v1/policies",
        json!({"name": "finance-default", "version": 1, "source": FINANCE_DEFAULT}),
    );
    assert_eq!(status, 200, "identical re-post");
    let (status, body2) = s.post(
        "/v1/policies",
        json!({"name": "finance-default", "version": 1, "source": "allow when true"}),
    );
    assert_eq!(status, 409, "{body2}");
    assert_eq!(body2["error"]["code"], "policy_version_exists");
    let _ = body;
    let v2 = FINANCE_DEFAULT.replace("5000", "10000");
    s.apply_policy("finance-default", 2, &v2);
    let (status, list) = s.get("/v1/policies");
    assert_eq!(status, 200);
    assert_eq!(list.as_array().unwrap().len(), 2);
    let (status, versions) = s.get("/v1/policies/finance-default");
    assert_eq!(status, 200);
    assert_eq!(versions.as_array().unwrap().len(), 2);
    let (status, source) = s.get("/v1/policies/finance-default/2");
    assert_eq!(status, 200);
    assert!(source["source"].as_str().unwrap().contains("10000"));
    let (status, body) = s.get("/v1/policies/finance-default/9");
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "policy_not_found");

    let corpus: Vec<Value> = [1000.0, 4999.0, 5000.0, 7250.0, 9999.0, 10000.0, 25000.0]
        .iter()
        .map(|amount| json!({"action": {"kind": "payment.issue", "amount": amount, "currency": "USD", "writes_to_system_of_record": true, "target": "ledger", "data_classes": [], "paths": []},
                             "run": {"id": "run_x", "department": "finance", "bundle": {"name": "halcyon.finance.invoice_intake", "version": "1.0.0"}, "workflow": "intake",
                                     "remit": {"autonomy": "supervised", "grants": [], "tools": ["ledger.*"], "scopes": []},
                                     "requested_by": {"id": "u-ana", "role": "ap_clerk", "manager": "u-tom"}}}))
        .collect();
    let (status, report) = s.post("/v1/policies/test", json!({"policy_a": {"name": "finance-default", "version": 1}, "policy_b": {"name": "finance-default", "version": 2}, "corpus": corpus}));
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["cases"], 7);
    let indices: Vec<u64> = report["flips"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["index"].as_u64().unwrap())
        .collect();
    assert_eq!(
        indices,
        vec![2, 3, 4],
        "exactly the rows between 5,000 and 10,000 flip"
    );
    assert_eq!(report["flips"][0]["rule_a"], "finance-default@1#0");
    assert_eq!(report["flips"][0]["rule_b"], "finance-default@2#1");
    let (status, report) = s.post("/v1/policies/test", json!({"policy_a": {"name": "finance-default", "version": 1}, "policy_b": {"source": v2}, "corpus": corpus}));
    assert_eq!(status, 200);
    assert_eq!(report["flips"].as_array().unwrap().len(), 3);
    let (status, body) = s.post("/v1/policies/test", json!({"policy_a": {"name": "finance-default", "version": 7}, "policy_b": {"source": "allow when true"}, "corpus": []}));
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "policy_not_found");
    let (status, body) = s.post("/v1/policies/test", json!({"policy_a": {"name": "finance-default"}, "policy_b": {"source": "allow when true"}, "corpus": []}));
    assert_eq!(status, 422);
    assert_eq!(body["error"]["details"]["field"], "policy_a");
}

#[test]
fn remits_narrow_never_widen() {
    let s = TestServer::start(None);
    let (status, parent) = s.post(
        "/v1/remits",
        json!({"tools": ["ledger.post_entry"], "scopes": ["sql:table:ledger_entries"], "grants": ["pii"], "spend": {"tokens": 200000, "usd": 2.0},
               "autonomy": "supervised", "ttl_seconds": 86400, "policy_set": ["finance-default"], "requested_by": {"id": "u-ana", "role": "ap_clerk", "manager": "u-tom"}}),
    );
    assert_eq!(status, 201, "{parent}");
    let remit_id = parent["remit_id"].as_str().unwrap();
    assert!(parent["token"].as_str().unwrap().starts_with("krt1."));
    assert_eq!(parent["token"].as_str().unwrap().split('.').count(), 4);
    let (status, shown) = s.get(&format!("/v1/remits/{remit_id}"));
    assert_eq!(status, 200);
    assert_eq!(shown["autonomy"], "supervised");
    assert_eq!(shown["spend"]["usd"], 2.0);
    assert_eq!(shown["parent_id"], Value::Null);
    assert_eq!(shown["run_id"], Value::Null);
    assert!(shown.get("signature").is_none());
    let (status, body) = s.get("/v1/remits/rem_missing");
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "remit_not_found");

    let widen = |body: Value| {
        let (status, resp) = s.post(&format!("/v1/remits/{remit_id}/derive"), body);
        assert_eq!(status, 422, "{resp}");
        assert_eq!(resp["error"]["code"], "remit_widens");
        resp["error"]["details"]["field"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(widen(json!({"tools": ["ledger.*"]})), "tools");
    assert_eq!(widen(json!({"tools": ["crm.read"]})), "tools");
    assert_eq!(widen(json!({"scopes": ["sql:table:*"]})), "scopes");
    assert_eq!(widen(json!({"grants": ["pii", "phi"]})), "grants");
    assert_eq!(widen(json!({"spend": {"usd": 3.0}})), "spend.usd");
    assert_eq!(widen(json!({"spend": {"tokens": 300000}})), "spend.tokens");
    assert_eq!(widen(json!({"autonomy": "autonomous"})), "autonomy");
    assert_eq!(widen(json!({"ttl_seconds": 999999})), "ttl_seconds");
    assert_eq!(widen(json!({"policy_set": []})), "policy_set");

    let (status, child) = s.post(
        &format!("/v1/remits/{remit_id}/derive"),
        json!({"tools": ["ledger.post_entry"], "spend": {"usd": 1.0}, "autonomy": "propose"}),
    );
    assert_eq!(status, 201, "{child}");
    assert_eq!(child["parent_id"], remit_id);
    let child_id = child["remit_id"].as_str().unwrap();
    let (status, shown) = s.get(&format!("/v1/remits/{child_id}"));
    assert_eq!(status, 200);
    assert_eq!(shown["autonomy"], "propose");
    assert_eq!(shown["spend"]["usd"], 1.0);
    assert_eq!(shown["spend"]["tokens"], 200000);
    assert_eq!(shown["grants"], json!(["pii"]));
    let payload = s
        .kernel()
        .verify_remit_token(child["token"].as_str().unwrap())
        .expect("verifies");
    assert_eq!(payload.rid, child_id);
    assert_eq!(payload.parent.as_deref(), Some(remit_id));
    let (status, body) = s.post(
        "/v1/remits",
        json!({"tools": ["led ger"], "autonomy": "supervised"}),
    );
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["code"], "invalid_request");
}

#[test]
fn run_start_validates_input_and_binds_the_remit() {
    let s = TestServer::start(None);
    let bundle_id = s.setup();
    let (remit_id, _) = s.remit("autonomous", Some(2.0), &["finance-default"]);
    let (status, body) = s.post("/v1/runs", json!({"bundle_id": bundle_id, "workflow": "intake", "input": {"invoice_id": "inv-1", "text": "x"}, "remit_id": remit_id, "requested_by": {"id": "u-ana"}}));
    assert_eq!(status, 422);
    assert_eq!(body["error"]["code"], "input_invalid");
    assert_eq!(body["error"]["details"]["path"], "total");
    let (status, body) = s.post("/v1/runs", json!({"bundle_id": bundle_id, "workflow": "intake", "input": {"invoice_id": 5, "text": "x", "total": 1}, "remit_id": remit_id, "requested_by": {"id": "u-ana"}}));
    assert_eq!(status, 422);
    assert_eq!(body["error"]["details"]["path"], "invoice_id");
    let (status, body) = s.post("/v1/runs", json!({"bundle_id": "bnd_missing", "workflow": "intake", "input": {}, "remit_id": remit_id}));
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "bundle_not_found");
    let (status, body) = s.post(
        "/v1/runs",
        json!({"bundle_id": bundle_id, "workflow": "nope", "input": {}, "remit_id": remit_id}),
    );
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "workflow_not_found");
    let (status, body) = s.post("/v1/runs", json!({"bundle_id": bundle_id, "workflow": "intake", "input": {"invoice_id": "inv-1", "text": "x", "total": 1.0}, "remit_id": "rem_missing"}));
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "remit_not_found");

    let run_id = s.start_run(&bundle_id, &remit_id, 1200.0);
    let (status, body) = s.post("/v1/runs", json!({"bundle_id": bundle_id, "workflow": "intake", "input": {"invoice_id": "inv-2", "text": "x", "total": 1.0}, "remit_id": remit_id}));
    assert_eq!(status, 409);
    assert_eq!(body["error"]["code"], "remit_bound");
    let (_, shown) = s.get(&format!("/v1/remits/{remit_id}"));
    assert_eq!(shown["run_id"], run_id);

    let run = s.run(&run_id);
    assert_eq!(run["state"], "running");
    assert_eq!(run["bundle"]["name"], "halcyon.finance.invoice_intake");
    assert_eq!(run["bundle"]["id"], bundle_id);
    assert_eq!(run["workflow"], "intake");
    assert_eq!(run["steps"].as_array().unwrap().len(), 6);
    assert_eq!(run["steps"][0]["state"], "scheduled");
    assert_eq!(run["budget"]["ceiling_usd"], 2.0);
    assert_eq!(run["last_seq"], 7);
    let kinds = s.kinds(&run_id);
    assert_eq!(kinds[0], "run.created");
    assert_eq!(kinds.iter().filter(|k| *k == "step.scheduled").count(), 6);
    let events = s.events(&run_id);
    assert_eq!(events[0]["seq"], 1);
    assert_eq!(events[0]["prev_hash"], "0".repeat(64));
    assert_eq!(events[0]["schema"], "kernos.events/1");
    assert_eq!(events[1]["prev_hash"], events[0]["hash"]);
    assert_eq!(events[0]["payload"]["budget"]["soft_ratio"], 0.8);

    // A remit lacking the bundle's tools still starts a run; the gateway refuses later.
    let (status, narrow) = s.post("/v1/remits", json!({"tools": ["ledger.lookup_vendor"], "autonomy": "observe", "policy_set": ["finance-default"], "requested_by": {"id": "u-ana"}}));
    assert_eq!(status, 201, "{narrow}");
    let (status, body) = s.post("/v1/runs", json!({"bundle_id": bundle_id, "workflow": "intake", "input": {"invoice_id": "inv-3", "text": "x", "total": 1.0}, "remit_id": narrow["remit_id"]}));
    assert_eq!(status, 201, "{body}");

    let (status, page) = s.get("/v1/runs?state=running&department=finance&limit=1");
    assert_eq!(status, 200);
    assert_eq!(page["runs"].as_array().unwrap().len(), 1);
    assert_eq!(page["runs"][0]["run_id"], run_id);
    let next = page["next"].as_str().unwrap();
    let (_, page2) = s.get(&format!("/v1/runs?state=running&limit=1&after={next}"));
    assert_eq!(page2["runs"][0]["run_id"], body["run_id"]);
    let (_, page3) = s.get("/v1/runs?state=completed");
    assert!(page3["runs"].as_array().unwrap().is_empty());
    assert_eq!(page3["next"], Value::Null);
    let (status, body) = s.get("/v1/runs/run_missing");
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "run_not_found");
}

#[test]
fn leases_complete_fail_and_quarantine() {
    let s = TestServer::start(None);
    let bundle_id = s.setup();
    assert!(s.lease("wrk-a1", 30).is_none(), "204 when idle");
    let (remit_id, _) = s.remit("autonomous", Some(2.0), &["finance-default"]);
    let run_id = s.start_run(&bundle_id, &remit_id, 1200.0);

    let lease = s.must_lease("wrk-a1");
    assert_eq!(lease["step"], "extract");
    assert_eq!(lease["attempt"], 1);
    assert_eq!(lease["heartbeat_seconds"], 10);
    assert_eq!(lease["step_def"]["prompt"], "extract");
    assert_eq!(lease["context"]["input"]["invoice_id"], "inv-1001");
    assert_eq!(lease["context"]["run"]["department"], "finance");
    assert_eq!(lease["context"]["run"]["bundle"]["version"], "1.0.0");
    assert_eq!(lease["context"]["remit"]["autonomy"], "autonomous");
    assert_eq!(lease["context"]["tools"].as_array().unwrap().len(), 5);
    assert_eq!(lease["context"]["mock"]["code"]["account"], "5100");
    assert_eq!(lease["context"]["pacing"], false);
    assert_eq!(lease["context"]["approved_actions"], json!([]));
    assert_eq!(lease["context"]["prior_events"], json!([]));
    assert!(lease["context"]["remit_token"]
        .as_str()
        .unwrap()
        .starts_with("krt1."));
    assert!(
        s.lease("wrk-a2", 30).is_none(),
        "strict order leaves nothing else"
    );
    let lease_id = lease["lease_id"].as_str().unwrap().to_string();
    let (status, hb) = s.post(&format!("/v1/leases/{lease_id}/heartbeat"), json!({}));
    assert_eq!(status, 200);
    assert!(hb["expires_at"].as_str().unwrap().ends_with('Z'));
    let (status, body) = s.post("/v1/leases/lse_missing/heartbeat", json!({}));
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "lease_not_found");

    // Non-deterministic failure: backoff then retry with attempt 2.
    let (status, failed) = s.post(
        &format!("/v1/leases/{lease_id}/fail"),
        json!({"error": {"code": "timeout", "message": "provider slow"}, "deterministic": false}),
    );
    assert_eq!(status, 200, "{failed}");
    assert_eq!(failed["outcome"], "retry_scheduled");
    let delay = failed["delay_ms"].as_u64().unwrap();
    assert!((250..=500).contains(&delay), "{delay}");
    assert!(
        s.lease("wrk-a1", 30).is_none(),
        "not runnable during the backoff"
    );
    assert!(wait_until(Duration::from_millis(delay + 400), || s
        .lease("wrk-a1", 30)
        .is_some()));
    let run = s.run(&run_id);
    assert_eq!(run["steps"][0]["attempts"], 2);
    assert_eq!(run["steps"][0]["state"], "leased");
    let lease_id = run["steps"][0]["lease"]["lease_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, body) = s.post(&format!("/v1/leases/{lease_id}/heartbeat"), json!({}));
    assert_eq!(status, 200, "{body}");

    // Expiry by a short TTL and re-lease by another worker.
    let (status, _) = s.post(
        &format!("/v1/leases/{lease_id}/fail"),
        json!({"error": {"code": "timeout", "message": "again"}, "deterministic": false}),
    );
    assert_eq!(status, 200);
    assert!(wait_until(Duration::from_millis(1500), || s
        .lease("wrk-a1", 1)
        .is_some()));
    let run = s.run(&run_id);
    assert_eq!(run["steps"][0]["attempts"], 3);
    let short_lease = run["steps"][0]["lease"]["lease_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        wait_until(Duration::from_secs(4), || s.run(&run_id)["steps"][0]
            ["state"]
            == "scheduled"),
        "the sweeper expires the 1 s lease"
    );
    assert!(s.kinds(&run_id).contains(&"step.lease_expired".to_string()));
    let (status, body) = s.post(&format!("/v1/leases/{short_lease}/heartbeat"), json!({}));
    assert_eq!(status, 410);
    assert_eq!(body["error"]["code"], "lease_expired");
    let (status, body) = s.complete(&short_lease, json!({}), None);
    assert_eq!(status, 410, "{body}");
    let lease = s.must_lease("wrk-a2");
    assert_eq!(lease["step"], "extract");
    assert_eq!(lease["attempt"], 4);
    let lease_id = lease["lease_id"].as_str().unwrap().to_string();
    let (status, done) = s.complete(&lease_id, json!({"vendor": "Northwind Dairy", "invoice_id": "inv-1001", "total": 1200.0, "currency": "USD"}), Some(json!({"tokens": 100, "usd": 0.002})));
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["run_state"], "running");
    assert_eq!(done["next_step"], "code");
    let run = s.run(&run_id);
    assert_eq!(run["steps"][0]["state"], "completed");
    assert_eq!(run["budget"]["used_usd"], 0.002);
    assert_eq!(run["budget"]["used_tokens"], 100);
    let (status, body) = s.complete(&lease_id, json!({}), None);
    assert_eq!(status, 410, "{body}");

    // Three deterministic failures quarantine the step and park the run; no fourth lease.
    for expected_attempt in 1..=3u64 {
        let lease = s.must_lease("wrk-a1");
        assert_eq!(lease["step"], "code");
        assert_eq!(lease["attempt"], expected_attempt);
        let id = lease["lease_id"].as_str().unwrap();
        let (status, failed) = s.post(&format!("/v1/leases/{id}/fail"), json!({"error": {"code": "output_invalid", "message": "schema"}, "deterministic": true}));
        assert_eq!(status, 200, "{failed}");
        if expected_attempt < 3 {
            assert_eq!(failed["outcome"], "retry_scheduled");
            assert_eq!(failed["delay_ms"], 0);
        } else {
            assert_eq!(failed["outcome"], "quarantined");
        }
    }
    assert!(s.lease("wrk-a1", 30).is_none(), "no fourth lease");
    let run = s.run(&run_id);
    assert_eq!(run["state"], "parked");
    assert_eq!(run["park_reason"], "quarantine");
    assert_eq!(run["steps"][1]["state"], "quarantined");
    let events = s.events(&run_id);
    let q = events
        .iter()
        .find(|e| e["kind"] == "step.quarantined")
        .unwrap();
    assert_eq!(q["payload"]["attempts"], 3);
    let parked = events.iter().find(|e| e["kind"] == "run.parked").unwrap();
    assert_eq!(parked["payload"]["reason"], "quarantine");
    let (status, body) = s.get("/v1/health");
    assert_eq!(status, 200);
    assert_eq!(body["runs"]["parked"], 1);

    let (status, body) = s.post(
        &format!("/v1/runs/{run_id}/resume"),
        json!({"actor": {"id": "u-ops"}}),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["run_state"], "running");
    let lease = s.must_lease("wrk-a1");
    assert_eq!(lease["step"], "code");
    assert_eq!(lease["attempt"], 1);
    s.complete(
        lease["lease_id"].as_str().unwrap(),
        json!({"account": "5100", "confidence": 0.93}),
        None,
    );
    s.drive(&run_id, "wrk-a1");
    let run = s.run(&run_id);
    assert_eq!(run["state"], "completed");
    assert_eq!(run["output"], json!({"rows": []}));
    let (status, body) = s.post(&format!("/v1/runs/{run_id}/resume"), json!({}));
    assert_eq!(status, 409);
    assert_eq!(body["error"]["code"], "run_not_parked");
}

#[test]
fn actions_allow_approval_and_deny() {
    let s = TestServer::start(None);
    let bundle_id = s.setup();

    // allow: autonomous under the threshold uses the default rule.
    let (remit_id, _) = s.remit("autonomous", None, &["finance-default"]);
    let run_id = s.start_run(&bundle_id, &remit_id, 1200.0);
    s.drive(&run_id, "wrk-a1");
    let run = s.run(&run_id);
    assert_eq!(run["state"], "completed");
    assert_eq!(run["decisions"][0]["decision"], "allow");
    assert_eq!(run["decisions"][0]["rule"], "default");

    // approval_required: parks the run and releases the lease.
    let (remit_id, _) = s.remit("supervised", None, &["finance-default"]);
    let run_id = s.start_run(&bundle_id, &remit_id, 7250.0);
    s.drive(&run_id, "wrk-a1");
    let run = s.run(&run_id);
    assert_eq!(run["state"], "parked");
    assert_eq!(run["park_reason"], "approval");
    assert_eq!(run["steps"][2]["state"], "waiting_approval");
    assert_eq!(run["steps"][2]["lease"], Value::Null);
    assert_eq!(run["decisions"][0]["decision"], "approval_required");
    assert_eq!(run["decisions"][0]["rule"], "finance-default@1#0");
    let approval_id = run["pending_approval"]["approval_id"]
        .as_str()
        .unwrap()
        .to_string();
    let events = s.events(&run_id);
    let requested = events
        .iter()
        .find(|e| e["kind"] == "approval.requested")
        .unwrap();
    assert_eq!(
        requested["payload"]["approver"],
        json!({"type": "role", "value": "finance_admin"})
    );
    assert_eq!(requested["payload"]["sla_seconds"], 14400);
    assert_eq!(requested["payload"]["escalate_to"], "reporting_line");
    let decided = events
        .iter()
        .find(|e| e["kind"] == "policy.decided")
        .unwrap();
    assert_eq!(decided["payload"]["policy"], "finance-default");
    assert_eq!(decided["payload"]["policy_version"], 1);
    assert_eq!(decided["actor"]["type"], "policy");
    assert!(
        s.lease("wrk-a1", 30).is_none(),
        "the worker holds no lease and nothing is runnable"
    );
    let old_lease = events
        .iter()
        .rev()
        .find(|e| e["kind"] == "step.leased")
        .unwrap()["payload"]["lease_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, body) = s.post(&format!("/v1/leases/{old_lease}/heartbeat"), json!({}));
    assert_eq!(status, 410, "{body}");
    let (status, list) = s.get("/v1/approvals?state=pending&approver=role:finance_admin");
    assert_eq!(status, 200);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["approval_id"], approval_id);
    assert_eq!(list[0]["run_id"], run_id);
    assert_eq!(list[0]["action"]["amount"], 7250.0);
    assert_eq!(list[0]["escalations"], 0);
    let (_, none) = s.get("/v1/approvals?state=pending&approver=role:platform_owner");
    assert!(none.as_array().unwrap().is_empty());

    // deny: personal data without the pii grant.
    let (remit_id, _) = s.remit("autonomous", None, &["finance-default"]);
    let run_id3 = s.start_run(&bundle_id, &remit_id, 100.0);
    let lease = s.must_lease("wrk-a1");
    s.complete(lease["lease_id"].as_str().unwrap(), json!({"vendor": "Northwind Dairy", "invoice_id": "inv-1001", "total": 100.0, "currency": "USD"}), None);
    let lease = s.must_lease("wrk-a1");
    s.complete(
        lease["lease_id"].as_str().unwrap(),
        json!({"account": "5100", "confidence": 0.9}),
        None,
    );
    let lease = s.must_lease("wrk-a1");
    assert_eq!(lease["step"], "propose_payment");
    let lease_id = lease["lease_id"].as_str().unwrap().to_string();
    let (status, body) = s.post(
        &format!("/v1/leases/{lease_id}/actions"),
        json!({"action": {"kind": "email.send", "writes_to_system_of_record": false, "data_classes": ["personal"], "paths": [], "summary": "Send statement"}}),
    );
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "action_denied");
    assert_eq!(body["error"]["details"]["rule"], "finance-default@1#3");
    assert!(body["error"]["details"]["action_id"]
        .as_str()
        .unwrap()
        .starts_with("act_"));
    let run = s.run(&run_id3);
    assert_eq!(run["decisions"][0]["decision"], "deny");
    let (status, failed) = s.post(
        &format!("/v1/leases/{lease_id}/fail"),
        json!({"error": {"code": "action_denied", "message": "denied"}, "deterministic": true}),
    );
    assert_eq!(status, 200, "{failed}");
    assert_eq!(failed["outcome"], "retry_scheduled");
    let (status, body) = s.post(
        &format!("/v1/leases/{lease_id}/actions"),
        json!({"action": {"kind": "x"}}),
    );
    assert_eq!(status, 410, "{body}");
    let (status, body) = s.post(
        "/v1/leases/lse_nope/actions",
        json!({"action": {"kind": "x"}}),
    );
    assert_eq!(status, 404, "{body}");
}

#[test]
fn approvals_resume_reject_and_escalate() {
    let s = TestServer::start(None);
    let bundle_id = s.setup();
    let (remit_id, _) = s.remit("supervised", None, &["finance-default"]);
    let run_id = s.start_run(&bundle_id, &remit_id, 7250.0);
    s.drive(&run_id, "wrk-a1");
    let approval_id = s.run(&run_id)["pending_approval"]["approval_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, body) = s.post(&format!("/v1/approvals/{approval_id}"), json!({"decision": "approved", "actor": {"id": "u-ana", "role": "ap_clerk"}, "reason": "I want it paid"}));
    assert_eq!(status, 403);
    assert_eq!(body["error"]["code"], "not_the_approver");
    let (status, body) = s.post(&format!("/v1/approvals/{approval_id}"), json!({"decision": "approved", "actor": {"id": "u-tom", "role": "finance_admin"}, "reason": "ok"}));
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["code"], "reason_required");
    let (status, body) = s.post(&format!("/v1/approvals/{approval_id}"), json!({"decision": "approved", "actor": {"id": "u-tom", "role": "finance_admin"}, "reason": "Vendor and amount verified against the PO"}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["run_id"], run_id);
    assert_eq!(body["run_state"], "running");
    let (status, body) = s.post(&format!("/v1/approvals/{approval_id}"), json!({"decision": "rejected", "actor": {"id": "u-tom", "role": "finance_admin"}, "reason": "changed my mind"}));
    assert_eq!(status, 409);
    assert_eq!(body["error"]["code"], "already_decided");
    let (status, shown) = s.get(&format!("/v1/approvals/{approval_id}"));
    assert_eq!(status, 200);
    assert_eq!(shown["state"], "approved");
    assert_eq!(shown["actor"]["id"], "u-tom");

    let lease = s.must_lease("wrk-a2");
    assert_eq!(lease["step"], "propose_payment");
    assert_eq!(
        lease["context"]["approved_actions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let lease_id = lease["lease_id"].as_str().unwrap();
    let (status, outcome) = s.post(
        &format!("/v1/leases/{lease_id}/actions"),
        json!({"action": {"kind": "payment.issue", "amount": 7250.0, "currency": "USD", "writes_to_system_of_record": true, "target": "ledger",
                          "data_classes": [], "paths": [], "idempotency_key": "inv-1001", "summary": "Pay invoice inv-1001 to Northwind Dairy"}}),
    );
    assert_eq!(status, 200, "{outcome}");
    assert_eq!(outcome["decision"], "allow");
    assert_eq!(outcome["rule"], format!("approved:{approval_id}"));
    s.complete(
        lease_id,
        json!({"action_id": outcome["action_id"], "decision": "allow", "rule": outcome["rule"]}),
        None,
    );
    s.drive(&run_id, "wrk-a2");
    let run = s.run(&run_id);
    assert_eq!(run["state"], "completed");
    let kinds = s.kinds(&run_id);
    for expected in [
        "action.proposed",
        "policy.decided",
        "approval.requested",
        "step.waiting_approval",
        "run.parked",
        "approval.decided",
        "run.resumed",
        "step.scheduled",
        "step.leased",
        "run.completed",
    ] {
        assert!(kinds.contains(&expected.to_string()), "{expected}");
    }
    let events = s.events(&run_id);
    let decided = events
        .iter()
        .find(|e| e["kind"] == "approval.decided")
        .unwrap();
    assert_eq!(
        decided["payload"]["actor"],
        json!({"id": "u-tom", "role": "finance_admin"})
    );
    assert_eq!(
        decided["payload"]["reason"],
        "Vendor and amount verified against the PO"
    );
    assert_eq!(decided["actor"], json!({"type": "user", "id": "u-tom"}));
    let (status, report) = s.post(&format!("/v1/runs/{run_id}/replay"), json!({}));
    assert_eq!(status, 200);
    assert_eq!(report["chain_valid"], true);
    assert_eq!(report["state_matches"], true);
    assert_eq!(report["decisions"], 2);
    assert_eq!(report["decision_mismatches"], json!([]));

    // Rejection fails the run with action_rejected.
    let (remit_id, _) = s.remit("supervised", None, &["finance-default"]);
    let run2 = s.start_run(&bundle_id, &remit_id, 9000.0);
    s.drive(&run2, "wrk-a1");
    let approval2 = s.run(&run2)["pending_approval"]["approval_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, body) = s.post(&format!("/v1/approvals/{approval2}"), json!({"decision": "rejected", "actor": {"id": "u-tom", "role": "finance_admin"}, "reason": "Duplicate of inv-1000"}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["run_state"], "failed");
    let run = s.run(&run2);
    assert_eq!(run["state"], "failed");
    assert_eq!(run["error"]["code"], "action_rejected");
    assert_eq!(run["steps"][2]["state"], "failed");

    // SLA escalation with a 1 s policy: reporting_line goes to the manager's manager.
    s.apply_policy("finance-fast", 1, FINANCE_FAST);
    let (remit_id, _) = s.remit("supervised", None, &["finance-fast", "finance-default"]);
    let run3 = s.start_run(&bundle_id, &remit_id, 6000.0);
    s.drive(&run3, "wrk-a1");
    let run = s.run(&run3);
    assert_eq!(run["decisions"][0]["rule"], "finance-fast@1#0");
    assert_eq!(
        run["pending_approval"]["approver"],
        json!({"type": "user", "value": "u-tom"})
    );
    assert!(wait_until(Duration::from_secs(4), || s
        .kinds(&run3)
        .contains(&"approval.escalated".to_string())));
    let events = s.events(&run3);
    let esc = events
        .iter()
        .find(|e| e["kind"] == "approval.escalated")
        .unwrap();
    assert_eq!(
        esc["payload"]["from"],
        json!({"type": "user", "value": "u-tom"})
    );
    assert_eq!(
        esc["payload"]["to"],
        json!({"type": "user", "value": "u-cfo"})
    );
    assert_eq!(esc["payload"]["reason"], "sla_expired");
    let (_, list) = s.get("/v1/approvals?state=pending&approver=user:u-cfo");
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["escalations"], 1);
    assert!(wait_until(Duration::from_secs(4), || s.run(&run3)
        ["needs_human"]
        == true));
    let run = s.run(&run3);
    assert_eq!(run["state"], "parked");
    assert_eq!(run["park_reason"], "human");
    let approval3 = run["pending_approval"]["approval_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, body) = s.post(&format!("/v1/approvals/{approval3}"), json!({"decision": "approved", "actor": {"id": "u-cfo", "role": "cfo"}, "reason": "Approved by the CFO"}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["run_state"], "running");
    s.drive(&run3, "wrk-a1");
    assert_eq!(s.run(&run3)["state"], "completed");
}

#[test]
fn budgets_throttle_then_park() {
    let s = TestServer::start(None);
    let bundle_id = s.setup();
    let (remit_id, _) = s.remit("autonomous", Some(0.0005), &["finance-default"]);
    let run_id = s.start_run(&bundle_id, &remit_id, 100.0);
    let lease = s.must_lease("wrk-a1");
    let (status, _) = s.complete(lease["lease_id"].as_str().unwrap(), json!({"vendor": "Northwind Dairy", "invoice_id": "inv-1001", "total": 100.0, "currency": "USD"}), Some(json!({"tokens": 300, "usd": 0.00042})));
    assert_eq!(status, 200);
    let run = s.run(&run_id);
    assert_eq!(run["budget"]["soft_hit"], true);
    assert_eq!(run["state"], "running");
    let events = s.events(&run_id);
    let soft = events
        .iter()
        .find(|e| e["kind"] == "budget.soft_threshold")
        .expect("soft threshold");
    assert_eq!(soft["payload"]["ceiling_usd"], 0.0005);
    let lease = s.must_lease("wrk-a1");
    assert_eq!(lease["context"]["pacing"], true);
    let (status, done) = s.complete(
        lease["lease_id"].as_str().unwrap(),
        json!({"account": "5100", "confidence": 0.93}),
        Some(json!({"tokens": 300, "usd": 0.0002})),
    );
    assert_eq!(status, 200);
    assert_eq!(done["run_state"], "parked");
    let run = s.run(&run_id);
    assert_eq!(run["state"], "parked");
    assert_eq!(run["park_reason"], "budget");
    assert_eq!(run["budget"]["exceeded"], true);
    let kinds = s.kinds(&run_id);
    assert!(kinds.contains(&"budget.exceeded".to_string()));
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "budget.soft_threshold")
            .count(),
        1
    );
    assert!(
        s.lease("wrk-a1", 30).is_none(),
        "no further leases for the run"
    );
}

#[test]
fn abandon_schedules_and_runs_compensations() {
    let s = TestServer::start(None);
    let bundle_id = s.setup();
    let (remit_id, _) = s.remit("autonomous", None, &["finance-default"]);
    let run_id = s.start_run(&bundle_id, &remit_id, 100.0);
    for _ in 0..5 {
        let lease = s.must_lease("wrk-a1");
        let id = lease["lease_id"].as_str().unwrap();
        match lease["step"].as_str().unwrap() {
            "extract" => s.complete(id, json!({"vendor": "Northwind Dairy", "invoice_id": "inv-1001", "total": 100.0, "currency": "USD"}), None),
            "code" => s.complete(id, json!({"account": "5100", "confidence": 0.93}), None),
            "propose_payment" => {
                let (_, o) = s.post(&format!("/v1/leases/{id}/actions"), json!({"action": {"kind": "payment.issue", "amount": 100.0, "writes_to_system_of_record": true}}));
                assert_eq!(o["decision"], "allow");
                s.complete(id, json!({"action_id": o["action_id"]}), None)
            }
            "post" => s.complete(id, json!({"entry_id": 41, "posted_at": "2026-09-04T12:00:00.000Z"}), None),
            "archive" => s.complete(id, json!({"archive_id": 9}), None),
            other => panic!("{other}"),
        };
    }
    let (status, body) = s.post(
        &format!("/v1/runs/{run_id}/abandon"),
        json!({"reason": "operator request", "actor": {"id": "u-ops"}}),
    );
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["compensations_scheduled"], 2);
    let run = s.run(&run_id);
    assert_eq!(
        run["state"], "running",
        "a run is not abandoned until its writes are unwound"
    );
    assert_eq!(run["abandoning"], true);
    assert_eq!(
        run["compensations"],
        json!([{"for_step": "archive", "step": "comp_archive", "state": "scheduled"}, {"for_step": "post", "step": "comp_post", "state": "scheduled"}])
    );
    let events = s.events(&run_id);
    let scheduled: Vec<&Value> = events
        .iter()
        .filter(|e| e["kind"] == "compensation.scheduled")
        .collect();
    assert_eq!(scheduled.len(), 2);
    assert_eq!(scheduled[0]["payload"]["for_step"], "archive");
    assert_eq!(scheduled[0]["payload"]["tool"], "ledger.unarchive");
    assert_eq!(scheduled[0]["payload"]["args"], json!({"archive_id": 9}));
    assert_eq!(scheduled[1]["payload"]["for_step"], "post");
    assert_eq!(
        scheduled[1]["payload"]["args"],
        json!({"entry_id": 41, "reason": "run abandoned"})
    );

    let c1 = s.must_lease("wrk-a1");
    assert_eq!(c1["step"], "comp_archive");
    assert_eq!(c1["step_def"]["kind"], "compensation");
    assert_eq!(c1["step_def"]["tool"], "ledger.unarchive");
    assert_eq!(c1["step_def"]["for_step"], "archive");
    assert_eq!(
        c1["step_def"]["idempotency_key"],
        format!("comp:{run_id}:archive")
    );
    let (status, done) = s.complete(
        c1["lease_id"].as_str().unwrap(),
        json!({"restored": true}),
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(
        done["run_state"], "running",
        "one compensation still pending"
    );
    assert_eq!(done["next_step"], "comp_post");
    let (status, body) = s.post(
        &format!("/v1/runs/{run_id}/abandon"),
        json!({"reason": "again", "actor": {}}),
    );
    assert_eq!(
        status, 409,
        "an unwind in progress cannot be restarted: {body}"
    );
    assert_eq!(body["error"]["code"], "run_not_abandonable");
    let c2 = s.must_lease("wrk-a1");
    assert_eq!(c2["step_def"]["tool"], "ledger.void_entry");
    assert_eq!(c2["step_def"]["args"]["entry_id"], 41);
    s.complete(
        c2["lease_id"].as_str().unwrap(),
        json!({"entry_id": 41, "voided_at": "2026-09-04T12:05:00.000Z"}),
        None,
    );
    let run = s.run(&run_id);
    assert_eq!(
        run["state"], "abandoned",
        "abandoned once the last compensation completed"
    );
    assert!(run["compensations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["state"] == "completed"));
    assert_eq!(
        s.kinds(&run_id)
            .iter()
            .filter(|k| *k == "compensation.completed")
            .count(),
        2
    );
    assert!(s.lease("wrk-a1", 30).is_none());
    let (status, body) = s.post(
        &format!("/v1/runs/{run_id}/abandon"),
        json!({"reason": "again", "actor": {}}),
    );
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "run_not_abandonable");
    // A run with no completed writes has nothing to unwind and is abandoned at once.
    let (remit_id, _) = s.remit("autonomous", None, &["finance-default"]);
    let clean = s.start_run(&bundle_id, &remit_id, 100.0);
    let (status, body) = s.post(
        &format!("/v1/runs/{clean}/abandon"),
        json!({"reason": "not needed", "actor": {"id": "u-ops"}}),
    );
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["compensations_scheduled"], 0);
    let run = s.run(&clean);
    assert_eq!(run["state"], "abandoned");
    assert_eq!(run["compensations"], json!([]));
    assert!(
        s.lease("wrk-a1", 30).is_none(),
        "an abandoned run leases nothing"
    );
    let (status, report) = s.post(&format!("/v1/runs/{clean}/replay"), json!({}));
    assert_eq!(status, 200);
    assert_eq!(report["chain_valid"], true);
    assert_eq!(report["state_matches"], true);
}

#[test]
fn a_key_trusted_after_the_kernel_started_is_accepted() {
    // The quickstart order: serve, then generate a key, then trust it, then
    // apply. None of that may need a restart of the control plane.
    let s = TestServer::start(None);
    let data = s.data_dir();
    let base = s.dir.path().join("second-publisher");
    assert_eq!(
        kernos::cli::run([
            "kernos",
            "keys",
            "generate",
            "--out",
            base.to_str().unwrap()
        ]),
        0
    );
    let publisher = KeyPair::load(&base.with_extension("key")).expect("key");
    let mut b = bundle();
    b["version"] = json!("2.0.0");
    let signature = sign_bundle(&b, &publisher);

    let (status, body) = s.post("/v1/bundles", json!({"bundle": b, "signature": signature}));
    assert_eq!(status, 422, "the key is not trusted yet: {body}");
    assert_eq!(body["error"]["code"], "bundle_signature_invalid");

    assert_eq!(
        kernos::cli::run([
            "kernos",
            "keys",
            "trust",
            base.with_extension("pub").to_str().unwrap(),
            "--data",
            data.to_str().unwrap(),
        ]),
        0
    );
    let (status, body) = s.post("/v1/bundles", json!({"bundle": b, "signature": signature}));
    assert_eq!(status, 201, "trusted while the kernel is running: {body}");
    assert_eq!(body["version"], "2.0.0");

    // Removing the key stops the next signature from it, equally without a restart.
    let trusted_file = data
        .join("keys")
        .join("trusted")
        .join(format!("{}.pub", publisher.key_id));
    assert!(trusted_file.exists());
    std::fs::remove_file(&trusted_file).expect("untrust");
    let mut later = bundle();
    later["version"] = json!("2.1.0");
    let (status, body) = s.post(
        "/v1/bundles",
        json!({"bundle": later, "signature": sign_bundle(&later, &publisher)}),
    );
    assert_eq!(status, 422, "untrusted again: {body}");
    assert_eq!(body["error"]["code"], "bundle_signature_invalid");

    // The key trusted before the server started still works throughout.
    let (status, body) = s.post(
        "/v1/bundles",
        json!({"bundle": later, "signature": sign_bundle(&later, &s.publisher)}),
    );
    assert_eq!(status, 201, "{body}");

    // `keys trust` no longer tells the operator to restart anything.
    let cli = Cli::try_parse_from([
        "kernos",
        "--json",
        "keys",
        "trust",
        base.with_extension("pub").to_str().unwrap(),
        "--data",
        data.to_str().unwrap(),
    ])
    .expect("parse");
    match execute(cli).expect("trust") {
        Output::Value(v) => {
            assert_eq!(v["key_id"], publisher.key_id);
            assert!(v["trusted_file"].is_string());
            assert!(v.get("note").is_none(), "no restart note: {v}");
        }
        _ => panic!("trust prints a value"),
    }
}

#[test]
fn cli_health_reports_the_running_kernel() {
    let s = TestServer::start(None);
    assert_eq!(
        kernos::cli::run(["kernos", "--server", &s.base, "health", "--json"]),
        0,
        "the container health check must pass against a live kernel"
    );
    assert_eq!(
        kernos::cli::run(["kernos", "--server", &s.base, "health"]),
        0
    );
    let cli =
        Cli::try_parse_from(["kernos", "--server", &s.base, "--json", "health"]).expect("parse");
    match execute(cli).expect("health") {
        Output::Value(report) => {
            assert_eq!(report["ok"], true);
            assert_eq!(report["version"], "0.1.0");
            assert!(report["uptime_s"].is_number());
            assert_eq!(report["runs"]["running"], 0);
        }
        _ => panic!("health prints a value"),
    }
    // A kernel that is not there exits non-zero, which is what the health check
    // in the container image relies on.
    assert_eq!(
        kernos::cli::run(["kernos", "--server", "http://127.0.0.1:1", "health"]),
        1
    );
}

#[test]
fn cli_validates_bundles_and_policies_offline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let good = dir.path().join("bundle.json");
    std::fs::write(&good, bundle().to_string()).unwrap();
    // The server address points nowhere: these commands never call one.
    let args = |file: &Path, extra: &[&str]| {
        let mut v = vec![
            "kernos".to_string(),
            "--server".into(),
            "http://127.0.0.1:1".into(),
            "--json".into(),
        ];
        v.extend(extra.iter().map(|e| e.to_string()));
        v.push(file.display().to_string());
        v
    };
    let value = |file: &Path, extra: &[&str]| -> Value {
        let cli = Cli::try_parse_from(args(file, extra)).expect("parse");
        match execute(cli) {
            Ok(Output::Value(v)) => v,
            Ok(_) => panic!("expected a value"),
            Err(e) => panic!("unexpected error: {}", e.message),
        }
    };
    let report = value(&good, &["bundle", "validate"]);
    assert_eq!(report["ok"], true);
    assert_eq!(report["name"], "halcyon.finance.invoice_intake");
    assert_eq!(report["version"], "1.0.0");
    assert_eq!(report["department"], "finance");
    assert_eq!(report["workflows"], 1);
    assert_eq!(report["steps"], 6);
    assert_eq!(kernos::cli::run(args(&good, &["bundle", "validate"])), 0);

    let bad = dir.path().join("bad-bundle.json");
    let mut broken = bundle();
    broken["workflows"]["intake"]["steps"][3]
        .as_object_mut()
        .unwrap()
        .remove("idempotency_key");
    std::fs::write(&bad, broken.to_string()).unwrap();
    let cli = Cli::try_parse_from(args(&bad, &["bundle", "validate"])).expect("parse");
    let err = execute(cli).err().expect("invalid bundle");
    assert_eq!(err.code, 1);
    assert_eq!(err.json["error"]["code"], "bundle_invalid");
    assert_eq!(
        err.json["error"]["details"]["path"],
        "workflows.intake.steps.3.idempotency_key"
    );
    assert_eq!(kernos::cli::run(args(&bad, &["bundle", "validate"])), 1);
    let missing = dir.path().join("nowhere.json");
    assert_eq!(
        kernos::cli::run(args(&missing, &["bundle", "validate"])),
        2,
        "a missing file is a usage error"
    );

    let policy = dir.path().join("finance-default.policy");
    std::fs::write(&policy, FINANCE_DEFAULT).unwrap();
    let report = value(&policy, &["policy", "check"]);
    assert_eq!(report["ok"], true);
    assert_eq!(report["name"], "finance-default");
    assert_eq!(report["rules"], 5);
    assert_eq!(kernos::cli::run(args(&policy, &["policy", "check"])), 0);

    let bad_policy = dir.path().join("broken.policy");
    std::fs::write(
        &bad_policy,
        "policy \"broken\"\nrequire approval when\n  action.kind ==\n",
    )
    .unwrap();
    let cli = Cli::try_parse_from(args(&bad_policy, &["policy", "check"])).expect("parse");
    let err = execute(cli).err().expect("invalid policy");
    assert_eq!(err.code, 1);
    assert_eq!(err.json["error"]["code"], "policy_invalid");
    assert_eq!(err.json["error"]["details"]["line"], 4);
    assert!(err.json["error"]["details"]["column"].as_u64().unwrap() >= 1);
    assert_eq!(kernos::cli::run(args(&bad_policy, &["policy", "check"])), 1);
    assert_eq!(kernos::cli::run(args(&missing, &["policy", "check"])), 2);
}

#[test]
fn external_events_need_a_lease_or_a_remit() {
    let s = TestServer::start(None);
    let bundle_id = s.setup();
    let (remit_id, parent_token) = s.remit("autonomous", None, &["finance-default"]);
    let run_id = s.start_run(&bundle_id, &remit_id, 100.0);
    let lease = s.must_lease("wrk-a1");
    let lease_id = lease["lease_id"].as_str().unwrap().to_string();
    let child_token = lease["context"]["remit_token"]
        .as_str()
        .unwrap()
        .to_string();
    let path = format!("/v1/runs/{run_id}/events");
    let event = json!({"kind": "tool.called", "payload": {"step": "extract", "tool": "ledger.lookup_vendor", "args": {"name": "Northwind Dairy"}, "scope": null, "idempotency_key": "inv-1001"}, "actor": {"type": "worker", "id": "wrk-a1"}});

    let (status, body) = s.call("POST", &path, Some(&event), &[]);
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "event_not_permitted");
    let (status, body) = s.call(
        "POST",
        &path,
        Some(&event),
        &[("X-Kernos-Lease", "lse_bogus")],
    );
    assert_eq!(status, 403, "{body}");
    let (status, body) = s.call(
        "POST",
        &path,
        Some(&event),
        &[("X-Kernos-Lease", &lease_id)],
    );
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["seq"], 9);
    assert_eq!(body["hash"].as_str().unwrap().len(), 64);
    let mut wrong_step = event.clone();
    wrong_step["payload"]["step"] = json!("code");
    let (status, body) = s.call(
        "POST",
        &path,
        Some(&wrong_step),
        &[("X-Kernos-Lease", &lease_id)],
    );
    assert_eq!(status, 403, "{body}");
    let internal = json!({"kind": "step.completed", "payload": {"step": "extract"}, "actor": {"type": "worker", "id": "wrk-a1"}});
    let (status, body) = s.call(
        "POST",
        &path,
        Some(&internal),
        &[("X-Kernos-Lease", &lease_id)],
    );
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "event_not_permitted");
    let refused = json!({"kind": "tool.refused", "payload": {"step": "extract", "tool": "ledger.post_entry", "reason": "tool_not_in_remit", "remit_id": remit_id, "detail": "not matched"}, "actor": {"type": "gateway", "id": "gw"}});
    let (status, body) = s.call(
        "POST",
        &path,
        Some(&refused),
        &[("X-Kernos-Remit", &child_token)],
    );
    assert_eq!(status, 201, "{body}");
    let (status, body) = s.call(
        "POST",
        &path,
        Some(&refused),
        &[("X-Kernos-Remit", &parent_token)],
    );
    assert_eq!(
        status, 201,
        "the parent remit is bound to this run too: {body}"
    );
    let (status, body) = s.call(
        "POST",
        &path,
        Some(&refused),
        &[("X-Kernos-Remit", "krt1.not.a.token")],
    );
    assert_eq!(status, 403, "{body}");
    let (other_remit, other_token) = s.remit("autonomous", None, &["finance-default"]);
    let _ = other_remit;
    let (status, body) = s.call(
        "POST",
        &path,
        Some(&refused),
        &[("X-Kernos-Remit", &other_token)],
    );
    assert_eq!(status, 403, "a remit for another run: {body}");
    let (status, body) = s.call(
        "POST",
        "/v1/runs/run_missing/events",
        Some(&refused),
        &[("X-Kernos-Remit", &child_token)],
    );
    assert_eq!(status, 404, "{body}");
    let responded = json!({"kind": "model.responded", "payload": {"step": "extract", "output": {"text": "x"}, "usage": {"input_tokens": 9, "output_tokens": 9, "cache_read_tokens": 0, "cache_write_tokens": 0}, "cost_usd": 5.0, "stop_reason": "end_turn", "refusal": false, "latency_ms": 1}, "actor": {"type": "worker", "id": "wrk-a1"}});
    let (status, _) = s.call(
        "POST",
        &path,
        Some(&responded),
        &[("X-Kernos-Lease", &lease_id)],
    );
    assert_eq!(status, 201);
    assert_eq!(
        s.run(&run_id)["budget"]["used_usd"],
        0.0,
        "model.responded never feeds usage"
    );
    let (status, page) = s.get(&format!("/v1/runs/{run_id}/events?from_seq=9&limit=2"));
    assert_eq!(status, 200);
    assert_eq!(page["events"].as_array().unwrap().len(), 2);
    assert_eq!(page["next_seq"], 11);
    let (_, page) = s.get(&format!("/v1/runs/{run_id}/events?from_seq=11&limit=500"));
    assert_eq!(page["events"].as_array().unwrap().len(), 2);
    assert_eq!(page["next_seq"], Value::Null);

    // After the lease is lost the next lease of the step carries the prior tool events.
    let (status, _) = s.post(
        &format!("/v1/leases/{lease_id}/fail"),
        json!({"error": {"code": "timeout", "message": "lost"}, "deterministic": false}),
    );
    assert_eq!(status, 200);
    assert!(wait_until(Duration::from_secs(2), || s
        .lease("wrk-a2", 30)
        .is_some()));
    let run = s.run(&run_id);
    let new_lease = run["steps"][0]["lease"]["lease_id"].as_str().unwrap();
    let _ = new_lease;
    let events = s.events(&run_id);
    let last_lease = events
        .iter()
        .rev()
        .find(|e| e["kind"] == "step.leased")
        .unwrap();
    assert_eq!(last_lease["payload"]["attempt"], 2);
    let (status, body) = s.get("/v1/actions?since=30d");
    assert_eq!(status, 200);
    assert!(body.as_array().unwrap().is_empty());
}

#[test]
fn replay_verifies_and_detects_tampering() {
    let s = TestServer::start(None);
    let bundle_id = s.setup();
    let (remit_id, _) = s.remit("autonomous", None, &["finance-default"]);
    let run_id = s.start_run(&bundle_id, &remit_id, 1200.0);
    s.drive(&run_id, "wrk-a1");
    assert_eq!(s.run(&run_id)["state"], "completed");
    let (status, report) = s.post(&format!("/v1/runs/{run_id}/replay"), json!({}));
    assert_eq!(status, 200);
    assert_eq!(report["chain_valid"], true);
    assert_eq!(report["state_matches"], true);
    assert_eq!(report["decisions"], 1);
    assert_eq!(report["decision_mismatches"], json!([]));
    assert_eq!(report["chain_errors"], json!([]));
    assert_eq!(report["events"], s.events(&run_id).len() as u64);
    assert_eq!(report["state"]["state"], "completed");
    let (status, body) = s.post("/v1/runs/run_missing/replay", json!({}));
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "run_not_found");

    // Flip a byte of one payload directly in the SQLite file.
    let events = s.events(&run_id);
    let target = events
        .iter()
        .find(|e| e["kind"] == "step.completed")
        .unwrap()["seq"]
        .as_u64()
        .unwrap();
    let db = s.data_dir().join("kernos.db");
    let conn = rusqlite::Connection::open(&db).expect("open db");
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    let changed = conn
        .execute(
            "UPDATE events SET payload = replace(payload, 'Northwind Dairy', 'Northwind Diary') WHERE run_id = ?1 AND seq = ?2",
            rusqlite::params![run_id, target as i64],
        )
        .expect("tamper");
    assert_eq!(changed, 1);
    drop(conn);
    let (status, report) = s.post(&format!("/v1/runs/{run_id}/replay"), json!({}));
    assert_eq!(status, 200);
    assert_eq!(report["chain_valid"], false);
    assert_eq!(report["chain_errors"][0]["seq"], target);
    assert_eq!(report["chain_errors"][0]["error"], "hash_mismatch");
    assert_eq!(report["state_matches"], false);
    assert_eq!(report["events"], events.len() as u64);
}

#[test]
fn metrics_are_prometheus_text() {
    let s = TestServer::start(None);
    let bundle_id = s.setup();
    let (remit_id, _) = s.remit("autonomous", Some(2.0), &["finance-default"]);
    let run_id = s.start_run(&bundle_id, &remit_id, 1200.0);
    s.drive(&run_id, "wrk-a1");
    let url = format!("{}/v1/metrics", s.base);
    let mut resp = s.agent.get(&url).call().expect("metrics");
    assert_eq!(resp.status().as_u16(), 200);
    let content_type = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(content_type.starts_with("text/plain"));
    let text = resp.body_mut().read_to_string().unwrap();
    assert!(text.contains("# TYPE kernos_runs gauge"));
    assert!(text.contains("kernos_runs{state=\"completed\"} 1"));
    assert!(text.contains("kernos_steps_leased_total 6"));
    assert!(text.contains("kernos_leases_expired_total 0"));
    assert!(text.contains("kernos_approvals_pending 0"));
    assert!(text.contains("kernos_events_total{kind=\"run.completed\"} 1"));
    assert!(text.contains("kernos_usage_usd_total{department=\"finance\"}"));
    assert!(text.contains("kernos_step_latency_seconds_bucket{le=\"+Inf\"} 6"));
    assert!(text.contains("kernos_step_latency_seconds_count 6"));
}

#[test]
fn cli_round_trip_against_the_server() {
    let s = TestServer::start(None);
    let dir = s.dir.path();
    let bundle_path = dir.join("bundle.json");
    std::fs::write(&bundle_path, bundle().to_string()).unwrap();
    let key = dir.join("publisher.key");
    let sig = dir.join("sig.json");
    let run = |args: &[&str]| -> Value {
        let mut full = vec!["kernos", "--server", &s.base, "--json"];
        full.extend_from_slice(args);
        let cli = Cli::try_parse_from(full).expect("parse");
        match execute(cli) {
            Ok(Output::Value(v)) => v,
            Ok(Output::Lines(lines)) => Value::Array(
                lines
                    .iter()
                    .map(|l| serde_json::from_str(l).unwrap())
                    .collect(),
            ),
            Ok(Output::Text(t)) => Value::String(t),
            Err(e) => panic!("{}: {}", e.code, e.message),
        }
    };
    let signed = run(&[
        "bundle",
        "sign",
        bundle_path.to_str().unwrap(),
        "--key",
        key.to_str().unwrap(),
        "--out",
        sig.to_str().unwrap(),
    ]);
    assert_eq!(signed["key_id"], s.publisher.key_id);
    assert_eq!(signed["algorithm"], "ed25519");
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(&sig).unwrap()).unwrap();
    assert_eq!(on_disk["sha256"].as_str().unwrap().len(), 64);
    assert!(on_disk.get("signature").is_some());
    let applied = run(&[
        "bundle",
        "apply",
        bundle_path.to_str().unwrap(),
        "--sig",
        sig.to_str().unwrap(),
    ]);
    let bundle_id = applied["bundle_id"].as_str().unwrap().to_string();
    assert!(bundle_id.starts_with("bnd_"));
    let listed = run(&["bundle", "list"]);
    assert_eq!(listed[0]["bundle_id"], bundle_id);

    let policy_path = dir.join("finance-default.policy");
    std::fs::write(&policy_path, FINANCE_DEFAULT).unwrap();
    let applied = run(&[
        "policy",
        "apply",
        policy_path.to_str().unwrap(),
        "--name",
        "finance-default",
        "--version",
        "1",
    ]);
    assert_eq!(applied["version"], 1);
    let v2_path = dir.join("v2.policy");
    std::fs::write(&v2_path, FINANCE_DEFAULT.replace("5000", "10000")).unwrap();
    let corpus_path = dir.join("actions.jsonl");
    let corpus: Vec<String> = [1000.0, 7250.0, 20000.0]
        .iter()
        .map(|a| json!({"action": {"kind": "payment.issue", "amount": a, "writes_to_system_of_record": true}, "run": {"remit": {"autonomy": "autonomous"}, "requested_by": {"manager": "u-tom"}}}).to_string())
        .collect();
    std::fs::write(&corpus_path, corpus.join("\n")).unwrap();
    let flips = run(&[
        "policy",
        "test",
        "--a",
        "finance-default@1",
        "--b",
        v2_path.to_str().unwrap(),
        "--corpus",
        corpus_path.to_str().unwrap(),
    ]);
    assert_eq!(flips["cases"], 3);
    assert_eq!(flips["flips"].as_array().unwrap().len(), 1);
    assert_eq!(flips["flips"][0]["index"], 1);

    let remit = run(&[
        "remit",
        "issue",
        "--tools",
        "ledger.*,http.get",
        "--scopes",
        "sql:table:*",
        "--usd",
        "2",
        "--autonomy",
        "supervised",
        "--ttl",
        "24h",
        "--policy-set",
        "finance-default",
        "--requested-by",
        "u-ana",
        "--role",
        "ap_clerk",
        "--manager",
        "u-tom",
    ]);
    let remit_id = remit["remit_id"].as_str().unwrap().to_string();
    let shown = run(&["remit", "show", &remit_id]);
    assert_eq!(shown["autonomy"], "supervised");
    assert_eq!(shown["tools"], json!(["ledger.*", "http.get"]));
    let input_path = dir.join("input.json");
    std::fs::write(
        &input_path,
        json!({"invoice_id": "inv-1001", "text": "Milk", "total": 7250.0}).to_string(),
    )
    .unwrap();
    let started = run(&[
        "run",
        "start",
        "--bundle",
        "halcyon.finance.invoice_intake@1.0.0",
        "--workflow",
        "intake",
        "--input",
        input_path.to_str().unwrap(),
        "--remit",
        &remit_id,
    ]);
    let run_id = started["run_id"].as_str().unwrap().to_string();
    s.drive(&run_id, "wrk-a1");
    let shown = run(&["run", "show", &run_id]);
    assert_eq!(shown["state"], "parked");
    let approvals = run(&["approvals", "list"]);
    assert_eq!(approvals.as_array().unwrap().len(), 1);
    let approval_id = approvals[0]["approval_id"].as_str().unwrap().to_string();
    let decided = run(&[
        "approvals",
        "decide",
        &approval_id,
        "--approve",
        "--as",
        "u-tom",
        "--role",
        "finance_admin",
        "--reason",
        "Looks right",
    ]);
    assert_eq!(decided["run_state"], "running");
    s.drive(&run_id, "wrk-a1");
    let events = run(&["run", "events", &run_id]);
    assert!(events.as_array().unwrap().len() > 20);
    let replay = run(&["run", "replay", &run_id]);
    assert_eq!(replay["chain_valid"], true);
    assert_eq!(replay["state_matches"], true);
    let actions = run(&["run", "actions", "--since", "30d"]);
    assert_eq!(
        actions.as_array().unwrap().len(),
        1,
        "the approved re-proposal updates the same action row"
    );
    assert_eq!(actions[0]["action"]["amount"], 7250.0);
    assert_eq!(actions[0]["run"]["remit"]["autonomy"], "supervised");
    let listed = run(&["run", "list", "--state", "completed"]);
    assert_eq!(listed["runs"][0]["run_id"], run_id);

    // Errors map to exit code 1 (API) and 2 (usage).
    assert_eq!(
        kernos::cli::run(["kernos", "--server", &s.base, "run", "show", "run_missing"]),
        1
    );
    assert_eq!(
        kernos::cli::run([
            "kernos", "--server", &s.base, "run", "abandon", &run_id, "--reason", "late"
        ]),
        1
    );
    assert_eq!(
        kernos::cli::run([
            "kernos", "--server", &s.base, "policy", "test", "--a", "nonsense", "--b", "x",
            "--corpus", "nope"
        ]),
        2
    );
    assert_eq!(
        kernos::cli::run([
            "kernos",
            "--server",
            &s.base,
            "bundle",
            "apply",
            dir.join("missing.json").to_str().unwrap()
        ]),
        2
    );
    assert_eq!(
        kernos::cli::run(["kernos", "--server", "http://127.0.0.1:1", "bundle", "list"]),
        1
    );
    assert_eq!(kernos::cli::run(["kernos", "nonsense"]), 2);
    let readable =
        kernos::cli::render(&json!({"run_id": "run_1", "state": "completed", "nested": {"a": 1}}));
    assert!(readable.contains("run_id  run_1"));
    let table = kernos::cli::render(&json!([{"id": "a", "n": 1}, {"id": "b", "n": 2}]));
    assert!(table.lines().count() == 3);
    let _ = Path::new("unused");
}
