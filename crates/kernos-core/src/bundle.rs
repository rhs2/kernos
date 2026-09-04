//! Bundle validation and signature verification per 05-BUNDLE.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

use crate::canonical::{canonical_bytes, sha256_hex};
use crate::keys::{KeyPair, PublicKey};
use crate::template::collect_paths;

/// The bundle format version accepted.
pub const FORMAT: &str = "kernos.bundle/1";

/// The maximum canonical size of a bundle.
pub const MAX_CANONICAL_BYTES: usize = 1024 * 1024;

/// Default step timeout.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 120;

/// A validation failure with the dotted path of the offence.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{path}: {message}")]
pub struct BundleError {
    /// Dotted path such as `workflows.intake.steps.3.idempotency_key`.
    pub path: String,
    /// What was wrong.
    pub message: String,
}

fn err(path: impl Into<String>, message: impl Into<String>) -> BundleError {
    BundleError {
        path: path.into(),
        message: message.into(),
    }
}

/// The signature file of 05-BUNDLE.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleSignature {
    /// The signing key id.
    pub key_id: String,
    /// Always `ed25519`.
    #[serde(default = "default_algorithm")]
    pub algorithm: String,
    /// base64url signature over the canonical bundle bytes.
    pub signature: String,
    /// Hex SHA-256 of the canonical bytes, informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

fn default_algorithm() -> String {
    "ed25519".into()
}

/// Signs a bundle with a publisher key.
pub fn sign_bundle(bundle: &Value, key: &KeyPair) -> BundleSignature {
    let bytes = canonical_bytes(bundle);
    BundleSignature {
        key_id: key.key_id.clone(),
        algorithm: "ed25519".into(),
        signature: key.sign(&bytes),
        sha256: Some(sha256_hex(&bytes)),
    }
}

/// Verifies a bundle signature against the trusted keys. Returns the key that
/// verified it; any failure (unknown key, wrong algorithm, bad signature) is
/// reported as one reason so the caller maps it to `bundle_signature_invalid`.
pub fn verify_bundle_signature<'a>(
    bundle: &Value,
    signature: &BundleSignature,
    trusted: &'a [PublicKey],
) -> Result<&'a PublicKey, String> {
    if signature.algorithm != "ed25519" {
        return Err(format!("unsupported algorithm {}", signature.algorithm));
    }
    let key = trusted
        .iter()
        .find(|k| k.key_id == signature.key_id)
        .ok_or_else(|| format!("key {} is not trusted", signature.key_id))?;
    let bytes = canonical_bytes(bundle);
    if !key.verify(&bytes, &signature.signature) {
        return Err("signature does not verify over the canonical bundle".into());
    }
    Ok(key)
}

/// Read access to a validated bundle.
#[derive(Debug, Clone, PartialEq)]
pub struct Bundle {
    value: Value,
}

impl Bundle {
    /// Validates and wraps a bundle value.
    pub fn new(value: Value) -> Result<Bundle, BundleError> {
        validate_bundle(&value)?;
        Ok(Bundle { value })
    }

    /// Wraps a value that was validated when it was stored.
    pub fn from_stored(value: Value) -> Bundle {
        Bundle { value }
    }

    /// The raw JSON.
    pub fn value(&self) -> &Value {
        &self.value
    }

    /// `name`.
    pub fn name(&self) -> &str {
        self.value["name"].as_str().unwrap_or("")
    }

    /// `version`.
    pub fn version(&self) -> &str {
        self.value["version"].as_str().unwrap_or("")
    }

    /// `department`.
    pub fn department(&self) -> Option<&str> {
        self.value.get("department").and_then(Value::as_str)
    }

    /// `policies`.
    pub fn policies(&self) -> Vec<String> {
        self.value
            .get("policies")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `tools`.
    pub fn tools(&self) -> Value {
        self.value
            .get("tools")
            .cloned()
            .unwrap_or_else(|| json!([]))
    }

    /// Whether a declared tool writes.
    pub fn tool_writes(&self, id: &str) -> Option<bool> {
        self.value
            .get("tools")?
            .as_array()?
            .iter()
            .find(|t| t.get("id").and_then(Value::as_str) == Some(id))
            .map(|t| t.get("writes").and_then(Value::as_bool).unwrap_or(false))
    }

    /// `prompts`.
    pub fn prompts(&self) -> Value {
        self.value
            .get("prompts")
            .cloned()
            .unwrap_or_else(|| json!({}))
    }

    /// `mock`.
    pub fn mock(&self) -> Value {
        self.value.get("mock").cloned().unwrap_or_else(|| json!({}))
    }

    /// Workflow names.
    pub fn workflow_names(&self) -> Vec<String> {
        self.value
            .get("workflows")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// A workflow by name.
    pub fn workflow(&self, name: &str) -> Option<&Value> {
        self.value.get("workflows")?.get(name)
    }

    /// The steps of a workflow.
    pub fn steps(&self, workflow: &str) -> Vec<&Value> {
        self.workflow(workflow)
            .and_then(|w| w.get("steps"))
            .and_then(Value::as_array)
            .map(|s| s.iter().collect())
            .unwrap_or_default()
    }

    /// One step of a workflow by id.
    pub fn step(&self, workflow: &str, id: &str) -> Option<&Value> {
        self.steps(workflow)
            .into_iter()
            .find(|s| s.get("id").and_then(Value::as_str) == Some(id))
    }

    /// A workflow's `input_schema`, `{}` when absent.
    pub fn input_schema(&self, workflow: &str) -> Value {
        self.workflow(workflow)
            .and_then(|w| w.get("input_schema"))
            .cloned()
            .unwrap_or_else(|| json!({}))
    }
}

fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 120
        && text.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

fn is_semver(text: &str) -> bool {
    let core = text.split(['-', '+']).next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && (p.len() == 1 || !p.starts_with('0'))
        })
}

fn is_step_id(text: &str) -> bool {
    let mut bytes = text.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn is_tool_id(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

/// Validates a bundle against every rule of 05-BUNDLE.
pub fn validate_bundle(bundle: &Value) -> Result<(), BundleError> {
    let root = bundle
        .as_object()
        .ok_or_else(|| err("", "bundle must be an object"))?;
    if root.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err(err("format", format!("format must be {FORMAT}")));
    }
    let name = root
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| err("name", "name is required"))?;
    if !is_name(name) {
        return Err(err(
            "name",
            "name must be a dotted lowercase identifier of at most 120 characters",
        ));
    }
    let version = root
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| err("version", "version is required"))?;
    if !is_semver(version) {
        return Err(err("version", "version must be a semantic version"));
    }
    if let Some(dept) = root.get("department") {
        if !dept.is_string() && !dept.is_null() {
            return Err(err("department", "department must be a string"));
        }
    }
    if let Some(policies) = root.get("policies") {
        let items = policies
            .as_array()
            .ok_or_else(|| err("policies", "policies must be a list of names"))?;
        for (i, p) in items.iter().enumerate() {
            if !p.is_string() {
                return Err(err(format!("policies.{i}"), "policy names must be strings"));
            }
        }
    }
    if canonical_bytes(bundle).len() > MAX_CANONICAL_BYTES {
        return Err(err("", "bundle canonical size exceeds 1 MiB"));
    }

    let mut tool_ids: Vec<(&str, bool)> = Vec::new();
    if let Some(tools) = root.get("tools") {
        let items = tools
            .as_array()
            .ok_or_else(|| err("tools", "tools must be a list"))?;
        for (i, tool) in items.iter().enumerate() {
            let path = format!("tools.{i}");
            let id = tool
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| err(format!("{path}.id"), "tool id is required"))?;
            if !is_tool_id(id) {
                return Err(err(
                    format!("{path}.id"),
                    "tool ids are connector.operation in lowercase",
                ));
            }
            if tool_ids.iter().any(|(t, _)| *t == id) {
                return Err(err(
                    format!("{path}.id"),
                    format!("tool {id} is declared twice"),
                ));
            }
            let writes = match tool.get("writes") {
                None => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(err(format!("{path}.writes"), "writes must be a boolean")),
            };
            if let Some(d) = tool.get("description") {
                if !d.is_string() {
                    return Err(err(
                        format!("{path}.description"),
                        "description must be a string",
                    ));
                }
            }
            tool_ids.push((id, writes));
        }
    }

    let mut prompt_names: Vec<&str> = Vec::new();
    if let Some(prompts) = root.get("prompts") {
        let map = prompts
            .as_object()
            .ok_or_else(|| err("prompts", "prompts must be an object"))?;
        for (name, prompt) in map {
            let path = format!("prompts.{name}");
            let obj = prompt
                .as_object()
                .ok_or_else(|| err(&path, "a prompt is {system, user}"))?;
            let system = obj
                .get("system")
                .and_then(Value::as_str)
                .ok_or_else(|| err(format!("{path}.system"), "system text is required"))?;
            if system.contains("{{") {
                return Err(err(
                    format!("{path}.system"),
                    "system prompts are frozen text and may not contain templates",
                ));
            }
            let user = obj
                .get("user")
                .and_then(Value::as_str)
                .ok_or_else(|| err(format!("{path}.user"), "user text is required"))?;
            let mut paths = Vec::new();
            collect_paths(&Value::String(user.to_string()), &mut paths)
                .map_err(|e| err(format!("{path}.user"), e.to_string()))?;
            prompt_names.push(name);
        }
    }
    if let Some(mock) = root.get("mock") {
        let map = mock
            .as_object()
            .ok_or_else(|| err("mock", "mock must be an object"))?;
        for (name, value) in map {
            if !prompt_names.contains(&name.as_str()) {
                return Err(err(
                    format!("mock.{name}"),
                    format!("mock names a prompt {name} that does not exist"),
                ));
            }
            let mut paths = Vec::new();
            collect_paths(value, &mut paths)
                .map_err(|e| err(format!("mock.{name}"), e.to_string()))?;
            for p in paths {
                check_path_root(&p, &format!("mock.{name}"))?;
            }
        }
    }

    let workflows = root
        .get("workflows")
        .and_then(Value::as_object)
        .ok_or_else(|| err("workflows", "workflows must be an object"))?;
    if workflows.is_empty() {
        return Err(err("workflows", "at least one workflow is required"));
    }
    for (wf_name, workflow) in workflows {
        let wf_path = format!("workflows.{wf_name}");
        let wf = workflow
            .as_object()
            .ok_or_else(|| err(&wf_path, "a workflow is an object"))?;
        if let Some(schema) = wf.get("input_schema") {
            if !schema.is_object() {
                return Err(err(
                    format!("{wf_path}.input_schema"),
                    "input_schema must be a JSON Schema object",
                ));
            }
        }
        let steps = wf
            .get("steps")
            .and_then(Value::as_array)
            .ok_or_else(|| err(format!("{wf_path}.steps"), "steps must be a list"))?;
        if steps.is_empty() {
            return Err(err(
                format!("{wf_path}.steps"),
                "a workflow needs at least one step",
            ));
        }
        let mut seen: Vec<&str> = Vec::new();
        for (i, step) in steps.iter().enumerate() {
            let path = format!("{wf_path}.steps.{i}");
            validate_step(step, &path, i, &seen, &tool_ids, &prompt_names, root)?;
            if let Some(id) = step.get("id").and_then(Value::as_str) {
                seen.push(id);
            }
        }
    }
    Ok(())
}

fn check_path_root(path: &str, at: &str) -> Result<(), BundleError> {
    let root = path.split('.').next().unwrap_or("");
    if matches!(root, "input" | "steps" | "run") {
        Ok(())
    } else {
        Err(err(
            at,
            format!("template path {path} must start with input, steps or run"),
        ))
    }
}

/// Checks the template paths in a value: roots, and that `steps.<id>` refers to
/// a step earlier than the current one (or the step itself for compensations).
fn check_template_paths(
    value: &Value,
    at: &str,
    earlier: &[&str],
    own: Option<&str>,
) -> Result<(), BundleError> {
    let mut paths = Vec::new();
    collect_paths(value, &mut paths).map_err(|e| err(at, e.to_string()))?;
    for p in paths {
        check_path_root(&p, at)?;
        let mut parts = p.split('.');
        if parts.next() == Some("steps") {
            let Some(target) = parts.next() else {
                return Err(err(at, format!("template path {p} must name a step")));
            };
            let allowed = earlier.contains(&target) || own == Some(target);
            if !allowed {
                return Err(err(
                    at,
                    format!(
                        "template path {p} references step {target}, which is not an earlier step"
                    ),
                ));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_step(
    step: &Value,
    path: &str,
    index: usize,
    earlier: &[&str],
    tools: &[(&str, bool)],
    prompts: &[&str],
    root: &serde_json::Map<String, Value>,
) -> Result<(), BundleError> {
    let obj = step
        .as_object()
        .ok_or_else(|| err(path, "a step is an object"))?;
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| err(format!("{path}.id"), "step id is required"))?;
    if !is_step_id(id) {
        return Err(err(format!("{path}.id"), "step ids match [a-z][a-z0-9_]*"));
    }
    if earlier.contains(&id) {
        return Err(err(
            format!("{path}.id"),
            format!("step id {id} is not unique"),
        ));
    }
    let _ = index;
    if let Some(t) = obj.get("timeout_seconds") {
        if !t.as_u64().is_some_and(|n| n > 0) {
            return Err(err(
                format!("{path}.timeout_seconds"),
                "timeout_seconds must be a positive integer",
            ));
        }
    }
    if let Some(d) = obj.get("description") {
        if !d.is_string() {
            return Err(err(
                format!("{path}.description"),
                "description must be a string",
            ));
        }
    }
    let kind = obj
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| err(format!("{path}.kind"), "step kind is required"))?;
    match kind {
        "model" => {
            let tier = obj
                .get("tier")
                .and_then(Value::as_str)
                .ok_or_else(|| err(format!("{path}.tier"), "model steps need a tier"))?;
            if !matches!(tier, "deep" | "standard" | "cheap") {
                return Err(err(
                    format!("{path}.tier"),
                    "tier is deep, standard or cheap",
                ));
            }
            if let Some(effort) = obj.get("effort") {
                if !matches!(effort.as_str(), Some("low" | "medium" | "high" | "xhigh")) {
                    return Err(err(
                        format!("{path}.effort"),
                        "effort is low, medium, high or xhigh",
                    ));
                }
            }
            let prompt = obj
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or_else(|| err(format!("{path}.prompt"), "model steps need a prompt"))?;
            if !prompts.contains(&prompt) {
                return Err(err(
                    format!("{path}.prompt"),
                    format!("prompt {prompt} does not exist"),
                ));
            }
            if let Some(user) = root
                .get("prompts")
                .and_then(|p| p.get(prompt))
                .and_then(|p| p.get("user"))
            {
                check_template_paths(user, &format!("prompts.{prompt}.user"), earlier, None)?;
            }
            if let Some(mock) = root.get("mock").and_then(|m| m.get(prompt)) {
                check_template_paths(mock, &format!("mock.{prompt}"), earlier, None)?;
            }
            let schema = obj.get("output_schema");
            if let Some(s) = schema {
                if !s.is_object() {
                    return Err(err(
                        format!("{path}.output_schema"),
                        "output_schema must be a JSON Schema object",
                    ));
                }
            }
            if let Some(m) = obj.get("max_output_tokens") {
                if !m.as_u64().is_some_and(|n| n > 0) {
                    return Err(err(
                        format!("{path}.max_output_tokens"),
                        "max_output_tokens must be a positive integer",
                    ));
                }
            }
            if let Some(r) = obj.get("on_refusal") {
                if !matches!(r.as_str(), Some("park" | "escalate" | "fail")) {
                    return Err(err(
                        format!("{path}.on_refusal"),
                        "on_refusal is park, escalate or fail",
                    ));
                }
            }
            if let Some(esc) = obj.get("escalate") {
                let e = esc.as_object().ok_or_else(|| {
                    err(
                        format!("{path}.escalate"),
                        "escalate is {when_confidence_below, to_tier}",
                    )
                })?;
                if !e.get("when_confidence_below").is_some_and(Value::is_number) {
                    return Err(err(
                        format!("{path}.escalate.when_confidence_below"),
                        "must be a number",
                    ));
                }
                if !matches!(
                    e.get("to_tier").and_then(Value::as_str),
                    Some("deep" | "standard" | "cheap")
                ) {
                    return Err(err(
                        format!("{path}.escalate.to_tier"),
                        "to_tier is deep, standard or cheap",
                    ));
                }
                let has_confidence = schema
                    .and_then(|s| s.get("properties"))
                    .and_then(|p| p.get("confidence"))
                    .is_some();
                if !has_confidence {
                    return Err(err(
                        format!("{path}.escalate"),
                        "escalate requires confidence in output_schema",
                    ));
                }
            }
            if let Some(classes) = obj.get("data_classes") {
                let ok = classes
                    .as_array()
                    .is_some_and(|c| c.iter().all(Value::is_string));
                if !ok {
                    return Err(err(
                        format!("{path}.data_classes"),
                        "data_classes is a list of strings",
                    ));
                }
            }
        }
        "tool" => {
            let tool = obj
                .get("tool")
                .and_then(Value::as_str)
                .ok_or_else(|| err(format!("{path}.tool"), "tool steps need a tool"))?;
            let writes = tools
                .iter()
                .find(|(t, _)| *t == tool)
                .map(|(_, w)| *w)
                .ok_or_else(|| {
                    err(
                        format!("{path}.tool"),
                        format!("tool {tool} is not declared in tools"),
                    )
                })?;
            let args = obj.get("args").cloned().unwrap_or_else(|| json!({}));
            if !args.is_object() {
                return Err(err(format!("{path}.args"), "args must be an object"));
            }
            check_template_paths(&args, &format!("{path}.args"), earlier, None)?;
            match obj.get("idempotency_key") {
                Some(key) => {
                    let ok = key.is_string() || key.get("$ref").is_some_and(Value::is_string);
                    if !ok {
                        return Err(err(
                            format!("{path}.idempotency_key"),
                            "idempotency_key is a templated string or a $ref",
                        ));
                    }
                    check_template_paths(key, &format!("{path}.idempotency_key"), earlier, None)?;
                }
                None if writes => {
                    return Err(err(
                        format!("{path}.idempotency_key"),
                        format!("tool {tool} writes, so idempotency_key is required"),
                    ))
                }
                None => {}
            }
            if let Some(scope) = obj.get("scope") {
                if !scope.is_string() && !scope.is_null() {
                    return Err(err(format!("{path}.scope"), "scope must be a string"));
                }
            }
            if let Some(comp) = obj.get("compensation") {
                let c = comp.as_object().ok_or_else(|| {
                    err(
                        format!("{path}.compensation"),
                        "compensation is {tool, args}",
                    )
                })?;
                let ctool = c.get("tool").and_then(Value::as_str).ok_or_else(|| {
                    err(
                        format!("{path}.compensation.tool"),
                        "compensation needs a tool",
                    )
                })?;
                if !tools.iter().any(|(t, _)| *t == ctool) {
                    return Err(err(
                        format!("{path}.compensation.tool"),
                        format!("tool {ctool} is not declared in tools"),
                    ));
                }
                let cargs = c.get("args").cloned().unwrap_or_else(|| json!({}));
                if !cargs.is_object() {
                    return Err(err(
                        format!("{path}.compensation.args"),
                        "args must be an object",
                    ));
                }
                check_template_paths(
                    &cargs,
                    &format!("{path}.compensation.args"),
                    earlier,
                    Some(id),
                )?;
            }
        }
        "action" => {
            let action = obj
                .get("action")
                .ok_or_else(|| err(format!("{path}.action"), "action steps need an action"))?;
            let a = action
                .as_object()
                .ok_or_else(|| err(format!("{path}.action"), "action must be an object"))?;
            if !a.get("kind").is_some_and(Value::is_string) {
                return Err(err(
                    format!("{path}.action.kind"),
                    "action kind is required",
                ));
            }
            check_template_paths(action, &format!("{path}.action"), earlier, None)?;
        }
        other => {
            return Err(err(
                format!("{path}.kind"),
                format!("kind {other} is not model, tool or action"),
            ))
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundle of 05-BUNDLE, verbatim.
    pub fn spec_bundle() -> Value {
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
            {"id": "ledger.lookup_vendor", "description": "Find a vendor by name", "writes": false}
          ],
          "prompts": {
            "extract": {"system": "You extract fields from supplier invoices for Halcyon Provisions. Answer only with the JSON the schema asks for.", "user": "Invoice text:\n{{input.text}}"},
            "code": {"system": "You assign a general-ledger account to an invoice line.", "user": "Vendor: {{steps.extract.output.vendor}}\nDescription: {{steps.extract.output.description}}\nAccounts: {{input.accounts}}"}
          },
          "mock": {
            "extract": {"vendor": "Northwind Dairy", "invoice_id": "{{input.invoice_id}}", "total": "{{input.total}}", "currency": "USD", "description": "Milk delivery"},
            "code": {"account": "5100", "confidence": 0.93}
          },
          "workflows": {
            "intake": {
              "description": "One invoice from text to posted entry.",
              "input_schema": {"type": "object", "required": ["invoice_id", "text", "total"], "properties": {"invoice_id": {"type": "string"}, "text": {"type": "string"}, "total": {"type": "number"}, "accounts": {"type": "array"}}},
              "steps": [
                {"id": "extract", "kind": "model", "tier": "standard", "effort": "low", "prompt": "extract",
                 "output_schema": {"type": "object", "required": ["vendor", "invoice_id", "total", "currency"], "properties": {"vendor": {"type": "string"}, "invoice_id": {"type": "string"}, "total": {"type": "number"}, "currency": {"type": "string"}, "description": {"type": "string"}}},
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
                 "compensation": {"tool": "ledger.void_entry", "args": {"entry_id": {"$ref": "steps.post.output.entry_id"}, "reason": "run abandoned"}}}
              ]
            }
          }
        })
    }

    fn path_of(mutate: impl FnOnce(&mut Value)) -> String {
        let mut b = spec_bundle();
        mutate(&mut b);
        validate_bundle(&b).expect_err("invalid").path
    }

    #[test]
    fn accepts_the_spec_bundle() {
        let bundle = Bundle::new(spec_bundle()).expect("valid");
        assert_eq!(bundle.name(), "halcyon.finance.invoice_intake");
        assert_eq!(bundle.department(), Some("finance"));
        assert_eq!(bundle.policies(), vec!["finance-default"]);
        assert_eq!(bundle.tool_writes("ledger.post_entry"), Some(true));
        assert_eq!(bundle.tool_writes("ledger.lookup_vendor"), Some(false));
        assert_eq!(bundle.tool_writes("nope"), None);
        assert_eq!(bundle.steps("intake").len(), 4);
        assert_eq!(bundle.workflow_names(), vec!["intake"]);
        assert!(bundle.step("intake", "post").is_some());
    }

    #[test]
    fn reports_offending_paths() {
        assert_eq!(
            path_of(|b| b["format"] = json!("kernos.bundle/2")),
            "format"
        );
        assert_eq!(path_of(|b| b["name"] = json!("Halcyon.Finance")), "name");
        assert_eq!(path_of(|b| b["version"] = json!("1.0")), "version");
        assert_eq!(
            path_of(|b| b["prompts"]["extract"]["system"] = json!("hi {{input.text}}")),
            "prompts.extract.system"
        );
        assert_eq!(
            path_of(|b| b["workflows"]["intake"]["steps"][0]["prompt"] = json!("missing")),
            "workflows.intake.steps.0.prompt"
        );
        assert_eq!(
            path_of(|b| b["workflows"]["intake"]["steps"][3]["tool"] = json!("crm.read")),
            "workflows.intake.steps.3.tool"
        );
        assert_eq!(
            path_of(|b| {
                b["workflows"]["intake"]["steps"][3]
                    .as_object_mut()
                    .unwrap()
                    .remove("idempotency_key");
            }),
            "workflows.intake.steps.3.idempotency_key"
        );
        assert_eq!(
            path_of(|b| b["workflows"]["intake"]["steps"][1]["id"] = json!("extract")),
            "workflows.intake.steps.1.id"
        );
        assert_eq!(
            path_of(|b| b["workflows"]["intake"]["steps"][1]["id"] = json!("1code")),
            "workflows.intake.steps.1.id"
        );
        assert_eq!(
            path_of(|b| {
                b["workflows"]["intake"]["steps"][1]["output_schema"]["properties"]
                    .as_object_mut()
                    .unwrap()
                    .remove("confidence");
            }),
            "workflows.intake.steps.1.escalate"
        );
        assert_eq!(
            path_of(|b| b["workflows"]["intake"]["steps"][3]["args"]["x"] =
                json!({"$ref": "secrets.key"})),
            "workflows.intake.steps.3.args"
        );
        assert_eq!(
            path_of(|b| b["workflows"]["intake"]["steps"][0]["kind"] = json!("script")),
            "workflows.intake.steps.0.kind"
        );
        // Forward reference: extract may not read code's output.
        assert_eq!(
            path_of(|b| b["prompts"]["extract"]["user"] = json!("{{steps.code.output.account}}")),
            "prompts.extract.user"
        );
        // A step may not reference its own output outside a compensation.
        assert_eq!(
            path_of(|b| b["workflows"]["intake"]["steps"][3]["args"]["self"] =
                json!({"$ref": "steps.post.output.x"})),
            "workflows.intake.steps.3.args"
        );
        assert_eq!(
            path_of(
                |b| b["workflows"]["intake"]["steps"][3]["compensation"]["tool"] =
                    json!("crm.undo")
            ),
            "workflows.intake.steps.3.compensation.tool"
        );
        assert_eq!(
            path_of(|b| b["workflows"]["intake"]["steps"][2]["action"] = json!({"amount": 1})),
            "workflows.intake.steps.2.action.kind"
        );
        assert_eq!(
            path_of(|b| b["workflows"]["intake"]["steps"] = json!([])),
            "workflows.intake.steps"
        );
        assert_eq!(path_of(|b| b["mock"]["nope"] = json!({})), "mock.nope");
        assert_eq!(
            path_of(|b| b["tools"][1]["id"] = json!("ledger.post_entry")),
            "tools.1.id"
        );
        assert_eq!(
            path_of(|b| b["workflows"]["intake"]["steps"][0]["tier"] = json!("huge")),
            "workflows.intake.steps.0.tier"
        );
        assert_eq!(
            path_of(|b| b["description"] = json!("x".repeat(MAX_CANONICAL_BYTES))),
            ""
        );
    }

    #[test]
    fn signatures_verify_only_with_trusted_keys() {
        let bundle = spec_bundle();
        let publisher = KeyPair::generate(1);
        let stranger = KeyPair::generate(2);
        let sig = sign_bundle(&bundle, &publisher);
        assert_eq!(sig.sha256.as_deref().map(str::len), Some(64));
        let trusted = vec![publisher.public()];
        assert!(verify_bundle_signature(&bundle, &sig, &trusted).is_ok());
        let foreign = sign_bundle(&bundle, &stranger);
        assert!(verify_bundle_signature(&bundle, &foreign, &trusted).is_err());
        let mut tampered = bundle.clone();
        tampered["description"] =
            json!("Invoice intake, coding and posting for Halcyon Provisions!");
        assert!(verify_bundle_signature(&tampered, &sig, &trusted).is_err());
        let mut wrong_algo = sig.clone();
        wrong_algo.algorithm = "rsa".into();
        assert!(verify_bundle_signature(&bundle, &wrong_algo, &trusted).is_err());
        // Key order inside the bundle does not matter: canonical form is stable.
        let reordered: Value = serde_json::from_str(&bundle.to_string()).expect("parse");
        assert!(verify_bundle_signature(&reordered, &sig, &trusted).is_ok());
    }
}
