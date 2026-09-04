//! The JSON Schema subset of 05-BUNDLE: `type`, `required`, `properties`,
//! `items`, `enum`, `minimum`, `maximum`, `minLength`, `maxLength`, `pattern`,
//! `additionalProperties`. Implemented here rather than pulled in as a crate so
//! that `details.path` is exactly the dotted path the specs promise.

use serde_json::Value;

/// The first violation found, with the dotted path of the offending value
/// (empty for the root) and a human message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaViolation {
    /// Dotted path such as `lines.0.amount`; empty for the root value.
    pub path: String,
    /// What was wrong.
    pub message: String,
}

/// Validates a value against a schema of the supported subset. Unknown keywords
/// are ignored, as JSON Schema requires.
pub fn validate(schema: &Value, value: &Value) -> Result<(), SchemaViolation> {
    validate_at(schema, value, &mut Vec::new())
}

fn join(path: &[String]) -> String {
    path.join(".")
}

fn validate_at(
    schema: &Value,
    value: &Value,
    path: &mut Vec<String>,
) -> Result<(), SchemaViolation> {
    let Some(schema) = schema.as_object() else {
        // `true` or a non-object schema accepts everything; `false` rejects.
        if schema == &Value::Bool(false) {
            return Err(SchemaViolation {
                path: join(path),
                message: "value not allowed".into(),
            });
        }
        return Ok(());
    };
    let fail = |path: &[String], message: String| SchemaViolation {
        path: join(path),
        message,
    };

    if let Some(expected) = schema.get("type") {
        let names: Vec<&str> = match expected {
            Value::String(s) => vec![s.as_str()],
            Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        if !names.is_empty() && !names.iter().any(|name| type_matches(name, value)) {
            return Err(fail(
                path,
                format!(
                    "expected type {} but found {}",
                    names.join(" or "),
                    type_name(value)
                ),
            ));
        }
    }

    if let Some(Value::Array(options)) = schema.get("enum") {
        if !options.iter().any(|o| o == value) {
            return Err(fail(path, "value is not one of the allowed values".into()));
        }
    }

    if let Some(number) = value.as_f64() {
        if let Some(min) = schema.get("minimum").and_then(Value::as_f64) {
            if number < min {
                return Err(fail(path, format!("{number} is below the minimum {min}")));
            }
        }
        if let Some(max) = schema.get("maximum").and_then(Value::as_f64) {
            if number > max {
                return Err(fail(path, format!("{number} is above the maximum {max}")));
            }
        }
    }

    if let Some(text) = value.as_str() {
        let length = text.chars().count() as u64;
        if let Some(min) = schema.get("minLength").and_then(Value::as_u64) {
            if length < min {
                return Err(fail(path, format!("string shorter than minLength {min}")));
            }
        }
        if let Some(max) = schema.get("maxLength").and_then(Value::as_u64) {
            if length > max {
                return Err(fail(path, format!("string longer than maxLength {max}")));
            }
        }
        if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
            match regex::Regex::new(pattern) {
                Ok(re) => {
                    if !re.is_match(text) {
                        return Err(fail(
                            path,
                            format!("string does not match pattern {pattern}"),
                        ));
                    }
                }
                Err(_) => {
                    return Err(fail(
                        path,
                        format!("schema pattern {pattern} is not a valid regex"),
                    ))
                }
            }
        }
    }

    if let Some(object) = value.as_object() {
        if let Some(Value::Array(required)) = schema.get("required") {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    path.push(key.to_string());
                    let err = fail(path, format!("missing required property {key}"));
                    path.pop();
                    return Err(err);
                }
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        for (key, item) in object {
            path.push(key.clone());
            let result = match properties.and_then(|p| p.get(key)) {
                Some(sub) => validate_at(sub, item, path),
                None => match schema.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        Err(fail(path, format!("unexpected property {key}")))
                    }
                    Some(sub @ Value::Object(_)) => validate_at(sub, item, path),
                    _ => Ok(()),
                },
            };
            path.pop();
            result?;
        }
    }

    if let Some(items) = value.as_array() {
        if let Some(item_schema) = schema.get("items") {
            for (i, item) in items.iter().enumerate() {
                path.push(i.to_string());
                let result = validate_at(item_schema, item, path);
                path.pop();
                result?;
            }
        }
    }

    Ok(())
}

fn type_matches(name: &str, value: &Value) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn intake_schema() -> Value {
        json!({"type": "object", "required": ["invoice_id", "text", "total"],
               "properties": {"invoice_id": {"type": "string"}, "text": {"type": "string", "minLength": 1},
                              "total": {"type": "number", "minimum": 0},
                              "accounts": {"type": "array", "items": {"type": "string", "pattern": "^[0-9]{4}$"}},
                              "kind": {"enum": ["a", "b"]}},
               "additionalProperties": false})
    }

    #[test]
    fn accepts_valid_input() {
        let input = json!({"invoice_id": "inv-1001", "text": "Milk", "total": 7250.0, "accounts": ["5100"], "kind": "a"});
        assert!(validate(&intake_schema(), &input).is_ok());
    }

    #[test]
    fn reports_the_offending_path() {
        let schema = intake_schema();
        let err = validate(&schema, &json!({"invoice_id": "x", "text": "y"})).expect_err("missing");
        assert_eq!(err.path, "total");
        let err = validate(
            &schema,
            &json!({"invoice_id": "x", "text": "y", "total": "7"}),
        )
        .expect_err("type");
        assert_eq!(err.path, "total");
        let err = validate(&schema, &json!({"invoice_id": "x", "text": "", "total": 1}))
            .expect_err("minLength");
        assert_eq!(err.path, "text");
        let err = validate(
            &schema,
            &json!({"invoice_id": "x", "text": "y", "total": -1}),
        )
        .expect_err("minimum");
        assert_eq!(err.path, "total");
        let err = validate(
            &schema,
            &json!({"invoice_id": "x", "text": "y", "total": 1, "accounts": ["51"]}),
        )
        .expect_err("pattern");
        assert_eq!(err.path, "accounts.0");
        let err = validate(
            &schema,
            &json!({"invoice_id": "x", "text": "y", "total": 1, "kind": "c"}),
        )
        .expect_err("enum");
        assert_eq!(err.path, "kind");
        let err = validate(
            &schema,
            &json!({"invoice_id": "x", "text": "y", "total": 1, "extra": 1}),
        )
        .expect_err("extra");
        assert_eq!(err.path, "extra");
        let err = validate(&schema, &json!("not an object")).expect_err("root");
        assert_eq!(err.path, "");
    }

    #[test]
    fn integer_and_type_lists_and_max() {
        let schema = json!({"type": ["integer", "null"], "maximum": 10});
        assert!(validate(&schema, &json!(3)).is_ok());
        assert!(validate(&schema, &json!(3.0)).is_ok());
        assert!(validate(&schema, &Value::Null).is_ok());
        assert!(validate(&schema, &json!(3.5)).is_err());
        assert!(validate(&schema, &json!(11)).is_err());
        let schema = json!({"type": "string", "maxLength": 2});
        assert!(validate(&schema, &json!("abc")).is_err());
        assert!(validate(&json!({}), &json!({"anything": true})).is_ok());
    }
}
