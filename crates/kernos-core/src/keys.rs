//! Ed25519 keys: the control-plane signing key and trusted publisher keys.
//!
//! File formats accepted, so that keys made by other tooling still load:
//! - Kernos JSON: `{"key_id", "algorithm": "ed25519", "public_key": <base64url>}`
//!   for public keys, plus `"private_key": <base64url 32-byte seed>` for private ones.
//! - PEM: PKCS#8 (`BEGIN PRIVATE KEY`) and SPKI (`BEGIN PUBLIC KEY`).
//! - Raw: a base64url, base64 or hex string of the 32-byte seed or public key.
//!
//! When the file carries no key id, the file name without its extension is used.

use std::fs;
use std::path::Path;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use ed25519_dalek::pkcs8::{DecodePrivateKey, DecodePublicKey};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde_json::{json, Value};

use crate::error::{KernelError, KernelResult};
use crate::ids::new_id;

/// A signing key with its identifier.
#[derive(Clone)]
pub struct KeyPair {
    /// The `key_...` identifier.
    pub key_id: String,
    /// The private key.
    pub signing: SigningKey,
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyPair")
            .field("key_id", &self.key_id)
            .finish()
    }
}

/// A verification key with its identifier.
#[derive(Debug, Clone)]
pub struct PublicKey {
    /// The `key_...` identifier.
    pub key_id: String,
    /// The public key.
    pub verifying: VerifyingKey,
}

impl KeyPair {
    /// Generates a fresh key pair with a new identifier.
    pub fn generate(now_ms: i64) -> Self {
        let mut seed = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut seed);
        KeyPair {
            key_id: new_id("key", now_ms),
            signing: SigningKey::from_bytes(&seed),
        }
    }

    /// The matching public key.
    pub fn public(&self) -> PublicKey {
        PublicKey {
            key_id: self.key_id.clone(),
            verifying: self.signing.verifying_key(),
        }
    }

    /// Signs bytes and returns the base64url (no padding) signature.
    pub fn sign(&self, bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(self.signing.sign(bytes).to_bytes())
    }

    /// The JSON file form, including the private seed.
    pub fn to_json(&self) -> Value {
        json!({
            "key_id": self.key_id,
            "algorithm": "ed25519",
            "public_key": encode_key(self.signing.verifying_key().as_bytes()),
            "private_key": encode_key(&self.signing.to_bytes()),
        })
    }

    /// Writes the private key file with mode 0600 (on Unix).
    pub fn write_private(&self, path: &Path) -> KernelResult<()> {
        write_private_file(path, &serde_json::to_vec_pretty(&self.to_json())?)
    }

    /// Writes the public key file.
    pub fn write_public(&self, path: &Path) -> KernelResult<()> {
        self.public().write(path)
    }

    /// Loads a private key from any accepted format.
    pub fn load(path: &Path) -> KernelResult<Self> {
        let text = fs::read_to_string(path)?;
        let stem = file_stem(path);
        if let Ok(json) = serde_json::from_str::<Value>(&text) {
            let key_id = json
                .get("key_id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| stem.clone());
            let seed_text = json
                .get("private_key")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    KernelError::bad_request("key_invalid", "private key file has no private_key")
                })?;
            let seed = decode_key(seed_text)?;
            return Ok(KeyPair {
                key_id,
                signing: signing_from_bytes(&seed)?,
            });
        }
        if text.contains("-----BEGIN") {
            let signing = SigningKey::from_pkcs8_pem(&text).map_err(|e| {
                KernelError::bad_request("key_invalid", format!("PEM private key: {e}"))
            })?;
            return Ok(KeyPair {
                key_id: stem,
                signing,
            });
        }
        let seed = decode_key(text.trim())?;
        Ok(KeyPair {
            key_id: stem,
            signing: signing_from_bytes(&seed)?,
        })
    }
}

impl PublicKey {
    /// Verifies a base64url signature over bytes.
    pub fn verify(&self, bytes: &[u8], signature_b64: &str) -> bool {
        let Ok(sig_bytes) = URL_SAFE_NO_PAD.decode(signature_b64) else {
            return false;
        };
        let Ok(signature) = Signature::from_slice(&sig_bytes) else {
            return false;
        };
        self.verifying.verify(bytes, &signature).is_ok()
    }

    /// The base64url public key bytes, as `GET /v1/keys` reports them.
    pub fn public_key_b64(&self) -> String {
        encode_key(self.verifying.as_bytes())
    }

    /// The JSON file form.
    pub fn to_json(&self) -> Value {
        json!({"key_id": self.key_id, "algorithm": "ed25519", "public_key": self.public_key_b64()})
    }

    /// Writes the public key file.
    pub fn write(&self, path: &Path) -> KernelResult<()> {
        fs::write(path, serde_json::to_vec_pretty(&self.to_json())?)?;
        Ok(())
    }

    /// Loads a public key from any accepted format.
    pub fn load(path: &Path) -> KernelResult<Self> {
        let text = fs::read_to_string(path)?;
        let stem = file_stem(path);
        if let Ok(json) = serde_json::from_str::<Value>(&text) {
            let key_id = json
                .get("key_id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| stem.clone());
            let key_text = json
                .get("public_key")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    KernelError::bad_request("key_invalid", "public key file has no public_key")
                })?;
            return Ok(PublicKey {
                key_id,
                verifying: verifying_from_bytes(&decode_key(key_text)?)?,
            });
        }
        if text.contains("-----BEGIN") {
            let verifying = VerifyingKey::from_public_key_pem(&text).map_err(|e| {
                KernelError::bad_request("key_invalid", format!("PEM public key: {e}"))
            })?;
            return Ok(PublicKey {
                key_id: stem,
                verifying,
            });
        }
        Ok(PublicKey {
            key_id: stem,
            verifying: verifying_from_bytes(&decode_key(text.trim())?)?,
        })
    }

    /// Loads every `*.pub` file in a directory; a missing directory is empty.
    /// Unreadable files are skipped with a warning rather than blocking start.
    pub fn load_dir(dir: &Path) -> Vec<PublicKey> {
        let mut keys = Vec::new();
        let Ok(entries) = fs::read_dir(dir) else {
            return keys;
        };
        let mut paths: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if path.extension().and_then(|e| e.to_str()) != Some("pub") {
                continue;
            }
            match PublicKey::load(&path) {
                Ok(key) => keys.push(key),
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "skipping unreadable trusted key")
                }
            }
        }
        keys
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("key")
        .to_string()
}

fn encode_key(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes base64url (with or without padding), standard base64 or hex.
pub fn decode_key(text: &str) -> KernelResult<Vec<u8>> {
    let text = text.trim().trim_end_matches('=');
    // Hex first: a 64-character hex string is also valid base64, so the
    // unambiguous form must win.
    if (text.len() == 64 || text.len() == 128) && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        let mut out = Vec::with_capacity(text.len() / 2);
        for i in (0..text.len()).step_by(2) {
            let byte = u8::from_str_radix(&text[i..i + 2], 16)
                .map_err(|_| KernelError::bad_request("key_invalid", "bad hex"))?;
            out.push(byte);
        }
        return Ok(out);
    }
    if let Ok(bytes) = URL_SAFE_NO_PAD.decode(text) {
        return Ok(bytes);
    }
    if let Ok(bytes) = STANDARD.decode(text) {
        return Ok(bytes);
    }
    Err(KernelError::bad_request(
        "key_invalid",
        "key is not base64url, base64 or hex",
    ))
}

fn signing_from_bytes(bytes: &[u8]) -> KernelResult<SigningKey> {
    match bytes.len() {
        32 | 64 => {
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&bytes[..32]);
            Ok(SigningKey::from_bytes(&seed))
        }
        n => Err(KernelError::bad_request(
            "key_invalid",
            format!("private key must be 32 or 64 bytes, got {n}"),
        )),
    }
}

/// Builds a verifying key from raw bytes.
pub fn verifying_from_bytes(bytes: &[u8]) -> KernelResult<VerifyingKey> {
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| KernelError::bad_request("key_invalid", "public key must be 32 bytes"))?;
    VerifyingKey::from_bytes(&array)
        .map_err(|e| KernelError::bad_request("key_invalid", format!("public key: {e}")))
}

#[cfg(unix)]
fn write_private_file(path: &Path, bytes: &[u8]) -> KernelResult<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, bytes: &[u8]) -> KernelResult<()> {
    fs::write(path, bytes)?;
    Ok(())
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_sign_verify_round_trip() {
        let pair = KeyPair::generate(1_788_609_600_000);
        assert!(pair.key_id.starts_with("key_"));
        let sig = pair.sign(b"hello");
        assert!(pair.public().verify(b"hello", &sig));
        assert!(!pair.public().verify(b"hullo", &sig));
        assert!(!pair.public().verify(b"hello", "not-a-signature"));
    }

    #[test]
    fn files_round_trip_in_every_format() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pair = KeyPair::generate(1);
        let private = dir.path().join("publisher.key");
        let public = dir.path().join("publisher.pub");
        pair.write_private(&private).expect("write");
        pair.write_public(&public).expect("write");
        let meta = fs::metadata(&private).expect("meta");
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let loaded = KeyPair::load(&private).expect("load");
        assert_eq!(loaded.key_id, pair.key_id);
        assert_eq!(loaded.signing.to_bytes(), pair.signing.to_bytes());
        let loaded_pub = PublicKey::load(&public).expect("load");
        assert_eq!(loaded_pub.key_id, pair.key_id);
        assert!(loaded_pub.verify(b"x", &pair.sign(b"x")));

        // Raw hex with the key id from the file name.
        let raw = dir.path().join("key_raw.pub");
        let hex: String = pair
            .signing
            .verifying_key()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        fs::write(&raw, hex).expect("write");
        let loaded_raw = PublicKey::load(&raw).expect("load");
        assert_eq!(loaded_raw.key_id, "key_raw");
        assert!(loaded_raw.verify(b"x", &pair.sign(b"x")));

        // PEM.
        use ed25519_dalek::pkcs8::{EncodePrivateKey, EncodePublicKey};
        let pem_private = dir.path().join("pem.key");
        let pem = pair
            .signing
            .to_pkcs8_pem(ed25519_dalek::pkcs8::spki::der::pem::LineEnding::LF)
            .expect("pem");
        fs::write(&pem_private, pem.as_bytes()).expect("write");
        let loaded_pem = KeyPair::load(&pem_private).expect("load pem");
        assert_eq!(loaded_pem.signing.to_bytes(), pair.signing.to_bytes());
        let pem_public = dir.path().join("pem.pub");
        let pem = pair
            .signing
            .verifying_key()
            .to_public_key_pem(ed25519_dalek::pkcs8::spki::der::pem::LineEnding::LF)
            .expect("pem");
        fs::write(&pem_public, pem).expect("write");
        assert!(PublicKey::load(&pem_public)
            .expect("load")
            .verify(b"x", &pair.sign(b"x")));

        let all = PublicKey::load_dir(dir.path());
        assert_eq!(all.len(), 3);
        assert!(PublicKey::load_dir(&dir.path().join("missing")).is_empty());
    }
}
