//! [`ApiError`]: the failure of a [`crate::RepoBackend`] call, with the
//! gRPC status code it travels as.

use thiserror::Error;

/// Failure of a [`crate::RepoBackend`] call.
///
/// Each variant is one gRPC status code, so a local and a remote backend
/// fail the same way for the same cause: conversions to and from
/// [`tonic::Status`] keep the variant and the message.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum ApiError {
    /// A named object, change, workspace, or node does not exist
    /// (`NOT_FOUND`).
    #[error("not found: {0}")]
    NotFound(String),
    /// The request is malformed: a bad id, an object whose id is not its
    /// hash, an undecodable object (`INVALID_ARGUMENT`).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// The request is well formed but the repository is not in a state to
    /// accept it (`FAILED_PRECONDITION`).
    #[error("failed precondition: {0}")]
    FailedPrecondition(String),
    /// A batch is over [`crate::MAX_BATCH_IDS`] or
    /// [`crate::MAX_BATCH_BYTES`] (`RESOURCE_EXHAUSTED`).
    #[error("resource exhausted: {0}")]
    ResourceExhausted(String),
    /// The backend does not implement this call yet (`UNIMPLEMENTED`), for
    /// example [`crate::RepoBackend::arbitrate`] before M5.
    #[error("unimplemented: {0}")]
    Unimplemented(String),
    /// The backend is shutting down or unreachable (`UNAVAILABLE`).
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Anything else: store I/O, a failed task (`INTERNAL`).
    #[error("internal: {0}")]
    Internal(String),
}

/// Result alias for [`crate::RepoBackend`] calls.
pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    /// The gRPC status code this error travels as.
    #[must_use]
    pub fn code(&self) -> tonic::Code {
        match self {
            Self::NotFound(_) => tonic::Code::NotFound,
            Self::InvalidArgument(_) => tonic::Code::InvalidArgument,
            Self::FailedPrecondition(_) => tonic::Code::FailedPrecondition,
            Self::ResourceExhausted(_) => tonic::Code::ResourceExhausted,
            Self::Unimplemented(_) => tonic::Code::Unimplemented,
            Self::Unavailable(_) => tonic::Code::Unavailable,
            Self::Internal(_) => tonic::Code::Internal,
        }
    }

    /// The message without the code prefix.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::NotFound(m)
            | Self::InvalidArgument(m)
            | Self::FailedPrecondition(m)
            | Self::ResourceExhausted(m)
            | Self::Unimplemented(m)
            | Self::Unavailable(m)
            | Self::Internal(m) => m,
        }
    }
}

impl From<ApiError> for tonic::Status {
    fn from(err: ApiError) -> Self {
        Self::new(err.code(), err.message())
    }
}

impl From<tonic::Status> for ApiError {
    fn from(status: tonic::Status) -> Self {
        let m = status.message().to_owned();
        match status.code() {
            tonic::Code::NotFound => Self::NotFound(m),
            tonic::Code::InvalidArgument | tonic::Code::OutOfRange => Self::InvalidArgument(m),
            tonic::Code::FailedPrecondition | tonic::Code::AlreadyExists | tonic::Code::Aborted => {
                Self::FailedPrecondition(m)
            }
            tonic::Code::ResourceExhausted => Self::ResourceExhausted(m),
            tonic::Code::Unimplemented => Self::Unimplemented(m),
            tonic::Code::Unavailable | tonic::Code::Cancelled | tonic::Code::DeadlineExceeded => {
                Self::Unavailable(m)
            }
            _ => Self::Internal(m),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_survives_a_status_round_trip() {
        let all = [
            ApiError::NotFound("a".into()),
            ApiError::InvalidArgument("b".into()),
            ApiError::FailedPrecondition("c".into()),
            ApiError::ResourceExhausted("d".into()),
            ApiError::Unimplemented("e".into()),
            ApiError::Unavailable("f".into()),
            ApiError::Internal("g".into()),
        ];
        for err in all {
            let back = ApiError::from(tonic::Status::from(err.clone()));
            assert_eq!(back, err);
        }
    }
}
