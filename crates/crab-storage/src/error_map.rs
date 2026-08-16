//! Maps `object_store::Error` variants to storage-domain errors.

use crate::error::StorageError;

/// Classifies an `object_store::Error` into an auth-specific storage error.
///
/// Returns `None` for errors that are not auth-related. Callers that want
/// auth failures to bypass generic retry handling should call this before
/// falling through to [`map_object_store_error`].
#[must_use]
pub fn classify_auth_error(err: &object_store::Error) -> Option<StorageError> {
    match err {
        object_store::Error::PermissionDenied { path, .. } => {
            Some(StorageError::AuthFailed { path: path.clone() })
        }

        object_store::Error::Unauthenticated { .. } => Some(StorageError::NoCredentials),

        object_store::Error::Generic { source, .. } => {
            let lower = source.to_string().to_lowercase();
            if lower.contains("expired") && lower.contains("token") {
                Some(StorageError::AuthExpired {
                    path: String::new(),
                })
            } else {
                None
            }
        }

        _ => None,
    }
}

/// Classifies an `object_store::Error` into a storage-domain error.
///
/// `path` is supplied by the caller because some object-store variants do not
/// carry one, but callers normally know which key they were operating on.
/// Variants that already embed a path use the source path.
#[must_use]
pub fn map_object_store_error(err: object_store::Error, path: &str) -> StorageError {
    match err {
        object_store::Error::Precondition { path: p, .. }
        | object_store::Error::AlreadyExists { path: p, .. } => {
            StorageError::StateConflict { path: p }
        }

        object_store::Error::NotFound { path: p, .. } => StorageError::NotFound { path: p },

        object_store::Error::PermissionDenied { path: p, .. } => {
            StorageError::Forbidden { path: p }
        }

        object_store::Error::Unauthenticated { .. } => StorageError::NoCredentials,

        err @ object_store::Error::Generic { .. } => {
            let msg = err.to_string().to_lowercase();
            if is_throttling_message(&msg) {
                StorageError::Throttled { retry_after: None }
            } else {
                StorageError::NetworkTransient { source: err }
            }
        }

        err @ object_store::Error::NotSupported { .. } => {
            StorageError::NotSupported { source: err }
        }

        err => {
            let _ = path;
            StorageError::ObjectStore { source: err }
        }
    }
}

fn is_throttling_message(message: &str) -> bool {
    message.contains("throttl")
        || message.contains("slowdown")
        || message.contains("slow down")
        || message.contains(" 429")
        || message.contains("too many requests")
        || message.contains(" 503")
        || message.contains("service unavailable")
}

#[cfg(test)]
#[expect(clippy::panic, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    fn boxed(msg: &'static str) -> Box<dyn std::error::Error + Send + Sync + 'static> {
        Box::<dyn std::error::Error + Send + Sync>::from(msg)
    }

    #[test]
    fn precondition_maps_to_state_conflict() {
        let err = object_store::Error::Precondition {
            path: "repo/refs/heads/main".into(),
            source: boxed("etag mismatch"),
        };
        match map_object_store_error(err, "ignored") {
            StorageError::StateConflict { path } => {
                assert_eq!(path, "repo/refs/heads/main");
            }
            other => panic!("expected StateConflict, got {other:?}"),
        }
    }

    #[test]
    fn already_exists_maps_to_state_conflict() {
        let err = object_store::Error::AlreadyExists {
            path: "repo/objects/abc".into(),
            source: boxed("exists"),
        };
        assert!(matches!(
            map_object_store_error(err, "ignored"),
            StorageError::StateConflict { .. }
        ));
    }

    #[test]
    fn not_found_maps_to_not_found() {
        let err = object_store::Error::NotFound {
            path: "repo/objects/missing".into(),
            source: boxed("404"),
        };
        match map_object_store_error(err, "ignored") {
            StorageError::NotFound { path } => assert_eq!(path, "repo/objects/missing"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn permission_denied_maps_to_forbidden() {
        let err = object_store::Error::PermissionDenied {
            path: "repo/locks/main".into(),
            source: boxed("403"),
        };
        match map_object_store_error(err, "ignored") {
            StorageError::Forbidden { path } => assert_eq!(path, "repo/locks/main"),
            other => panic!("expected Forbidden, got {other:?}"),
        }
    }

    #[test]
    fn unauthenticated_maps_to_no_credentials() {
        let err = object_store::Error::Unauthenticated {
            path: "repo/refs/main".into(),
            source: boxed("401"),
        };
        assert!(matches!(
            map_object_store_error(err, "ignored"),
            StorageError::NoCredentials
        ));
    }

    #[test]
    fn generic_maps_to_network_transient() {
        let err = object_store::Error::Generic {
            store: "S3",
            source: boxed("connection reset"),
        };
        assert!(matches!(
            map_object_store_error(err, "repo/objects/x"),
            StorageError::NetworkTransient { .. }
        ));
    }

    #[test]
    fn throttled_generic_maps_to_throttled() {
        let err = object_store::Error::Generic {
            store: "S3",
            source: boxed("service unavailable: slow down"),
        };
        assert!(matches!(
            map_object_store_error(err, "repo/objects/x"),
            StorageError::Throttled { retry_after: None }
        ));
    }

    #[test]
    fn not_supported_maps_to_not_supported() {
        let err = object_store::Error::NotSupported {
            source: boxed("conditional copy"),
        };
        assert!(matches!(
            map_object_store_error(err, "ignored"),
            StorageError::NotSupported { .. }
        ));
    }

    #[test]
    fn not_implemented_falls_through_to_object_store() {
        let err = object_store::Error::NotImplemented {
            operation: "copy_opts".to_owned(),
            implementer: "FixtureStore".to_owned(),
        };
        assert!(matches!(
            map_object_store_error(err, "ignored"),
            StorageError::ObjectStore { .. }
        ));
    }

    #[test]
    fn source_is_preserved_through_network_transient() {
        let err = object_store::Error::Generic {
            store: "S3",
            source: boxed("timeout"),
        };
        let mapped = map_object_store_error(err, "repo/x");
        let src = std::error::Error::source(&mapped).expect("NetworkTransient carries source");
        assert!(src.to_string().contains("S3") || src.to_string().contains("timeout"));
    }

    #[test]
    fn classify_permission_denied_as_auth_failed() {
        let err = object_store::Error::PermissionDenied {
            path: "repo/packs/abc".into(),
            source: boxed("403"),
        };
        match classify_auth_error(&err) {
            Some(StorageError::AuthFailed { path }) => {
                assert_eq!(path, "repo/packs/abc");
            }
            other => panic!("expected AuthFailed, got {other:?}"),
        }
    }

    #[test]
    fn classify_unauthenticated_as_no_credentials() {
        let err = object_store::Error::Unauthenticated {
            path: "repo/refs/main".into(),
            source: boxed("401"),
        };
        assert!(matches!(
            classify_auth_error(&err),
            Some(StorageError::NoCredentials)
        ));
    }

    #[test]
    fn classify_expired_token_as_auth_expired() {
        let err = object_store::Error::Generic {
            store: "S3",
            source: boxed("The security token included in the request is expired"),
        };
        assert!(matches!(
            classify_auth_error(&err),
            Some(StorageError::AuthExpired { .. })
        ));
    }

    #[test]
    fn classify_non_auth_generic_returns_none() {
        let err = object_store::Error::Generic {
            store: "S3",
            source: boxed("connection reset"),
        };
        assert!(classify_auth_error(&err).is_none());
    }

    #[test]
    fn classify_not_found_returns_none() {
        let err = object_store::Error::NotFound {
            path: "repo/objects/missing".into(),
            source: boxed("404"),
        };
        assert!(classify_auth_error(&err).is_none());
    }
}
