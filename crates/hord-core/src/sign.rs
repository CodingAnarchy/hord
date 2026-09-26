//! Ed25519 signatures over hord objects (spec §10.5.4).
//!
//! A key is named by its [key id](PublicKey::key_id), `ed25519:` followed by
//! the 64 hex digits of the public key, so a [`Signature`] names the key
//! that verifies it. Whose key it is (which [`crate::Actor`]) is recorded
//! by the server that registered it, not by the key.
//!
//! What is signed is a domain-separated message: a fixed prefix, the
//! domain, a zero byte, then the message. Objects are signed by the
//! [`ObjectId`] of their canonical encoding with the signature unset:
//!
//! - [`sign_change`] / [`verify_change`]: `ChangeRecord.signature`, domain
//!   [`CHANGE_DOMAIN`];
//! - [`sign_evidence`] / [`verify_evidence`]: `Evidence.signature`, domain
//!   [`EVIDENCE_DOMAIN`];
//! - [`SigningKey::sign`] / [`PublicKey::verify`]: any other message under a
//!   caller's own domain (the arbitration seam: an `Arbitrated` event or a
//!   resolution signs its encoding under its own domain).

use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use ed25519_dalek::{Signer, Verifier};
use hord_encoding::ObjectId;
use thiserror::Error;

use crate::{Bytes, ChangeRecord, Evidence, Signature};

/// Prefix of every signed message, so a hord signature is never valid for
/// another protocol's message.
const PREFIX: &[u8] = b"hord-sig-v1\0";

/// Domain of `ChangeRecord.signature`.
pub const CHANGE_DOMAIN: &str = "hord.change-record";

/// Domain of `Evidence.signature`.
pub const EVIDENCE_DOMAIN: &str = "hord.evidence";

/// Key id prefix: the algorithm.
const KEY_ID_PREFIX: &str = "ed25519:";

/// Failure to sign, verify, or read a key.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SignError {
    /// The object carries no signature.
    #[error("not signed")]
    Unsigned,
    /// The signature was made by a different key than the one given.
    #[error("signed by key {signed}, not {expected}")]
    WrongKey {
        /// The signature's key id.
        signed: String,
        /// The key it was checked against.
        expected: String,
    },
    /// The signature does not verify.
    #[error("signature does not verify with key {0}")]
    BadSignature(String),
    /// A key id or key file is malformed.
    #[error("invalid key: {0}")]
    InvalidKey(String),
    /// The object could not be encoded.
    #[error(transparent)]
    Encoding(#[from] hord_encoding::Error),
    /// The system random source failed.
    #[error("random source: {0}")]
    Random(String),
}

/// A private Ed25519 key.
pub struct SigningKey(ed25519_dalek::SigningKey);

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SigningKey")
            .field(&self.public().key_id())
            .finish()
    }
}

impl SigningKey {
    /// A new key from the system's random source.
    pub fn generate() -> Result<Self, SignError> {
        let mut seed = [0u8; ed25519_dalek::SECRET_KEY_LENGTH];
        getrandom::fill(&mut seed).map_err(|err| SignError::Random(err.to_string()))?;
        Ok(Self(ed25519_dalek::SigningKey::from_bytes(&seed)))
    }

    /// Read a PKCS#8 PEM private key (`-----BEGIN PRIVATE KEY-----`).
    pub fn from_pem(pem: &str) -> Result<Self, SignError> {
        ed25519_dalek::SigningKey::from_pkcs8_pem(pem)
            .map(Self)
            .map_err(|err| SignError::InvalidKey(err.to_string()))
    }

    /// The key as PKCS#8 PEM.
    pub fn to_pem(&self) -> Result<String, SignError> {
        self.0
            .to_pkcs8_pem(LineEnding::LF)
            .map(|pem| pem.to_string())
            .map_err(|err| SignError::InvalidKey(err.to_string()))
    }

    /// Its public half.
    #[must_use]
    pub fn public(&self) -> PublicKey {
        PublicKey(self.0.verifying_key())
    }

    /// Sign `message` under `domain`.
    #[must_use]
    pub fn sign(&self, domain: &str, message: &[u8]) -> Signature {
        let signature = self.0.sign(&framed(domain, message));
        Signature {
            key_id: self.public().key_id(),
            bytes: Bytes::from(signature.to_bytes().to_vec()),
        }
    }
}

/// A public Ed25519 key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublicKey(ed25519_dalek::VerifyingKey);

impl PublicKey {
    /// Its key id: `ed25519:` and the key in hex.
    #[must_use]
    pub fn key_id(&self) -> String {
        format!("{KEY_ID_PREFIX}{}", hex::encode(self.0.as_bytes()))
    }

    /// The key a key id names.
    pub fn from_key_id(key_id: &str) -> Result<Self, SignError> {
        let bad = || SignError::InvalidKey(format!("{key_id:?} is not an ed25519:<hex> key id"));
        let hex = key_id.strip_prefix(KEY_ID_PREFIX).ok_or_else(bad)?;
        let bytes: [u8; ed25519_dalek::PUBLIC_KEY_LENGTH] = hex::decode(hex)
            .map_err(|_| bad())?
            .try_into()
            .map_err(|_| bad())?;
        ed25519_dalek::VerifyingKey::from_bytes(&bytes)
            .map(Self)
            .map_err(|err| SignError::InvalidKey(err.to_string()))
    }

    /// Check that `signature` is this key's, over `message` under `domain`.
    pub fn verify(
        &self,
        domain: &str,
        message: &[u8],
        signature: &Signature,
    ) -> Result<(), SignError> {
        let expected = self.key_id();
        if signature.key_id != expected {
            return Err(SignError::WrongKey {
                signed: signature.key_id.clone(),
                expected,
            });
        }
        let bad = || SignError::BadSignature(expected.clone());
        let bytes: [u8; ed25519_dalek::SIGNATURE_LENGTH] =
            signature.bytes.as_slice().try_into().map_err(|_| bad())?;
        let signature = ed25519_dalek::Signature::from_bytes(&bytes);
        self.0
            .verify(&framed(domain, message), &signature)
            .map_err(|_| bad())
    }
}

impl std::fmt::Display for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key_id())
    }
}

impl std::str::FromStr for PublicKey {
    type Err = SignError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_key_id(s)
    }
}

fn framed(domain: &str, message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(PREFIX.len() + domain.len() + 1 + message.len());
    out.extend_from_slice(PREFIX);
    out.extend_from_slice(domain.as_bytes());
    out.push(0);
    out.extend_from_slice(message);
    out
}

/// What a change's signature covers: the id of the record without it.
fn change_message(record: &ChangeRecord) -> Result<ObjectId, SignError> {
    let unsigned = ChangeRecord {
        signature: None,
        ..record.clone()
    };
    Ok(ObjectId::of(&unsigned)?)
}

/// What evidence's signature covers: the id of the evidence without it.
fn evidence_message(evidence: &Evidence) -> Result<ObjectId, SignError> {
    let unsigned = Evidence {
        signature: None,
        ..evidence.clone()
    };
    Ok(ObjectId::of(&unsigned)?)
}

/// Set `record.signature` to `key`'s signature over the rest of it.
pub fn sign_change(record: &mut ChangeRecord, key: &SigningKey) -> Result<(), SignError> {
    let message = change_message(record)?;
    record.signature = Some(key.sign(CHANGE_DOMAIN, message.as_bytes()));
    Ok(())
}

/// Check `record.signature` against `key`.
pub fn verify_change(record: &ChangeRecord, key: &PublicKey) -> Result<(), SignError> {
    let signature = record.signature.as_ref().ok_or(SignError::Unsigned)?;
    key.verify(CHANGE_DOMAIN, change_message(record)?.as_bytes(), signature)
}

/// Set `evidence.signature` to `key`'s signature over the rest of it.
pub fn sign_evidence(evidence: &mut Evidence, key: &SigningKey) -> Result<(), SignError> {
    let message = evidence_message(evidence)?;
    evidence.signature = Some(key.sign(EVIDENCE_DOMAIN, message.as_bytes()));
    Ok(())
}

/// Check `evidence.signature` against `key`.
pub fn verify_evidence(evidence: &Evidence, key: &PublicKey) -> Result<(), SignError> {
    let signature = evidence.signature.as_ref().ok_or(SignError::Unsigned)?;
    key.verify(
        EVIDENCE_DOMAIN,
        evidence_message(evidence)?.as_bytes(),
        signature,
    )
}

/// The key a signature names, checked to verify it: for an object whose
/// signer is not known in advance. Proves only that the named key signed.
pub fn signer(signature: &Signature) -> Result<PublicKey, SignError> {
    PublicKey::from_key_id(&signature.key_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Actor, EvidenceKind, EvidenceResult, Timestamp};

    fn review() -> Evidence {
        Evidence {
            kind: EvidenceKind::Review,
            qualifier: Some("human".into()),
            snapshot: ObjectId::from_canonical(b"snapshot"),
            toolchain: ObjectId::from_canonical(b"toolchain"),
            command: "hord review --as human --approve".into(),
            scope: None,
            result: EvidenceResult::Pass,
            log: None,
            cost_ms: 0,
            produced_by: Actor::Human { id: "ada".into() },
            produced_at: Timestamp::from_millis(1),
            signature: None,
        }
    }

    #[test]
    fn evidence_verifies_with_its_key_only() -> Result<(), SignError> {
        let key = SigningKey::generate()?;
        let other = SigningKey::generate()?;
        let mut evidence = review();
        assert!(matches!(
            verify_evidence(&evidence, &key.public()),
            Err(SignError::Unsigned)
        ));
        sign_evidence(&mut evidence, &key)?;
        verify_evidence(&evidence, &key.public())?;
        assert!(matches!(
            verify_evidence(&evidence, &other.public()),
            Err(SignError::WrongKey { .. })
        ));
        // A signature re-labelled with the other key's id does not verify.
        let mut forged = evidence.clone();
        if let Some(sig) = &mut forged.signature {
            sig.key_id = other.public().key_id();
        }
        assert!(matches!(
            verify_evidence(&forged, &other.public()),
            Err(SignError::BadSignature(_))
        ));
        // Any change to the signed content breaks it.
        let mut tampered = evidence.clone();
        tampered.result = EvidenceResult::Fail {
            summary: "no".into(),
        };
        assert!(matches!(
            verify_evidence(&tampered, &key.public()),
            Err(SignError::BadSignature(_))
        ));
        Ok(())
    }

    #[test]
    fn domains_separate_signatures() -> Result<(), SignError> {
        let key = SigningKey::generate()?;
        let sig = key.sign("a", b"message");
        key.public().verify("a", b"message", &sig)?;
        assert!(key.public().verify("b", b"message", &sig).is_err());
        assert!(key.public().verify("a", b"other", &sig).is_err());
        Ok(())
    }

    #[test]
    fn keys_round_trip_through_pem_and_key_ids() -> Result<(), SignError> {
        let key = SigningKey::generate()?;
        let pem = key.to_pem()?;
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"), "{pem}");
        let back = SigningKey::from_pem(&pem)?;
        assert_eq!(back.public(), key.public());
        let id = key.public().key_id();
        assert_eq!(id.len(), KEY_ID_PREFIX.len() + 64);
        assert_eq!(id.parse::<PublicKey>()?, key.public());
        assert!("ed25519:zz".parse::<PublicKey>().is_err());
        assert!("rsa:00".parse::<PublicKey>().is_err());
        Ok(())
    }
}
