use std::error::Error;
use std::fmt;

use crab_read::ReadError;
use crab_storage::StorageError;
use crab_types::error::ErrorCategory;

use super::CrabError;
use crate::storage::retry::{RetryClass, retry_class};

/// Source-preserving diagnostics for failures returned through shared reconstruction.
#[derive(Debug)]
pub struct ReadFailure(pub(super) ReadError);

enum Cause<'a> {
    Product(&'a CrabError),
    OriginIntegrity {
        path: &'a str,
        source: &'a crab_cache::CacheError,
    },
    Io(&'a std::io::Error),
    Network(&'a object_store::Error),
    Store {
        display: &'a dyn fmt::Display,
        retry: RetryClass,
    },
    Other,
}

impl ReadFailure {
    fn with_cause<T>(&self, diagnostic: impl FnOnce(Cause<'_>) -> T) -> T {
        let mut current: Option<&(dyn Error + 'static)> = Some(&self.0);
        while let Some(error) = current {
            if let Some(error) = error.downcast_ref::<CrabError>() {
                return diagnostic(Cause::Product(error));
            }
            if let Some(crab_cache_store::CacheStoreError::OriginIntegrity { path, source }) =
                error.downcast_ref::<crab_cache_store::CacheStoreError>()
            {
                return diagnostic(Cause::OriginIntegrity { path, source });
            }
            if let Some(error) = error.downcast_ref::<StorageError>() {
                // Reuse product policy without taking ownership of Xet's shared
                // source. Only diagnostic fields are copied; opaque SDK errors
                // stay borrowed and the original chain remains in self.0.
                let product = match error {
                    StorageError::NetworkTransient { source } => {
                        return diagnostic(Cause::Network(source));
                    }
                    StorageError::NotSupported { source }
                    | StorageError::ObjectStore { source } => {
                        return diagnostic(Cause::Store {
                            display: source,
                            retry: crate::storage::retry::classify_storage(source),
                        });
                    }
                    StorageError::MultipartJournal { source, .. } => {
                        // Direct conversion wraps this in object_store::Generic.
                        // Borrow its display without replacing the opaque source
                        // or bypassing that wrapper for a nested journal I/O error.
                        let display = format_args!("Generic multipart journal error: {source}");
                        return diagnostic(Cause::Store {
                            display: &display,
                            retry: RetryClass::Transient,
                        });
                    }
                    StorageError::Io { source } => return diagnostic(Cause::Io(source)),
                    StorageError::Throttled { retry_after } => CrabError::Throttled {
                        retry_after: *retry_after,
                    },
                    StorageError::StateConflict { path } => CrabError::CasConflict {
                        path: path.clone(),
                        expected_etag: None,
                    },
                    StorageError::NotFound { path } => CrabError::NotFound { path: path.clone() },
                    StorageError::InvalidHash { hash } => {
                        CrabError::Internal(format!("invalid storage object hash: {hash}"))
                    }
                    StorageError::CorruptObject { path, reason } => CrabError::CorruptObject {
                        path: path.clone(),
                        reason: reason.clone(),
                    },
                    StorageError::UnsupportedProvider { provider } => CrabError::Configuration {
                        key: format!(
                            "unsupported storage provider for object-store construction: {provider:?}"
                        ),
                        origin: "storage provider".into(),
                    },
                    StorageError::InvalidStaticEnvTarget { target, reason } => {
                        CrabError::Configuration {
                            key: target.clone(),
                            origin: reason.clone(),
                        }
                    }
                    StorageError::StaticEnvProviderMismatch {
                        expected,
                        actual,
                        bucket,
                    } => CrabError::Configuration {
                        key: format!("static-env provider for {bucket}"),
                        origin: format!("expected {expected:?}, got {actual:?}"),
                    },
                    StorageError::ProviderConfig {
                        provider,
                        bucket,
                        source,
                    } => CrabError::Configuration {
                        key: format!("failed to build {provider:?} object store: {source}"),
                        origin: bucket.clone(),
                    },
                    StorageError::InvalidObjectStoreUrl { url, source } => {
                        CrabError::Configuration {
                            key: format!("invalid object-store URL {url:?}: {source}"),
                            origin: "object-store URL".into(),
                        }
                    }
                    StorageError::UrlStoreConfig { url, source } => CrabError::Configuration {
                        key: format!("failed to build object store from URL {url:?}: {source}"),
                        origin: "object-store URL".into(),
                    },
                    StorageError::Forbidden { path } => CrabError::Forbidden { path: path.clone() },
                    StorageError::NoCredentials => CrabError::NoCredentials,
                    StorageError::AuthFailed { path } => {
                        CrabError::AuthFailed { path: path.clone() }
                    }
                    StorageError::AuthExpired { path } => {
                        CrabError::AuthExpired { path: path.clone() }
                    }
                    StorageError::Cancelled => CrabError::Cancelled,
                    StorageError::Internal(message) => CrabError::Internal(message.clone()),
                };
                return diagnostic(Cause::Product(&product));
            }
            if let Some(error) = error.downcast_ref::<std::io::Error>() {
                return diagnostic(Cause::Io(error));
            }
            current = error.source();
        }
        diagnostic(Cause::Other)
    }

    pub(crate) fn code(&self) -> &'static str {
        self.with_cause(|cause| match cause {
            Cause::Product(error) => error.code(),
            Cause::OriginIntegrity { .. } => "CRAB-E0020",
            Cause::Io(_) => "CRAB-E0070",
            Cause::Network(_) => "CRAB-E0001",
            Cause::Store { .. } => "CRAB-E0071",
            Cause::Other => "CRAB-E0099",
        })
    }

    pub(super) fn exit_code(&self) -> u8 {
        self.with_cause(|cause| match cause {
            Cause::Product(error) => error.exit_code(),
            Cause::OriginIntegrity { .. } => 4,
            Cause::Io(_) | Cause::Store { .. } => 5,
            Cause::Network(_) => 1,
            Cause::Other => 9,
        })
    }

    pub(super) fn category(&self) -> ErrorCategory {
        self.with_cause(|cause| match cause {
            Cause::Product(error) => error.category(),
            Cause::OriginIntegrity { .. } => ErrorCategory::Integrity,
            Cause::Io(_) | Cause::Store { .. } => ErrorCategory::Transport,
            Cause::Network(_) => ErrorCategory::Transient,
            Cause::Other => ErrorCategory::Internal,
        })
    }

    pub(super) fn is_retryable(&self) -> bool {
        self.with_cause(|cause| match cause {
            Cause::Product(error) => error.is_retryable(),
            Cause::Network(_) => true,
            _ => false,
        })
    }

    pub(crate) fn retry_class(&self) -> RetryClass {
        self.with_cause(|cause| match cause {
            Cause::Product(error) => match retry_class(error) {
                RetryClass::InspectErrno => RetryClass::Fatal,
                class => class,
            },
            Cause::Network(_) => RetryClass::Transient,
            Cause::Store { retry, .. } => retry,
            // Origin verification exhausted cache repair. Writer failures can
            // follow partial output; replaying that writer is not safe.
            Cause::OriginIntegrity { .. } | Cause::Io(_) | Cause::Other => RetryClass::Fatal,
        })
    }

    pub(super) fn details_json(&self) -> serde_json::Value {
        self.with_cause(|cause| match cause {
            Cause::Product(error) => error.details_json(),
            Cause::OriginIntegrity { path, source } => serde_json::json!({
                "path": path,
                "reason": source.to_string(),
                "origin": "object-store",
            }),
            Cause::Io(error) => serde_json::json!({ "message": error.to_string() }),
            Cause::Network(source) => {
                serde_json::json!({ "source": source.to_string() })
            }
            Cause::Store { display, .. } => serde_json::json!({ "source": display.to_string() }),
            Cause::Other => serde_json::json!({ "message": self.0.to_string() }),
        })
    }

    pub(super) fn hint(&self) -> Option<&'static str> {
        self.with_cause(|cause| match cause {
            Cause::Product(error) => error.hint(),
            Cause::Store { .. } => Some(super::STORAGE_HINT),
            _ => None,
        })
    }

    pub(super) fn docs_anchor(&self) -> Option<&'static str> {
        self.with_cause(|cause| match cause {
            Cause::Product(error) => error.docs_anchor(),
            Cause::Store { .. } => Some(super::STORAGE_DOCS_ANCHOR),
            _ => None,
        })
    }
}

impl fmt::Display for ReadFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.with_cause(|cause| match cause {
            Cause::Product(error) => error.fmt(f),
            Cause::OriginIntegrity { path, source } => write!(
                f,
                "origin object at {path} failed integrity verification [{}]: {source}",
                self.code()
            ),
            Cause::Io(error) => write!(f, "I/O error [{}]: {error}", self.code()),
            Cause::Network(source) => {
                write!(f, "network transient error [{}]: {source}", self.code())
            }
            Cause::Store { display, .. } => {
                write!(f, "object store error [{}]: {display}", self.code())
            }
            Cause::Other => write!(f, "{} [{}]", self.0, self.code()),
        })
    }
}

impl Error for ReadFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn storage_errors() -> Vec<StorageError> {
        use crab_storage::identity::StorageProviderKind;

        let transport = || object_store::Error::Generic {
            store: "test-origin",
            source: Box::new(std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
        };
        let path = || "repo/xorbs/object".to_owned();
        vec![
            StorageError::NetworkTransient {
                source: transport(),
            },
            StorageError::Throttled {
                retry_after: Some(Duration::from_millis(275)),
            },
            StorageError::Throttled { retry_after: None },
            StorageError::StateConflict { path: path() },
            StorageError::NotFound { path: path() },
            StorageError::InvalidHash {
                hash: "invalid".into(),
            },
            StorageError::CorruptObject {
                path: path(),
                reason: "invalid footer".into(),
            },
            StorageError::NotSupported {
                source: object_store::Error::NotSupported {
                    source: "unsupported operation".into(),
                },
            },
            StorageError::UnsupportedProvider {
                provider: StorageProviderKind::Local,
            },
            StorageError::InvalidStaticEnvTarget {
                target: "invalid".into(),
                reason: "missing bucket".into(),
            },
            StorageError::StaticEnvProviderMismatch {
                expected: StorageProviderKind::S3,
                actual: StorageProviderKind::Gcs,
                bucket: "test-bucket".into(),
            },
            StorageError::ProviderConfig {
                provider: StorageProviderKind::S3,
                bucket: "test-bucket".into(),
                source: transport(),
            },
            StorageError::InvalidObjectStoreUrl {
                url: "invalid".into(),
                source: url::ParseError::RelativeUrlWithoutBase,
            },
            StorageError::UrlStoreConfig {
                url: "s3://test-bucket".into(),
                source: transport(),
            },
            StorageError::AuthFailed { path: path() },
            StorageError::AuthExpired { path: path() },
            StorageError::NoCredentials,
            StorageError::Forbidden { path: path() },
            StorageError::Io {
                source: std::io::Error::from(std::io::ErrorKind::Interrupted),
            },
            StorageError::Io {
                source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            },
            StorageError::Cancelled,
            StorageError::MultipartJournal {
                operation: "claim",
                source: Box::new(std::io::Error::from(std::io::ErrorKind::Interrupted)),
            },
            StorageError::MultipartJournal {
                operation: "record_part",
                source: Box::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            },
            StorageError::MultipartJournal {
                operation: "renew",
                source: "journal lease lost".into(),
            },
            StorageError::ObjectStore {
                source: transport(),
            },
            StorageError::ObjectStore {
                source: object_store::Error::Precondition {
                    path: path(),
                    source: "generation changed".into(),
                },
            },
            StorageError::ObjectStore {
                source: object_store::Error::PermissionDenied {
                    path: path(),
                    source: "denied".into(),
                },
            },
            StorageError::Internal("invariant failed".into()),
        ]
    }

    #[test]
    fn nested_storage_diagnostics_match_direct_product_conversion() {
        for (source, direct) in storage_errors().into_iter().zip(storage_errors()) {
            let direct = CrabError::from(direct);
            let nested = CrabError::Read(ReadFailure(ReadError::CacheStore(
                crab_cache_store::CacheStoreError::Storage(source),
            )));
            let source_before = std::iter::successors(nested.source(), |source| (*source).source())
                .find_map(|source| source.downcast_ref::<StorageError>())
                .unwrap() as *const StorageError;

            assert_eq!(nested.code(), direct.code(), "{direct:?}");
            assert_eq!(nested.exit_code(), direct.exit_code(), "{direct:?}");
            assert_eq!(nested.category(), direct.category(), "{direct:?}");
            assert_eq!(nested.is_retryable(), direct.is_retryable(), "{direct:?}");
            assert_eq!(nested.hint(), direct.hint(), "{direct:?}");
            assert_eq!(nested.docs_anchor(), direct.docs_anchor(), "{direct:?}");
            assert_eq!(nested.details_json(), direct.details_json(), "{direct:?}");
            assert_eq!(nested.to_string(), direct.to_string(), "{direct:?}");
            let retry = match retry_class(&direct) {
                RetryClass::InspectErrno => RetryClass::Fatal,
                class => class,
            };
            assert_eq!(retry_class(&nested), retry, "{direct:?}");
            let source_after = std::iter::successors(nested.source(), |source| (*source).source())
                .find_map(|source| source.downcast_ref::<StorageError>())
                .unwrap() as *const StorageError;
            assert_eq!(source_before, source_after);
        }
    }

    #[tokio::test]
    async fn real_reconstruction_keeps_transport_classification_above_inner_io() {
        use futures_util::TryStreamExt;
        use object_store::ObjectStore;

        let directory = tempfile::tempdir().unwrap();
        let content = bytes::Bytes::from_static(b"verified original payload");
        let (runtime, pointer, counted) =
            crate::read::test_support::stored_file(directory.path(), content, false)
                .await
                .unwrap();
        let objects: Vec<_> = counted.list(None).try_collect().await.unwrap();
        let xorb = objects
            .iter()
            .find(|object| {
                matches!(
                    crab_cache::cache_key_for_path(object.location.as_ref()),
                    Some(crab_cache::CacheKey::Xorb(_))
                )
            })
            .unwrap();
        counted.block_body_reads_for(&xorb.location);
        let error = runtime
            .reconstruct_from_pointer(&pointer.serialize())
            .await
            .unwrap_err();

        assert_eq!(error.code(), "CRAB-E0001", "{error:?}");
        assert_eq!(error.exit_code(), 1);
        assert_eq!(error.category(), ErrorCategory::Transient);
        assert_eq!(retry_class(&error), RetryClass::Transient);
        assert!(
            std::iter::successors(error.source(), |source| (*source).source())
                .any(|source| source.is::<crab_read::ReconstructionError>())
        );
        assert!(
            std::iter::successors(error.source(), |source| (*source).source()).any(
                |source| matches!(
                    source.downcast_ref::<StorageError>(),
                    Some(StorageError::NetworkTransient { .. })
                )
            )
        );
        assert!(
            std::iter::successors(error.source(), |source| (*source).source())
                .any(|source| source.is::<std::io::Error>())
        );
    }
}
