//! Bundle templating: `{{path}}` inside strings and `{"$ref": "path"}` as whole
//! values, evaluated against `{input, steps.<id>.output, run: {...}}`.

use serde_json::{Map, Value};
use thiserror::Error;

/// A templating failure. `template_missing_path` is deterministic and fails the
/// step; a bad `$ref` shape is a bundle defect caught at validation time.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{code}: {path}")]
pub struct TemplateError {
    /// `template_missing_path` or `template_bad_ref`.
    pub code: &'static str,
    /// The offending path.
    pub path: String,
}

/// Looks a dotted path up in the context; numeric segments index lists.
pub fn lookup_path<'a>(context: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = context;
    for segment in path.split('.') {
        current = match current {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

/// Renders a value as template text: strings verbatim, everything else as
/// compact JSON.
pub fn render_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Replaces every `{{path}}` in a string. A missing path is an error, never an
/// empty string.
pub fn render_string(template: &str, context: &Value) -> Result<String, TemplateError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let path = after[..end].trim();
        match lookup_path(context, path) {
            Some(value) => out.push_str(&render_text(value)),
            None => {
                return Err(TemplateError {
                    code: "template_missing_path",
                    path: path.to_string(),
                })
            }
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Resolves `$ref` objects (type preserved) and `{{path}}` strings at any depth.
pub fn resolve(value: &Value, context: &Value) -> Result<Value, TemplateError> {
    match value {
        Value::String(s) => Ok(Value::String(render_string(s, context)?)),
        Value::Array(items) => items
            .iter()
            .map(|item| resolve(item, context))
            .collect::<Result<_, _>>()
            .map(Value::Array),
        Value::Object(map) => {
            if let Some(reference) = ref_path(map) {
                return lookup_path(context, reference)
                    .cloned()
                    .ok_or_else(|| TemplateError {
                        code: "template_missing_path",
                        path: reference.to_string(),
                    });
            }
            let mut out = Map::with_capacity(map.len());
            for (key, item) in map {
                out.insert(key.clone(), resolve(item, context)?);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

fn ref_path(map: &Map<String, Value>) -> Option<&str> {
    if map.len() == 1 {
        map.get("$ref").and_then(Value::as_str)
    } else {
        None
    }
}

/// Every path referenced by a value, from `$ref` objects and `{{path}}` strings,
/// for bundle validation. `$ref` objects with a non-string value are reported as
/// `template_bad_ref` errors.
pub fn collect_paths(value: &Value, out: &mut Vec<String>) -> Result<(), TemplateError> {
    match value {
        Value::String(s) => {
            let mut rest = s.as_str();
            while let Some(start) = rest.find("{{") {
                let after = &rest[start + 2..];
                let Some(end) = after.find("}}") else { break };
                out.push(after[..end].trim().to_string());
                rest = &after[end + 2..];
            }
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                collect_paths(item, out)?;
            }
            Ok(())
        }
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref") {
                match reference.as_str() {
                    Some(path) if map.len() == 1 => {
                        out.push(path.to_string());
                        return Ok(());
                    }
                    _ => {
                        return Err(TemplateError {
                            code: "template_bad_ref",
                            path: reference.to_string(),
                        })
                    }
                }
            }
            for item in map.values() {
                collect_paths(item, out)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context() -> Value {
        json!({"input": {"invoice_id": "inv-1001", "total": 7250.0, "lines": [{"amount": 5}]},
               "steps": {"extract": {"output": {"vendor": "Northwind Dairy", "tags": ["a", "b"]}}},
               "run": {"id": "run_x"}})
    }

    #[test]
    fn renders_strings_with_every_type() {
        let s = render_string("Pay {{input.invoice_id}} of {{ input.total }} to {{steps.extract.output.vendor}} {{steps.extract.output.tags}} {{input.lines.0.amount}}", &context()).expect("render");
        assert_eq!(
            s,
            "Pay inv-1001 of 7250.0 to Northwind Dairy [\"a\",\"b\"] 5"
        );
        let err = render_string("{{input.nope}}", &context()).expect_err("missing");
        assert_eq!(err.code, "template_missing_path");
        assert_eq!(err.path, "input.nope");
        assert_eq!(
            render_string("no templates {{", &context()).expect("render"),
            "no templates {{"
        );
    }

    #[test]
    fn resolves_refs_preserving_types() {
        let args = json!({"amount": {"$ref": "input.total"}, "vendor": {"$ref": "steps.extract.output.vendor"},
                          "nested": {"list": [{"$ref": "input.lines.0"}], "text": "id {{input.invoice_id}}"},
                          "keep": {"$ref": 1, "x": 2}});
        let resolved = resolve(&args, &context()).expect("resolve");
        assert_eq!(resolved["amount"], json!(7250.0));
        assert_eq!(resolved["vendor"], json!("Northwind Dairy"));
        assert_eq!(resolved["nested"]["list"][0], json!({"amount": 5}));
        assert_eq!(resolved["nested"]["text"], json!("id inv-1001"));
        assert_eq!(resolved["keep"], json!({"$ref": 1, "x": 2}));
        let err = resolve(&json!({"$ref": "steps.code.output.account"}), &context())
            .expect_err("missing");
        assert_eq!(err.path, "steps.code.output.account");
    }

    #[test]
    fn collects_paths_for_validation() {
        let mut paths = Vec::new();
        collect_paths(
            &json!({"a": {"$ref": "input.x"}, "b": ["{{steps.s.output.y}} and {{run.id}}"]}),
            &mut paths,
        )
        .expect("ok");
        assert_eq!(paths, vec!["input.x", "steps.s.output.y", "run.id"]);
        let err = collect_paths(&json!({"$ref": 5}), &mut paths).expect_err("bad ref");
        assert_eq!(err.code, "template_bad_ref");
    }
}
