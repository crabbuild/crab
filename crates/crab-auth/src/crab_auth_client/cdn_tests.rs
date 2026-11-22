use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::*;

const APPROVAL: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn public_exports_require_a_distinct_native_origin_in_the_same_cloud() {
    for (source, export) in [
        ("s3://models", "s3://models-export"),
        ("gs://models", "gs://models-export"),
        (
            "https://crabfixtures.blob.core.windows.net/models",
            "https://crabfixtures.blob.core.windows.net/models-export",
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
    assert!(reader.refresh_cdn_id_token(&original).await.unwrap() == rotated);
    assert!(child.load_cdn_login().is_ok());

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
        reader.refresh_cdn_id_token(&rotated).await,
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
