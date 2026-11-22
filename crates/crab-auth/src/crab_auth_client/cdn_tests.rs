use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::*;

const APPROVAL: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn public_exports_require_a_distinct_native_origin_in_the_same_cloud() {
    for (source, export) in [
        ("s3://models", "s3://models-export"),
        ("gs://models", "gs://models-export"),
        (
            "https://crabfixtures.blob.core.windows.net/models",
            "https://crabexport.blob.core.windows.net/models",
        ),
    ] {
        let deployment =
            CdnViewerDeployment::new(source, "org/models", "https://edge.example.com").unwrap();
        deployment.validate_public_export(export).unwrap();
        assert!(deployment.validate_public_export(source).is_err());
        assert!(
            deployment
                .validate_public_export(&format!("{export}/"))
                .is_err()
        );
        let foreign = if source.starts_with("s3://") {
            "gs://models-export"
        } else {
            "s3://models-export"
        };
        assert!(deployment.validate_public_export(foreign).is_err());
    }
    let azure = CdnViewerDeployment::new(
        "https://crabfixtures.blob.core.windows.net/models",
        "org/models",
        "https://edge.example.com",
    )
    .unwrap();
    assert!(
        azure
            .validate_public_export("https://crabfixtures.blob.core.windows.net/models-export")
            .is_err()
    );
}

fn provider(cache: Arc<TokenCache>) -> CrabAuthProvider {
    CrabAuthProvider {
        endpoint: "https://auth.example.com/v1/credentials".into(),
        issuer_url: "https://idp.example.com".into(),
        client_id: "fixture-client".into(),
        client_version: "fixture-version".into(),
        token_cache: cache,
        cached: RwLock::new(None),
        http: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        cdn: Vec::new(),
        allow_viewer_grants: true,
        cdn_principal: OnceLock::new(),
        retained_view: OnceLock::new(),
    }
}

fn deployment() -> CdnViewerDeployment {
    CdnViewerDeployment::new("gs://fixture", "org/repo", "https://downloads.example.com")
        .unwrap()
        .with_approval(Some(APPROVAL))
        .unwrap()
}

fn id_token(subject: &str, issued: u32) -> String {
    let body = URL_SAFE_NO_PAD.encode(format!(
        r#"{{"iss":"https://idp.example.com","sub":"{subject}","iat":{issued}}}"#
    ));
    format!("header.{body}.fixture-signature")
}

async fn credential_endpoint(
    permissions: &[&str],
    viewer_grants: bool,
    scope: Option<&StorageScope>,
    viewer: Option<(Option<&str>, &str)>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/credentials", listener.local_addr().unwrap());
    let expiry = (time::OffsetDateTime::now_utc() + time::Duration::hours(1))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let mut body = serde_json::json!({
        "provider": "gcp",
        "credentials": {"access_token": "fixture-native-token"},
        "permissions": permissions,
        "expires_at": expiry,
        "storage_scope": scope,
    });
    let approval_header = if let Some((approval, base_url)) = viewer {
        body["cdn"] = serde_json::json!({
            "provider": "cloud_cdn",
            "origin": "gs://fixture",
            "base_url": base_url,
            "repo_prefix": "org/repo",
            "expires_at": time::OffsetDateTime::now_utc().unix_timestamp() + 300,
            "grants": [{"prefix": "org/repo/v2/pack-layers/", "credential": "Cloud-CDN-Cookie=URLPrefix=fixture"}],
        });
        approval
            .map(|value| format!("{CDN_APPROVAL_HEADER}: {value}\r\n"))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let body = body.to_string();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut head = String::new();
        while !head.ends_with("\r\n\r\n") {
            assert_ne!(stream.read_line(&mut head).await.unwrap(), 0);
        }
        assert_eq!(
            head.to_ascii_lowercase()
                .contains("x-crab-cdn-grant-version: 1\r\n"),
            viewer_grants,
        );
        assert_eq!(
            head.to_ascii_lowercase()
                .contains(&format!("{CDN_APPROVAL_HEADER}: {APPROVAL}\r\n")),
            viewer_grants,
        );
        let header = format!(
            "HTTP/1.1 200 OK\r\n{approval_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(header.as_bytes()).await.unwrap();
        stream.write_all(body.as_bytes()).await.unwrap();
    });
    (url, server)
}

#[tokio::test]
async fn issuer_retention_uses_only_the_last_acknowledged_revision() {
    for (operation, wire_operation) in [
        (ViewReadOperation::Mount, "mount"),
        (ViewReadOperation::Fetch, "fetch"),
    ] {
        check_retention_revision(operation, wire_operation).await;
    }
}

async fn check_retention_revision(operation: ViewReadOperation, wire_operation: &'static str) {
    let dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(TokenCache::test_cache(dir.path().to_path_buf()));
    let token = id_token("alice", 1);
    cache.store(CRAB_AUTH_CACHE_KEY, &token, None).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut reader = provider(cache);
    reader.endpoint = format!(
        "http://{}/issuer/v1/credentials/",
        listener.local_addr().unwrap()
    );
    reader.load_bound_login().unwrap();
    let reader = Arc::new(reader);
    let retention = ViewRetention::new(
        reader.clone(),
        "crab://fixture/org/repo".into(),
        "org/repo/acl-views/v2/scope/view".into(),
        "ab".repeat(32),
        operation,
    )
    .unwrap();
    let server = tokio::spawn(async move {
        let mut nonce = None;
        let incarnation = "01234567-89ab-7000-8000-0123456789ab";
        let revision =
            |sequence| serde_json::json!({"incarnation": incarnation, "sequence": sequence});
        let view_prefix = "org/repo/acl-views/v2/scope/view";
        let expiry = (time::OffsetDateTime::now_utc() + time::Duration::hours(1))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        for (expected_action, expected_sequence, reply) in [
            (
                "acquire",
                None,
                serde_json::json!({
                    "action": "acquired", "repo_prefix": view_prefix, "view_state_digest": "d".repeat(64),
                    "revision": revision(0), "remaining_ms": 60_000,
                }),
            ),
            (
                "credentials",
                None,
                serde_json::json!({
                    "provider": "gcp", "credentials": {"access_token": "fixture-native-token"},
                    "permissions": ["read"], "expires_at": expiry,
                    "storage_scope": {"repo_prefix": view_prefix, "global_prefix": format!("{view_prefix}/.crab"),
                                      "source_repo": "org/repo", "scope_hash": "a".repeat(64)},
                }),
            ),
            (
                "renew",
                Some(0),
                serde_json::json!({
                    "action": "renewed", "revision": revision(1), "remaining_ms": 60_000,
                }),
            ),
            (
                "renew",
                Some(1),
                serde_json::json!({
                    "action": "renewed", "revision": revision(3), "remaining_ms": 60_000,
                }),
            ),
            (
                "release",
                Some(1),
                serde_json::json!({"action": "released"}),
            ),
        ] {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(socket);
            let mut header = String::new();
            while !header.ends_with("\r\n\r\n") {
                assert_ne!(stream.read_line(&mut header).await.unwrap(), 0);
            }
            let credentials = expected_action == "credentials";
            let path = if credentials {
                "/issuer/v1/credentials/"
            } else {
                "/issuer/v1/views/retention"
            };
            assert!(header.starts_with(&format!("POST {path} HTTP/1.1\r\n")));
            assert!(header.contains(&format!("authorization: Bearer {token}\r\n")));
            let size = header
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .unwrap()
                .parse()
                .unwrap();
            let mut body = vec![0; size];
            stream.read_exact(&mut body).await.unwrap();
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let binding = if credentials {
                &request["retained_view"]
            } else {
                &request
            };
            let current_nonce = binding["reader_nonce"].as_str().unwrap();
            assert_eq!(current_nonce.len(), 64);
            assert!(current_nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert_eq!(
                nonce
                    .get_or_insert_with(|| current_nonce.to_owned())
                    .as_str(),
                current_nonce
            );
            assert_eq!(request["repo_url"], "crab://fixture/org/repo");
            assert_eq!(binding["view_prefix"], view_prefix);
            assert_eq!(binding["target"], "ab".repeat(32));
            assert_eq!(request["id_token"], token);
            if credentials {
                assert_eq!(request["operation"], wire_operation);
                assert_eq!(binding["incarnation"], incarnation);
            } else {
                assert_eq!(request["operation"]["action"], expected_action);
                assert_eq!(request["read_operation"], wire_operation);
                assert_eq!(
                    request["operation"]["revision"]["sequence"].as_u64(),
                    expected_sequence
                );
            }
            let reply = reply.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let start = tokio::time::Instant::now();
    let (digest, mut lease) = retention.acquire().await.unwrap();
    assert_eq!(digest, "d".repeat(64));
    let (renewal, deadline) = lease.schedule();
    assert!(deadline <= tokio::time::Instant::now() + Duration::from_secs(60));
    assert!(deadline >= start + Duration::from_secs(60));
    assert_eq!(deadline - renewal, Duration::from_secs(20));
    let wrong = if wire_operation == "fetch" {
        "mount"
    } else {
        "fetch"
    };
    assert!(reader.resolve("fixture", "org/repo", wrong).await.is_err());
    let refreshed = reader
        .refresh_for("fixture", "org/repo", wire_operation)
        .await
        .unwrap();
    assert_eq!(
        refreshed.storage_scope.unwrap().repo_prefix,
        lease.view_prefix()
    );
    lease.renew().await.unwrap();
    assert!(matches!(
        lease.renew().await,
        Err(AuthError::InvalidCredentialResponse(_))
    ));
    lease.release().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn cached_and_refreshed_reads_cannot_change_principal() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(TokenCache::test_cache(dir.path().to_path_buf()));
    let original = id_token("alice", 1);
    cache
        .store(CRAB_AUTH_CACHE_KEY, &original, Some("fixture-refresh"))
        .unwrap();
    let (endpoint, server) = credential_endpoint(&["read"], true, None, None).await;
    let mut reader = provider(Arc::clone(&cache)).with_cdn(deployment()).unwrap();
    reader.endpoint = endpoint;
    reader
        .resolve("fixture", "org/repo", "fetch")
        .await
        .unwrap();
    server.await.unwrap();
    let principal = reader.cdn_principal().unwrap();
    let child = provider(Arc::clone(&cache))
        .with_cdn_checkout(deployment(), principal)
        .unwrap();

    // Same subject, new token. The reader can reuse its unexpired authorization.
    let rotated = id_token("alice", 2);
    cache.store(CRAB_AUTH_CACHE_KEY, &rotated, None).unwrap();
    reader
        .resolve("fixture", "org/repo", "fetch")
        .await
        .unwrap();
    assert!(reader.refresh_bound_id_token(&original).await.unwrap() == rotated);
    assert!(child.load_bound_login().is_ok());

    cache
        .store(CRAB_AUTH_CACHE_KEY, &id_token("bob", 3), None)
        .unwrap();
    for result in [
        reader.resolve("fixture", "org/repo", "fetch").await,
        reader.refresh_for("fixture", "org/repo", "fetch").await,
        child.resolve("fixture", "org/repo", "hydrate").await,
    ] {
        assert!(matches!(result, Err(AuthError::CredentialsExpired(_))));
    }
    cache.delete(CRAB_AUTH_CACHE_KEY).unwrap();
    assert!(matches!(
        reader.refresh_bound_id_token(&rotated).await,
        Err(AuthError::NoCredentials)
    ));
}

#[tokio::test]
async fn negotiated_origin_only_credentials_must_remain_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(TokenCache::test_cache(dir.path().to_path_buf()));
    cache
        .store(CRAB_AUTH_CACHE_KEY, &id_token("alice", 1), None)
        .unwrap();
    let (endpoint, server) = credential_endpoint(&["read", "write"], true, None, None).await;
    let mut reader = provider(cache).with_cdn(deployment()).unwrap();
    reader.endpoint = endpoint;
    let error = reader
        .resolve("fixture", "org/repo", "fetch")
        .await
        .unwrap_err();
    assert!(matches!(error, AuthError::InvalidCredentialResponse(_)));
    server.await.unwrap();
}

#[tokio::test]
async fn release_origin_read_keeps_the_bound_issuer_scope_and_principal() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(TokenCache::test_cache(dir.path().to_path_buf()));
    cache
        .store(CRAB_AUTH_CACHE_KEY, &id_token("alice", 1), None)
        .unwrap();
    let scope_hash = "a".repeat(64);
    let repo_prefix = format!("org/repo/acl-views/v1/{scope_hash}/1-fixture");
    let scope = StorageScope {
        global_prefix: format!("{repo_prefix}/.crab"),
        repo_prefix,
        source_repo: "org/repo".into(),
        scope_hash,
    };
    let (endpoint, server) = credential_endpoint(&["read"], false, Some(&scope), None).await;
    let mut reader = provider(Arc::clone(&cache))
        .with_cdn(deployment())
        .unwrap()
        .with_viewer_grants(false);
    reader.endpoint = endpoint;
    let result = reader
        .resolve("fixture", "org/repo", "fetch")
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(result.storage_scope.as_ref(), Some(&scope));
    assert!(result.cdn.is_none());

    let child = provider(Arc::clone(&cache))
        .with_cdn_checkout(deployment(), reader.cdn_principal().unwrap())
        .unwrap()
        .with_viewer_grants(false);
    cache
        .store(CRAB_AUTH_CACHE_KEY, &id_token("bob", 2), None)
        .unwrap();
    assert!(matches!(
        child.resolve("fixture", "org/repo", "hydrate").await,
        Err(AuthError::CredentialsExpired(_))
    ));
}

#[tokio::test]
async fn private_viewer_grant_requires_the_exact_approval_and_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(TokenCache::test_cache(dir.path().to_path_buf()));
    cache
        .store(CRAB_AUTH_CACHE_KEY, &id_token("alice", 1), None)
        .unwrap();
    let foreign = "b".repeat(64);
    for (approval, url, valid) in [
        (None, "https://downloads.example.com", false),
        (
            Some(foreign.as_str()),
            "https://downloads.example.com",
            false,
        ),
        (Some(APPROVAL), "https://other.example.com", false),
        (Some(APPROVAL), "https://downloads.example.com", true),
    ] {
        let (endpoint, server) =
            credential_endpoint(&["read"], true, None, Some((approval, url))).await;
        let mut reader = provider(Arc::clone(&cache)).with_cdn(deployment()).unwrap();
        reader.endpoint = endpoint;
        let result = reader.resolve("fixture", "org/repo", "fetch").await;
        server.await.unwrap();
        match result {
            Ok(result) => assert!(valid && result.cdn.is_some()),
            Err(error) => {
                assert!(!valid && matches!(error, AuthError::InvalidCredentialResponse(_)))
            }
        }
    }
}

#[tokio::test]
async fn private_binding_without_approval_retains_native_authority() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(TokenCache::test_cache(dir.path().to_path_buf()));
    cache
        .store(CRAB_AUTH_CACHE_KEY, &id_token("alice", 1), None)
        .unwrap();
    let (endpoint, server) = credential_endpoint(&["read"], false, None, None).await;
    let mut reader = provider(cache)
        .with_cdn(deployment().with_approval(None).unwrap())
        .unwrap();
    reader.endpoint = endpoint;
    let resolution = reader
        .resolve("fixture", "org/repo", "fetch")
        .await
        .unwrap();
    assert!(resolution.cdn.is_none());
    assert!(reader.cdn_principal().is_some());
    server.await.unwrap();
}

#[test]
fn private_cdn_requires_https_for_the_credential_and_oidc_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(TokenCache::test_cache(dir.path().to_path_buf()));
    let mut reader = provider(Arc::clone(&cache));
    reader.endpoint = "http://auth.example.com/v1/credentials".into();
    assert!(reader.with_cdn(deployment()).is_err());
    let mut reader = provider(cache);
    reader.issuer_url = "http://idp.example.com".into();
    assert!(reader.with_cdn(deployment()).is_err());
}
