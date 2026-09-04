//! Runtime values of the policy language and the conversion from JSON.

use serde_json::Value as Json;

/// A value produced while evaluating an expression. Objects exist only so that a
/// path resolving to a JSON object can still take part in `==` and `!=` and be
/// truthy; the language has no object literals.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `null`, also the value of every missing path.
    Null,
    /// A boolean.
    Bool(bool),
    /// A number; JSON integers are widened to f64.
    Number(f64),
    /// A string.
    Str(String),
    /// A list of values.
    List(Vec<Value>),
    /// A JSON object reached through a path.
    Object(serde_json::Map<String, Json>),
}

impl Value {
    /// Truthiness used by rule matching, `and`, `or` and `not`: `null` and `false`
    /// are false, as are zero, the empty string and the empty list.
    pub fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => *n != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::List(items) => !items.is_empty(),
            Value::Object(_) => true,
        }
    }

    /// The string inside, if this is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Converts a JSON value into a policy value.
    pub fn from_json(json: &Json) -> Value {
        match json {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(*b),
            Json::Number(n) => n.as_f64().map(Value::Number).unwrap_or(Value::Null),
            Json::String(s) => Value::Str(s.clone()),
            Json::Array(items) => Value::List(items.iter().map(Value::from_json).collect()),
            Json::Object(map) => Value::Object(map.clone()),
        }
    }

    /// Converts back to JSON, used when reporting evaluated values.
    pub fn to_json(&self) -> Json {
        match self {
            Value::Null => Json::Null,
            Value::Bool(b) => Json::Bool(*b),
            Value::Number(n) => serde_json::Number::from_f64(*n)
                .map(Json::Number)
                .unwrap_or(Json::Null),
            Value::Str(s) => Json::String(s.clone()),
            Value::List(items) => Json::Array(items.iter().map(Value::to_json).collect()),
            Value::Object(map) => Json::Object(map.clone()),
        }
    }
}

/// Looks a dotted path up in a JSON context. Numeric segments index lists. A
/// missing segment yields `Null`, which is what makes evaluation total.
pub fn lookup<'a>(context: &'a Json, path: &[String]) -> Option<&'a Json> {
    let mut current = context;
    for segment in path {
        current = match current {
            Json::Object(map) => map.get(segment)?,
            Json::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}
