//! The git bridge's signature on its divergence checks (ADR 0038, as
//! amended): the bridge signs each `BridgeChecked` with its own key, the
//! `recorder`, so a check recorded where no token is involved (a local
//! endpoint) cannot borrow a bridge's key id.

use hord_core::sign::{self, SignError, SigningKey};
use hord_core::{Bytes, ObjectId, Signature};
use serde::Serialize;

use crate::proto;

/// Domain of a `BridgeChecked` signature.
pub const BRIDGE_CHECK_DOMAIN: &str = "hord.bridge-check";

/// What a bridge signs: every field of the check but the signature.
#[derive(Serialize)]
struct Signed<'a> {
    remote: &'a str,
    diverged: bool,
    expected: Option<&'a str>,
    actual: Option<&'a str>,
    trigger: i32,
    detail: &'a str,
    head: Option<&'a str>,
    recorder: Option<&'a str>,
}

/// The message a bridge signs for `check`: the [`ObjectId`] bytes of its
/// fields' canonical encoding.
fn message(check: &proto::BridgeChecked) -> Result<ObjectId, SignError> {
    Ok(ObjectId::of(&Signed {
        remote: &check.remote,
        diverged: check.diverged,
        expected: check.expected.as_deref(),
        actual: check.actual.as_deref(),
        trigger: check.trigger,
        detail: &check.detail,
        head: check.head.as_deref(),
        recorder: check.recorder.as_deref(),
    })?)
}

/// Name `key` as `check`'s recorder and sign it.
pub fn sign_bridge_check(
    check: &mut proto::BridgeChecked,
    key: &SigningKey,
) -> Result<(), SignError> {
    check.recorder = Some(key.public().key_id());
    check.signature = None;
    let signature = key.sign(BRIDGE_CHECK_DOMAIN, message(check)?.as_bytes());
    check.signature = Some(signature.bytes.to_vec());
    Ok(())
}

/// Check `check`'s signature against the key its `recorder` names.
pub fn verify_bridge_check(check: &proto::BridgeChecked) -> Result<(), SignError> {
    let (Some(recorder), Some(bytes)) = (&check.recorder, &check.signature) else {
        return Err(SignError::Unsigned);
    };
    let signature = Signature {
        key_id: recorder.clone(),
        bytes: Bytes::from(bytes.clone()),
    };
    sign::signer(&signature)?.verify(BRIDGE_CHECK_DOMAIN, message(check)?.as_bytes(), &signature)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_check_verifies_only_as_signed() -> Result<(), SignError> {
        let key = SigningKey::generate()?;
        let mut check = proto::BridgeChecked {
            remote: "https://github.com/o/r.git".into(),
            detail: "in sync".into(),
            ..Default::default()
        };
        assert!(matches!(
            verify_bridge_check(&check),
            Err(SignError::Unsigned)
        ));
        sign_bridge_check(&mut check, &key)?;
        assert_eq!(check.recorder, Some(key.public().key_id()));
        verify_bridge_check(&check)?;
        let diverged = proto::BridgeChecked {
            diverged: true,
            ..check.clone()
        };
        assert!(verify_bridge_check(&diverged).is_err());
        let other = SigningKey::generate()?.public().key_id();
        let borrowed = proto::BridgeChecked {
            recorder: Some(other),
            ..check
        };
        assert!(verify_bridge_check(&borrowed).is_err());
        Ok(())
    }
}
