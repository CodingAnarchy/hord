//! Encoding and decoding errors.

use thiserror::Error;

/// Failure to encode, decode, or parse an [`ObjectId`](crate::ObjectId).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// Canonical CBOR encoding failed.
    #[error("canonical CBOR encode failed: {0}")]
    Encode(String),
    /// CBOR decoding failed, or the input was not exactly one well-formed item.
    #[error("CBOR decode failed: {0}")]
    Decode(String),
    /// A hex string was not a valid 32-byte [`ObjectId`](crate::ObjectId).
    #[error("invalid ObjectId hex: {0}")]
    ObjectIdHex(String),
    /// A byte slice used as an [`ObjectId`](crate::ObjectId) was not 32 bytes.
    #[error("ObjectId must be {expected} bytes, got {actual}")]
    ObjectIdLength {
        /// Expected length in bytes (always 32).
        expected: usize,
        /// Actual length of the provided slice.
        actual: usize,
    },
}
