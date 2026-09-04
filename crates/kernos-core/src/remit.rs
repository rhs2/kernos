//! Remits: the signed capability token (`krt1.`) and delegation that only narrows.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::canonical::canonical_bytes;
use crate::keys::{KeyPair, PublicKey};

/// The token format version.
pub const TOKEN_PREFIX: &str = "krt1";

/// Default remit lifetime when `ttl_seconds` is omitted.
pub const DEFAULT_TTL_SECONDS: u64 = 86400;

/// Autonomy levels, ordered `observe < propose < supervised < autonomous`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Autonomy {
    /// Read operations only.
    Observe,
    /// Reads, and proposals that always need approval for writes.
    Propose,
    /// Writes allowed when policy allows; policies usually gate them.
    Supervised,
    /// Writes allowed when policy allows.
    Autonomous,
}

impl Autonomy {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Autonomy::Observe => "observe",
            Autonomy::Propose => "propose",
            Autonomy::Supervised => "supervised",
            Autonomy::Autonomous => "autonomous",
        }
    }

    /// Parses the wire spelling.
    pub fn parse(text: &str) -> Option<Autonomy> {
        match text {
            "observe" => Some(Autonomy::Observe),
            "propose" => Some(Autonomy::Propose),
            "supervised" => Some(Autonomy::Supervised),
            "autonomous" => Some(Autonomy::Autonomous),
            _ => None,
        }
    }
}

/// The spend ceiling of a remit. `None` means unlimited.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Spend {
    /// Token ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// Currency ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd: Option<f64>,
}

/// The signed payload of 03-REMIT.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemitPayload {
    /// Remit id.
    pub rid: String,
    /// Parent remit, for derived remits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The run this remit is bound to, for run-bound child remits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// Issuing key id.
    pub iss: String,
    /// Issued at, epoch seconds.
    pub iat: i64,
    /// Not before, epoch seconds.
    pub nbf: i64,
    /// Expiry, epoch seconds.
    pub exp: i64,
    /// Tool patterns.
    pub tools: Vec<String>,
    /// Scope patterns.
    pub scopes: Vec<String>,
    /// Data-class grants.
    pub grants: Vec<String>,
    /// Spend ceiling.
    pub spend: Spend,
    /// Autonomy level.
    pub autonomy: Autonomy,
    /// Policies that apply.
    pub policy_set: Vec<String>,
    /// Who asked for the remit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<Value>,
}

/// Body of `POST /v1/remits`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct IssueRequest {
    /// Tool patterns (required).
    pub tools: Vec<String>,
    /// Scope patterns.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Grants.
    #[serde(default)]
    pub grants: Vec<String>,
    /// Spend ceiling.
    #[serde(default)]
    pub spend: Spend,
    /// Autonomy; `observe` when omitted.
    #[serde(default)]
    pub autonomy: Option<Autonomy>,
    /// Lifetime; 24 hours when omitted.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    /// Policies.
    #[serde(default)]
    pub policy_set: Vec<String>,
    /// Requester.
    #[serde(default)]
    pub requested_by: Option<Value>,
}

/// Body of `POST /v1/remits/{id}/derive`: every field optional, omitted fields
/// are inherited.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DeriveRequest {
    /// Narrower tool patterns.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// Narrower scope patterns.
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    /// A subset of grants.
    #[serde(default)]
    pub grants: Option<Vec<String>>,
    /// A lower spend.
    #[serde(default)]
    pub spend: Option<Spend>,
    /// A lower autonomy.
    #[serde(default)]
    pub autonomy: Option<Autonomy>,
    /// A shorter lifetime.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    /// A superset of policies.
    #[serde(default)]
    pub policy_set: Option<Vec<String>>,
}

/// A derivation that would widen the parent, naming the field.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{field} widens the parent remit: {message}")]
pub struct WidensError {
    /// The offending field (`tools`, `spend.usd`, `autonomy`, ...).
    pub field: String,
    /// What was wider.
    pub message: String,
}

/// Why a token failed verification; the names are the refusal reasons of 03-REMIT.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TokenError {
    /// Wrong prefix, wrong part count or undecodable parts.
    #[error("token_malformed")]
    Malformed,
    /// The key id is unknown or the signature does not verify.
    #[error("signature_invalid")]
    SignatureInvalid,
    /// `exp` has passed.
    #[error("remit_expired")]
    Expired,
    /// `nbf` has not arrived.
    #[error("remit_not_yet_valid")]
    NotYetValid,
}

impl TokenError {
    /// The refusal reason string.
    pub fn reason(&self) -> &'static str {
        match self {
            TokenError::Malformed => "token_malformed",
            TokenError::SignatureInvalid => "signature_invalid",
            TokenError::Expired => "remit_expired",
            TokenError::NotYetValid => "remit_not_yet_valid",
        }
    }
}

/// Encodes and signs a payload as `krt1.<payload>.<signature>.<key_id>`. The
/// signature covers the exact canonical payload bytes that are base64url-encoded.
pub fn encode_token(payload: &RemitPayload, key: &KeyPair) -> serde_json::Result<String> {
    let value = serde_json::to_value(payload)?;
    let bytes = canonical_bytes(&value);
    let signature = key.sign(&bytes);
    Ok(format!(
        "{TOKEN_PREFIX}.{}.{}.{}",
        URL_SAFE_NO_PAD.encode(&bytes),
        signature,
        key.key_id
    ))
}

/// The parts of a token before signature verification.
#[derive(Debug, Clone)]
pub struct DecodedToken {
    /// The payload.
    pub payload: RemitPayload,
    /// The canonical bytes that were signed.
    pub payload_bytes: Vec<u8>,
    /// The base64url signature.
    pub signature: String,
    /// The key id.
    pub key_id: String,
}

/// Splits and decodes a token without verifying it.
pub fn decode_token(token: &str) -> Result<DecodedToken, TokenError> {
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() != 4 || parts[0] != TOKEN_PREFIX {
        return Err(TokenError::Malformed);
    }
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|_| TokenError::Malformed)?;
    if URL_SAFE_NO_PAD.decode(parts[2]).is_err() || parts[3].is_empty() {
        return Err(TokenError::Malformed);
    }
    let payload: RemitPayload =
        serde_json::from_slice(&payload_bytes).map_err(|_| TokenError::Malformed)?;
    Ok(DecodedToken {
        payload,
        payload_bytes,
        signature: parts[2].to_string(),
        key_id: parts[3].to_string(),
    })
}

/// Verifies a token against the keys the resolver knows and the clock: the
/// checks 1 to 3 of 03-REMIT, in order.
pub fn verify_token(
    token: &str,
    resolve_key: impl Fn(&str) -> Option<PublicKey>,
    now_seconds: i64,
) -> Result<RemitPayload, TokenError> {
    let decoded = decode_token(token)?;
    let key = resolve_key(&decoded.key_id).ok_or(TokenError::SignatureInvalid)?;
    if !key.verify(&decoded.payload_bytes, &decoded.signature) {
        return Err(TokenError::SignatureInvalid);
    }
    if now_seconds < decoded.payload.nbf {
        return Err(TokenError::NotYetValid);
    }
    if now_seconds >= decoded.payload.exp {
        return Err(TokenError::Expired);
    }
    Ok(decoded.payload)
}

/// Exact-or-glob matching of a tool or scope pattern against an identifier: a
/// pattern is literal, or ends in `*` and matches any identifier with that prefix.
pub fn pattern_matches(pattern: &str, id: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => id.starts_with(prefix),
        None => pattern == id,
    }
}

/// True when everything the child pattern can match, the parent pattern also
/// matches. `ledger.post_entry` under `ledger.*` is fine; `ledger.*` under
/// `ledger.post_entry` is not.
pub fn pattern_covers(parent: &str, child: &str) -> bool {
    match (parent.strip_suffix('*'), child.strip_suffix('*')) {
        (Some(parent_prefix), Some(child_prefix)) => child_prefix.starts_with(parent_prefix),
        (Some(parent_prefix), None) => child.starts_with(parent_prefix),
        (None, Some(_)) => false,
        (None, None) => parent == child,
    }
}

/// The narrowed fields of a child remit, ready to be given an id and signed.
#[derive(Debug, Clone, PartialEq)]
pub struct Narrowed {
    /// Tools.
    pub tools: Vec<String>,
    /// Scopes.
    pub scopes: Vec<String>,
    /// Grants.
    pub grants: Vec<String>,
    /// Spend.
    pub spend: Spend,
    /// Autonomy.
    pub autonomy: Autonomy,
    /// Expiry, epoch seconds.
    pub exp: i64,
    /// Not before, epoch seconds.
    pub nbf: i64,
    /// Policies.
    pub policy_set: Vec<String>,
}

/// Applies the narrowing rules of 03-REMIT to a derive request. Every given
/// field must be a subset of the parent; omitted fields are inherited.
pub fn narrow(
    parent: &RemitPayload,
    request: &DeriveRequest,
    now_seconds: i64,
) -> Result<Narrowed, WidensError> {
    let widen = |field: &str, message: String| WidensError {
        field: field.to_string(),
        message,
    };

    let tools = match &request.tools {
        Some(tools) => {
            for child in tools {
                if !parent.tools.iter().any(|p| pattern_covers(p, child)) {
                    return Err(widen(
                        "tools",
                        format!("{child} is not covered by {:?}", parent.tools),
                    ));
                }
            }
            tools.clone()
        }
        None => parent.tools.clone(),
    };
    let scopes = match &request.scopes {
        Some(scopes) => {
            for child in scopes {
                if !parent.scopes.iter().any(|p| pattern_covers(p, child)) {
                    return Err(widen(
                        "scopes",
                        format!("{child} is not covered by {:?}", parent.scopes),
                    ));
                }
            }
            scopes.clone()
        }
        None => parent.scopes.clone(),
    };
    let grants = match &request.grants {
        Some(grants) => {
            for grant in grants {
                if !parent.grants.contains(grant) {
                    return Err(widen(
                        "grants",
                        format!("{grant} is not granted by the parent"),
                    ));
                }
            }
            grants.clone()
        }
        None => parent.grants.clone(),
    };
    let spend = match &request.spend {
        Some(spend) => {
            let tokens = match (spend.tokens, parent.spend.tokens) {
                (Some(child), Some(limit)) if child > limit => {
                    return Err(widen(
                        "spend.tokens",
                        format!("{child} exceeds the parent's {limit}"),
                    ))
                }
                (None, Some(limit)) => Some(limit),
                (child, _) => child,
            };
            let usd = match (spend.usd, parent.spend.usd) {
                (Some(child), Some(limit)) if child > limit => {
                    return Err(widen(
                        "spend.usd",
                        format!("{child} exceeds the parent's {limit}"),
                    ))
                }
                (None, Some(limit)) => Some(limit),
                (child, _) => child,
            };
            Spend { tokens, usd }
        }
        None => parent.spend,
    };
    let autonomy = match request.autonomy {
        Some(level) => {
            if level > parent.autonomy {
                return Err(widen(
                    "autonomy",
                    format!(
                        "{} is above the parent's {}",
                        level.as_str(),
                        parent.autonomy.as_str()
                    ),
                ));
            }
            level
        }
        None => parent.autonomy,
    };
    let nbf = now_seconds.max(parent.nbf);
    let exp = match request.ttl_seconds {
        Some(ttl) => {
            let exp = now_seconds.saturating_add(ttl as i64);
            if exp > parent.exp {
                return Err(widen(
                    "ttl_seconds",
                    format!("expiry {exp} is after the parent's {}", parent.exp),
                ));
            }
            exp
        }
        None => parent.exp,
    };
    let policy_set = match &request.policy_set {
        Some(set) => {
            for policy in &parent.policy_set {
                if !set.contains(policy) {
                    return Err(widen("policy_set", format!("{policy} was removed")));
                }
            }
            set.clone()
        }
        None => parent.policy_set.clone(),
    };
    Ok(Narrowed {
        tools,
        scopes,
        grants,
        spend,
        autonomy,
        exp,
        nbf,
        policy_set,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent() -> RemitPayload {
        RemitPayload {
            rid: "rem_parent".into(),
            parent: None,
            run: None,
            iss: "key_x".into(),
            iat: 1_000,
            nbf: 1_000,
            exp: 87_400,
            tools: vec!["ledger.*".into(), "http.get".into()],
            scopes: vec!["sql:table:*".into(), "http:host:api.halcyon.example".into()],
            grants: vec!["pii".into()],
            spend: Spend {
                tokens: Some(200_000),
                usd: Some(2.0),
            },
            autonomy: Autonomy::Supervised,
            policy_set: vec!["finance-default".into()],
            requested_by: None,
        }
    }

    #[test]
    fn token_round_trip_and_verification_order() {
        let key = KeyPair::generate(1);
        let token = encode_token(&parent(), &key).expect("encode");
        assert!(token.starts_with("krt1."));
        assert_eq!(token.split('.').count(), 4);
        let resolver = |id: &str| (id == key.key_id).then(|| key.public());
        let payload = verify_token(&token, resolver, 5_000).expect("verify");
        assert_eq!(payload, parent());
        assert_eq!(
            verify_token(&token, resolver, 500),
            Err(TokenError::NotYetValid)
        );
        assert_eq!(
            verify_token(&token, resolver, 87_400),
            Err(TokenError::Expired)
        );
        assert_eq!(
            verify_token(&token, |_| None, 5_000),
            Err(TokenError::SignatureInvalid)
        );
        let other = KeyPair::generate(2);
        let wrong = |id: &str| (id == key.key_id).then(|| other.public());
        assert_eq!(
            verify_token(&token, wrong, 5_000),
            Err(TokenError::SignatureInvalid)
        );
        assert_eq!(
            verify_token("krt2.a.b.c", resolver, 5_000),
            Err(TokenError::Malformed)
        );
        assert_eq!(
            verify_token("krt1.a.b", resolver, 5_000),
            Err(TokenError::Malformed)
        );
        let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
        let replacement = if &parts[1][0..1] == "e" { "f" } else { "e" };
        parts[1].replace_range(0..1, replacement);
        let tampered = parts.join(".");
        assert!(matches!(
            verify_token(&tampered, resolver, 5_000),
            Err(TokenError::SignatureInvalid) | Err(TokenError::Malformed)
        ));
    }

    #[test]
    fn patterns() {
        assert!(pattern_matches("ledger.*", "ledger.post_entry"));
        assert!(pattern_matches("ledger.post_entry", "ledger.post_entry"));
        assert!(!pattern_matches("ledger.post_entry", "ledger.void_entry"));
        assert!(!pattern_matches("ledger.*", "http.get"));
        assert!(pattern_covers("ledger.*", "ledger.post_entry"));
        assert!(pattern_covers("ledger.*", "ledger.*"));
        assert!(pattern_covers("ledger.*", "ledger.p*"));
        assert!(!pattern_covers("ledger.post_entry", "ledger.*"));
        assert!(!pattern_covers("ledger.p*", "ledger.*"));
        assert!(pattern_covers("*", "anything.*"));
    }

    #[test]
    fn narrowing_rules() {
        let p = parent();
        let ok = narrow(
            &p,
            &DeriveRequest {
                tools: Some(vec!["ledger.post_entry".into()]),
                spend: Some(Spend {
                    tokens: None,
                    usd: Some(1.0),
                }),
                autonomy: Some(Autonomy::Propose),
                ttl_seconds: Some(100),
                policy_set: Some(vec!["finance-default".into(), "extra".into()]),
                ..Default::default()
            },
            2_000,
        )
        .expect("narrow");
        assert_eq!(ok.tools, vec!["ledger.post_entry"]);
        assert_eq!(
            ok.spend,
            Spend {
                tokens: Some(200_000),
                usd: Some(1.0)
            }
        );
        assert_eq!(ok.autonomy, Autonomy::Propose);
        assert_eq!(ok.exp, 2_100);
        assert_eq!(ok.scopes, p.scopes);
        assert_eq!(ok.policy_set.len(), 2);

        let field = |req: DeriveRequest| narrow(&p, &req, 2_000).expect_err("widens").field;
        assert_eq!(
            field(DeriveRequest {
                tools: Some(vec!["*".into()]),
                ..Default::default()
            }),
            "tools"
        );
        assert_eq!(
            field(DeriveRequest {
                tools: Some(vec!["crm.read".into()]),
                ..Default::default()
            }),
            "tools"
        );
        assert_eq!(
            field(DeriveRequest {
                scopes: Some(vec!["fs:path:/".into()]),
                ..Default::default()
            }),
            "scopes"
        );
        assert_eq!(
            field(DeriveRequest {
                grants: Some(vec!["phi".into()]),
                ..Default::default()
            }),
            "grants"
        );
        assert_eq!(
            field(DeriveRequest {
                spend: Some(Spend {
                    tokens: None,
                    usd: Some(3.0)
                }),
                ..Default::default()
            }),
            "spend.usd"
        );
        assert_eq!(
            field(DeriveRequest {
                spend: Some(Spend {
                    tokens: Some(300_000),
                    usd: None
                }),
                ..Default::default()
            }),
            "spend.tokens"
        );
        assert_eq!(
            field(DeriveRequest {
                autonomy: Some(Autonomy::Autonomous),
                ..Default::default()
            }),
            "autonomy"
        );
        assert_eq!(
            field(DeriveRequest {
                ttl_seconds: Some(1_000_000),
                ..Default::default()
            }),
            "ttl_seconds"
        );
        assert_eq!(
            field(DeriveRequest {
                policy_set: Some(vec![]),
                ..Default::default()
            }),
            "policy_set"
        );
        // A narrower child of a narrower child.
        let child = RemitPayload {
            tools: vec!["ledger.post_entry".into()],
            ..p.clone()
        };
        assert_eq!(
            narrow(
                &child,
                &DeriveRequest {
                    tools: Some(vec!["ledger.*".into()]),
                    ..Default::default()
                },
                2_000
            )
            .expect_err("w")
            .field,
            "tools"
        );
    }

    #[test]
    fn autonomy_order_and_serialisation() {
        assert!(Autonomy::Observe < Autonomy::Propose);
        assert!(Autonomy::Propose < Autonomy::Supervised);
        assert!(Autonomy::Supervised < Autonomy::Autonomous);
        assert_eq!(
            serde_json::to_value(Autonomy::Supervised).expect("json"),
            serde_json::json!("supervised")
        );
        assert_eq!(Autonomy::parse("autonomous"), Some(Autonomy::Autonomous));
        let json = serde_json::to_value(parent()).expect("json");
        assert!(json.get("parent").is_none());
        assert!(json.get("run").is_none());
        assert_eq!(json["spend"]["usd"], 2.0);
    }
}
