//! Canonical JSON, implemented once for the whole system.
//!
//! Rules (00-OVERVIEW): object keys sorted by UTF-16 code units, no insignificant
//! whitespace, strings escaped with the minimal RFC 8259 escapes, integers
//! without exponent, non-integer numbers as the shortest round-trip form. Only the
//! kernel hashes and signs, so every other language treats the results as opaque.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Serialises a JSON value in canonical form.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out);
    out
}

/// Canonical form as bytes, the input to every hash and signature.
pub fn canonical_bytes(value: &Value) -> Vec<u8> {
    canonical_json(value).into_bytes()
}

/// Lowercase hex SHA-256 of arbitrary bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// SHA-256 of the canonical form of a value; the hash used for actions, bundle
/// content identity and truncated payload references.
pub fn hash_value(value: &Value) -> String {
    sha256_hex(&canonical_bytes(value))
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by_key(|(key, _)| utf16_units(key));
            out.push('{');
            for (i, (key, item)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_value(item, out);
            }
            out.push('}');
        }
    }
}

fn utf16_units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_keys_and_strips_whitespace() {
        let v = json!({"b": 1, "a": {"z": [1, 2, {"y": null}], "c": true}});
        assert_eq!(
            canonical_json(&v),
            r#"{"a":{"c":true,"z":[1,2,{"y":null}]},"b":1}"#
        );
    }

    #[test]
    fn sorts_by_utf16_units_not_bytes() {
        // U+FF5E (BMP, one unit 0xFF5E) sorts before U+1F600 (surrogates 0xD83D 0xDE00)
        // by UTF-16 units, while byte order would put U+FF5E after.
        let v = json!({"\u{1F600}": 1, "\u{FF5E}": 2});
        assert_eq!(canonical_json(&v), "{\"\u{1F600}\":1,\"\u{FF5E}\":2}");
    }

    #[test]
    fn minimal_escapes_and_numbers() {
        let v = json!({"s": "a\"b\\c\n\t\u{1}\u{e9}", "i": 7250, "f": 7250.5, "n": -3, "z": 0.1});
        assert_eq!(
            canonical_json(&v),
            "{\"f\":7250.5,\"i\":7250,\"n\":-3,\"s\":\"a\\\"b\\\\c\\n\\t\\u0001\u{e9}\",\"z\":0.1}"
        );
    }

    #[test]
    fn hash_is_stable() {
        let v = json!({"x": 1});
        assert_eq!(hash_value(&v), sha256_hex(b"{\"x\":1}"));
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
