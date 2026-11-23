use std::{path::Path, time::SystemTime};

use crab_auth::{TransferOperation, token_cache::expand_token_cache_path};
use crab_auth_store::ManagedRepositoryResolver;
use crab_metadata::capsule_protocol::{
    CheckpointPointer, PackSourceKind, RepositoryRoot, load_layered_checkpoint_control,
};
use crab_storage::{StorageError, Store, StoreLayout, store::CDN_PROBE_MAX_BYTES};
use object_store::path::Path as ObjectPath;
use tokio_util::sync::CancellationToken;

use super::CheckResult;
use crate::{
    auth::build_repository_store,
    core::{config::Config, error::CrabError, project_config::ProjectConfig},
    replication::{
        ReadSource, ReadStoreSelection,
        discovery::load_if_unconfigured,
        fetch::{read_store_for_batch_with_selector, select_fetch_replica},
    },
};

pub(super) async fn check(root: &Path, cancel: &CancellationToken) -> Vec<CheckResult> {
    let startup = || {
        let url = ProjectConfig::remote_url(root)?;
        let config = Config::resolve_for_repo(root)?;
        let resolver =
            ManagedRepositoryResolver::new(expand_token_cache_path(&config.auth.token_cache_path));
        let locator = resolver.classify(&url)?;
        Ok::<_, CrabError>((url, config, locator))
    };
    let (url, mut config, locator) = match startup() {
        Ok(selection) => selection,
        Err(error) => return vec![failed("CDN binding", error)],
    };
    let timeout = config.operation_timeout;
    let read_url =
        matches!(&locator, crab_git::RepositoryLocator::Direct(_)).then_some(url.as_str());
    let selecting = async {
        let resolved = build_repository_store(
            &config,
            locator,
            TransferOperation::Fetch,
            crab_storage::cdn::CdnConsumer::Cli,
            cancel,
        )
        .await?;
        if let Some(url) = read_url {
            load_if_unconfigured(
                &resolved.store,
                &resolved.repository_prefix,
                url,
                &mut config,
            )
            .await?;
        }
        read_store_for_batch_with_selector(
            &resolved.store,
            &resolved.repository_prefix,
            Some(&resolved.capsule_root),
            read_url,
            &config,
            cancel,
            select_fetch_replica,
        )
        .await
    };
    let resolved = tokio::select! {
        biased;
        () = cancel.cancelled() => return vec![failed("remote access", CrabError::Cancelled)],
        selected = tokio::time::timeout(timeout, selecting) => match selected {
            Ok(Ok(resolved)) => resolved,
            Ok(Err(error)) => return vec![failed("remote access", error)],
            Err(_) => return vec![CheckResult::fail("remote access", "fetch store selection timed out")],
        },
    };
    probe(resolved, timeout, cancel).await
}

async fn probe(
    selected: ReadStoreSelection,
    timeout: std::time::Duration,
    cancel: &CancellationToken,
) -> Vec<CheckResult> {
    let origin = selected.store.into_storage();
    let root = selected.capsule_root.record().root();
    let source = match selected.source {
        ReadSource::Primary => "primary".to_owned(),
        ReadSource::Replica { name } => format!("read replica {name:?}"),
    };
    let mut checks = vec![CheckResult::ok(
        "remote access",
        format!(
            "origin capsule v2 root generation {} is reachable on {source}",
            root.generation()
        ),
    )];
    let Some(binding) = origin.cdn_binding() else {
        checks.push(CheckResult::warn(
            "CDN binding",
            "selected fetch uses authorized origin",
        ));
        return checks;
    };
    let mode = if binding.private {
        "private"
    } else {
        "public export"
    };
    let target = origin
        .native_cdn_origin()
        .unwrap_or("native provider target");
    checks.push(CheckResult::ok(
        "CDN binding",
        format!(
            "{mode} on {} for {target}/{}",
            binding.authority, binding.repository_prefix
        ),
    ));

    let layout = StoreLayout::with_global_prefix(
        origin.clone(),
        selected.router.repo_prefix().to_owned(),
        selected.router.global_prefix().to_owned(),
    );
    let sample = async {
        if let Some((path, digest, size)) = immutable_sample(&layout, root) {
            return Ok(Some((path, digest.to_owned(), size)));
        }
        match root.checkpoint() {
            Some(pointer) => checkpoint_pack_sample(&layout, pointer).await,
            None => Ok(None),
        }
    };
    let sample = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(CrabError::Cancelled),
        result = tokio::time::timeout(timeout, sample) => match result {
            Ok(result) => result,
            Err(_) => {
                checks.push(CheckResult::fail("CDN origin", "native checkpoint control GET timed out"));
                return checks;
            }
        },
    };
    let sample = match sample {
        Ok(sample) => sample,
        Err(error) => {
            checks.push(failed("CDN origin", error));
            return checks;
        }
    };
    let Some((path, digest, size)) = sample else {
        checks.push(CheckResult::warn(
            "CDN eligibility",
            "the current root has no immutable Git object within both the compiled limits and the 1 MiB probe limit",
        ));
        return checks;
    };
    let Some(attempt) = origin.cdn_attempt(&path) else {
        checks.push(CheckResult::warn(
            "CDN eligibility",
            "the selected route excludes the sample",
        ));
        return checks;
    };
    checks.push(CheckResult::ok(
        "CDN eligibility",
        format!("root-authenticated {size} B Git object"),
    ));

    let verified = async {
        let hash = blake3::Hash::from_hex(&digest).map_err(|_| StorageError::CorruptObject {
            path: "CDN diagnostic origin object".into(),
            reason: "invalid authenticated digest".into(),
        })?;
        let bytes = origin.verify_bounded(&path, hash.as_bytes(), size).await?;
        if bytes.len() as u64 == size {
            Ok(bytes)
        } else {
            Err(StorageError::CorruptObject {
                path: "CDN diagnostic origin object".into(),
                reason: "length differs from the authenticated root".into(),
            })
        }
    };
    let bytes = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(StorageError::Cancelled),
        result = tokio::time::timeout(timeout, verified) => match result {
            Ok(result) => result,
            Err(_) => {
                checks.push(CheckResult::fail("CDN origin", "native immutable GET timed out"));
                return checks;
            }
        },
    };
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(error) => {
            checks.push(failed("CDN origin", error.into()));
            return checks;
        }
    };
    checks.push(CheckResult::ok(
        "CDN origin",
        "native immutable body verified",
    ));

    let result = match attempt.probe(&bytes, cancel).await {
        Ok(result) => result,
        Err(error) => {
            checks.push(failed("CDN probe", error.into()));
            return checks;
        }
    };
    let expiry = match result.viewer_expires_at {
        Some(expiry) => match expiry.duration_since(SystemTime::now()) {
            Ok(remaining) => {
                CheckResult::ok("CDN viewer", format!("valid for {} s", remaining.as_secs()))
            }
            Err(_) => CheckResult::warn("CDN viewer", "credential expired during the probe"),
        },
        None if binding.private => CheckResult::warn("CDN viewer", "credential expiry unavailable"),
        None => CheckResult::ok("CDN viewer", "public export uses anonymous viewer reads"),
    };
    checks.push(expiry);
    checks.push(read_check("CDN GET", result.whole_object));
    checks.push(read_check("CDN range", result.range));
    checks.push(match result.mutable_status {
        Some(Ok(status @ (401 | 403))) => {
            CheckResult::ok("CDN mutable route", format!("HTTP {status} denial"))
        }
        Some(Ok(status)) => CheckResult::fail(
            "CDN mutable route",
            format!("expected HTTP 401 or 403, received {status}"),
        ),
        Some(Err(error)) => failed("CDN mutable route", error.into()),
        None => CheckResult::warn(
            "CDN mutable route",
            "requires verified GET and range delivery",
        ),
    });
    checks
}

fn immutable_sample<'a>(
    layout: &StoreLayout<Store>,
    root: &'a RepositoryRoot,
) -> Option<(ObjectPath, &'a str, u64)> {
    let checkpoint = root.checkpoint().map(|pointer| {
        (
            layout.capsule_checkpoint_path(pointer.hash()),
            pointer.hash(),
            pointer.size(),
        )
    });
    let history = root.history().map(|pointer| {
        (
            layout.capsule_history_segment_path(pointer.hash()),
            pointer.hash(),
            pointer.size(),
        )
    });
    root.capsule_frontier()
        .iter()
        .map(|pointer| {
            (
                layout.capsule_path(pointer.hash()),
                pointer.hash(),
                pointer.size(),
            )
        })
        .chain(checkpoint)
        .chain(history)
        .filter_map(|sample| {
            probe_rank(layout.store(), &sample.0, sample.2).map(|rank| (rank, sample))
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, sample)| sample)
}

async fn checkpoint_pack_sample(
    layout: &StoreLayout<Store>,
    pointer: &CheckpointPointer,
) -> Result<Option<(ObjectPath, String, u64)>, CrabError> {
    if pointer.control_size() > CDN_PROBE_MAX_BYTES {
        return Ok(None);
    }
    // Resolve the candidate from a native control read so the probe can prove
    // byte equality independently of the edge it is testing.
    let origin = StoreLayout::new(
        layout.store().clone().without_cdn_route(),
        layout.repo_prefix().to_owned(),
    );
    let checkpoint = load_layered_checkpoint_control(&origin, pointer).await?;
    Ok(checkpoint
        .sources()
        .iter()
        .filter(|source| source.kind() == PackSourceKind::PackLayer)
        .filter_map(|source| {
            let path = layout.capsule_pack_layer_path(source.object_hash());
            probe_rank(layout.store(), &path, source.object_size()).map(|rank| (rank, path, source))
        })
        .min_by_key(|(rank, _, _)| *rank)
        .map(|(_, path, source)| (path, source.object_hash().to_owned(), source.object_size())))
}

fn probe_rank(store: &Store, path: &ObjectPath, size: u64) -> Option<(bool, u64)> {
    if !(2..=CDN_PROBE_MAX_BYTES).contains(&size) {
        return None;
    }
    let attempt = store.cdn_attempt(path)?;
    let whole = attempt.admits(size, None);
    let range = attempt.admits(size, Some(1));
    // A sample that admits both forms can also verify mutable-route denial.
    (whole || range).then_some((!(whole && range), size))
}

fn failed(name: &'static str, error: CrabError) -> CheckResult {
    // Provider and issuer errors can carry URLs and response data. The stable
    // error code preserves the diagnostic category without emitting either.
    CheckResult::fail(name, format!("failed ({})", error.code()))
}

fn read_check(name: &'static str, result: Option<crab_storage::Result<()>>) -> CheckResult {
    match result {
        Some(Ok(())) => CheckResult::ok(name, "matches verified native bytes"),
        Some(Err(error)) => failed(name, error.into()),
        None => CheckResult::warn(name, "skipped by the compiled CDN read limits"),
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use bytes::Bytes;
    use crab_metadata::capsule_protocol::{
        CapsuleGitPack, CapsulePointer, LayeredCheckpoint, PackLayer, PointerCatalog, RootRecord,
        load_root,
    };
    use crab_storage::cdn::{
        CdnAccess, CdnCredential, CdnCredentials, CdnReadFamily, CdnRoute, CdnTransport,
    };
    use object_store::memory::InMemory;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };

    use super::*;
    use crate::cmd::doctor::CheckStatus;

    struct Viewer(SystemTime);

    #[async_trait::async_trait]
    impl CdnCredentials for Viewer {
        async fn credential(
            &self,
            key: &ObjectPath,
            rejected: Option<u64>,
        ) -> crab_storage::Result<Option<CdnCredential>> {
            assert!(key.as_ref().starts_with("replica/org/repo/v2/capsules/"));
            assert_eq!(rejected, None);
            Ok(Some(
                CdnCredential::bearer(0, "fixture-token".into()).with_expiration(self.0),
            ))
        }
    }

    async fn populated_store(
        route: CdnRoute,
        actual_origin_bytes: Bytes,
    ) -> (ReadStoreSelection, String) {
        let store = Store::new(Arc::new(InMemory::new()))
            .with_target_identity([7; 32])
            .with_cdn_route(route)
            .unwrap();
        let prefix = "replica/org/repo";
        let layout = StoreLayout::new(store.clone(), prefix.into());
        let initial = RootRecord::encode(
            RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        let digest = blake3::hash(b"good").to_hex().to_string();
        let transaction = "2".repeat(64);
        let pointer = CapsulePointer::new(
            &digest,
            4,
            2,
            2,
            "3".repeat(64),
            0,
            vec![transaction.clone()],
            initial.digest(),
        )
        .unwrap();
        let next = initial
            .root()
            .advance(
                initial.digest(),
                Default::default(),
                Default::default(),
                vec![pointer],
                &transaction,
            )
            .unwrap();
        let path = layout.capsule_path(&digest);
        store.put(&path, actual_origin_bytes).await.unwrap();
        store
            .put(
                &layout.capsule_root_path(),
                RootRecord::encode(next).unwrap().bytes().clone(),
            )
            .await
            .unwrap();
        let capsule_root = load_root(&layout).await.unwrap();
        let store = crate::storage::Store::from_storage(store);
        (
            ReadStoreSelection {
                view_retention: None,
                router: crate::storage::StoreLayout::new(store.clone(), prefix.into()),
                store,
                source: ReadSource::Replica {
                    name: "west".into(),
                },
                capsule_root,
            },
            path.to_string(),
        )
    }

    #[tokio::test]
    async fn selects_a_pack_layer_from_an_authenticated_native_checkpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let store = Store::new(Arc::new(InMemory::new()))
            .with_target_identity([7; 32])
            .with_cdn_route(CdnRoute::new(
                [7; 32],
                "org/repo".into(),
                None,
                CdnAccess::Public,
                CdnTransport::loopback_fixture(listener.local_addr().unwrap()).unwrap(),
                Duration::from_secs(1),
            ))
            .unwrap();
        let layout = StoreLayout::new(store, "org/repo".into());
        let pack = CapsuleGitPack::new(
            Bytes::from_static(b"pack"),
            Bytes::from_static(b"index"),
            Bytes::from_static(b"reverse"),
            Bytes::from_static(b"locator"),
            "3".repeat(40),
            1,
        )
        .unwrap();
        let layer = PackLayer::build(&pack).unwrap();
        let checkpoint = LayeredCheckpoint::build(
            1,
            &"a".repeat(64),
            vec![layer.source_descriptor().unwrap()],
            PointerCatalog::new(),
            None,
        )
        .unwrap();
        let pointer = CheckpointPointer::new_layered(
            checkpoint.hash(),
            checkpoint.bytes().len() as u64,
            checkpoint.control_offset(),
            checkpoint.control_size(),
            checkpoint.footer_hash(),
            checkpoint.covered_generation(),
            checkpoint.covered_root_digest(),
            checkpoint.pack_count().unwrap(),
            checkpoint.object_count().unwrap(),
        )
        .unwrap();
        layout
            .store()
            .put(
                &layout.capsule_checkpoint_path(checkpoint.hash()),
                checkpoint.bytes().clone(),
            )
            .await
            .unwrap();

        assert_eq!(
            checkpoint_pack_sample(&layout, &pointer).await.unwrap(),
            Some((
                layout.capsule_pack_layer_path(layer.hash()),
                layer.hash().to_owned(),
                layer.bytes().len() as u64,
            ))
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn private_probe_verifies_bytes_reports_expiry_and_requires_mutable_denial() {
        for (body, status, verdict) in [
            ("good", Some("403 Forbidden"), CheckStatus::Ok),
            ("good", Some("206 Partial Content"), CheckStatus::Fail),
            ("evil", None, CheckStatus::Warn),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let route = CdnRoute::new(
                [7; 32],
                "replica/org/repo".into(),
                None,
                CdnAccess::Private(Arc::new(Viewer(
                    SystemTime::now() + Duration::from_secs(60),
                ))),
                CdnTransport::loopback_fixture(listener.local_addr().unwrap()).unwrap(),
                Duration::from_secs(10),
            );
            let (resolved, object_path) = populated_store(route, Bytes::from_static(b"good")).await;
            let responder = tokio::spawn(async move {
                let responses = [
                    format!("HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 4\r\n\r\n{body}"),
                    "HTTP/1.1 206 Partial Content\r\nConnection: close\r\nContent-Range: bytes 0-0/4\r\nContent-Length: 1\r\n\r\ng".to_owned(),
                ].into_iter().chain(status.map(|status| {
                    format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
                }));
                for (index, response) in responses.enumerate() {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut stream = BufReader::new(stream);
                    let mut request = String::new();
                    while !request.ends_with("\r\n\r\n") {
                        assert_ne!(stream.read_line(&mut request).await.unwrap(), 0);
                    }
                    let expected_path = if index < 2 {
                        object_path.as_str()
                    } else {
                        "replica/org/repo/v2/root"
                    };
                    assert!(request.starts_with(&format!("GET /{expected_path} HTTP/1.1\r\n")));
                    assert!(request.contains("authorization: Bearer fixture-token\r\n"));
                    assert_eq!(request.contains("range: bytes=0-0\r\n"), index > 0);
                    if index == 2 {
                        assert!(request.contains("cache-control: no-store\r\n"));
                    }
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
                if status.is_none() {
                    assert!(
                        tokio::time::timeout(Duration::from_millis(30), listener.accept())
                            .await
                            .is_err()
                    );
                }
            });
            let checks = probe(resolved, Duration::from_secs(10), &CancellationToken::new()).await;
            responder.await.unwrap();
            assert_eq!(checks.len(), 8);
            assert!(checks[0].detail.contains("read replica \"west\""));
            assert!(
                checks
                    .iter()
                    .all(|check| !check.detail.contains("fixture-token"))
            );
            assert!(
                checks[..5]
                    .iter()
                    .all(|check| check.status == CheckStatus::Ok)
            );
            assert!(checks[4].detail.contains("valid for"));
            assert_eq!(checks[5].name, "CDN GET");
            assert_eq!(
                checks[5].status,
                if body == "good" {
                    CheckStatus::Ok
                } else {
                    CheckStatus::Fail
                }
            );
            assert_eq!(checks[6].status, CheckStatus::Ok);
            assert_eq!(checks[7].name, "CDN mutable route");
            assert_eq!(checks[7].status, verdict);
        }
    }

    #[tokio::test]
    async fn failed_origin_proof_stops_before_edge_io() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let route = CdnRoute::new(
            [7; 32],
            "replica/org/repo".into(),
            None,
            CdnAccess::Public,
            CdnTransport::loopback_fixture(listener.local_addr().unwrap()).unwrap(),
            Duration::from_secs(10),
        );
        let (resolved, _) = populated_store(route, Bytes::from_static(b"evil")).await;
        let checks = probe(resolved, Duration::from_secs(10), &CancellationToken::new()).await;
        let failure = checks.last().unwrap();
        assert_eq!(failure.name, "CDN origin");
        assert_eq!(failure.status, CheckStatus::Fail);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn probe_sends_only_admitted_immutable_reads() {
        for (limits, ranged) in [
            ("0 4 1 3000", Some(true)),
            ("4 0 0 3000", Some(false)),
            ("3 3 1 3000", None),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let route = CdnRoute::new(
                [7; 32],
                "replica/org/repo".into(),
                None,
                CdnAccess::Public,
                CdnTransport::loopback_fixture(listener.local_addr().unwrap()).unwrap(),
                Duration::from_secs(3),
            )
            .with_loopback_read_limits(CdnReadFamily::Capsule, limits)
            .unwrap();
            let (resolved, object_path) = populated_store(route, Bytes::from_static(b"good")).await;
            let responder = tokio::spawn(async move {
                if let Some(ranged) = ranged {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut stream = BufReader::new(stream);
                    let mut request = String::new();
                    while !request.ends_with("\r\n\r\n") {
                        assert_ne!(stream.read_line(&mut request).await.unwrap(), 0);
                    }
                    assert!(request.starts_with(&format!("GET /{object_path} HTTP/1.1\r\n")));
                    assert_eq!(request.contains("range: bytes=0-0\r\n"), ranged);
                    let response = if ranged {
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/4\r\nContent-Length: 1\r\nConnection: close\r\n\r\ng"
                    } else {
                        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ngood"
                    };
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), listener.accept())
                        .await
                        .is_err()
                );
            });
            let checks = probe(resolved, Duration::from_secs(3), &CancellationToken::new()).await;
            tokio::time::timeout(Duration::from_secs(2), responder)
                .await
                .unwrap()
                .unwrap();
            if let Some(ranged) = ranged {
                let (pass, skipped) = if ranged { (6, 5) } else { (5, 6) };
                assert_eq!(checks[pass].status, CheckStatus::Ok);
                assert_eq!(checks[skipped].status, CheckStatus::Warn);
                assert!(checks[skipped].detail.contains("compiled CDN read limits"));
                assert_eq!(checks[7].name, "CDN mutable route");
                assert_eq!(checks[7].status, CheckStatus::Warn);
            } else {
                let eligibility = checks.last().unwrap();
                assert_eq!(eligibility.name, "CDN eligibility");
                assert_eq!(eligibility.status, CheckStatus::Warn);
                assert!(eligibility.detail.contains("compiled limits"));
            }
        }
    }
}
