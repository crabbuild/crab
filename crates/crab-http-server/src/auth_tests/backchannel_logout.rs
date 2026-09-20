use super::*;
use jsonwebtoken::{Algorithm, EncodingKey, Header};

fn claims(h: &Harness) -> Value {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    json!({"iss":h.provider.issuer,"sub":"alice-id","aud":"crab-browser","iat":now,"exp":now+300,"jti":uuid::Uuid::now_v7().to_string(),"events":{"http://schemas.openid.net/event/backchannel-logout":{}}})
}

fn signed(h: &Harness, claims: &Value) -> String {
    let rotated = h.provider.rotated.load(Ordering::SeqCst);
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some(if rotated { "two" } else { "one" }.into());
    header.typ = Some("logout+jwt".into());
    jsonwebtoken::encode(
        &header,
        claims,
        &EncodingKey::from_ed_pem(if rotated { KEY_TWO } else { KEY_ONE }.as_bytes()).unwrap(),
    )
    .unwrap()
}

async fn post(h: &Harness, token: &str) -> reqwest::Response {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("logout_token", token)
        .append_pair("provider_extension", "ignored")
        .finish();
    h.http
        .post(format!("{}/auth/backchannel-logout", h.origin))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn pending_delivery_resumes_after_restart_and_already_logged_out_succeeds() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let claims = claims(&h);
    let token = signed(&h, &claims);
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab logout replay v1\0");
    hasher.update(h.provider.issuer.as_bytes());
    hasher.update(b"\0");
    hasher.update(claims["jti"].as_str().unwrap().as_bytes());
    let root = h.server.catalog.as_ref().unwrap().root();
    let path = root.path(&format!(
        ".crab/http-server/v1/logout-replays/{}",
        hasher.finalize().to_hex()
    ));
    root.store.create_strict(&path,bytes::Bytes::from(json!({"token_digest":blake3::hash(token.as_bytes()).to_hex().to_string(),"expires_at":claims["exp"],"completed":false}).to_string())).await.unwrap();
    assert_eq!(post(&h, &token).await.status(), StatusCode::OK);
    assert_eq!(
        h.json("/api/session", &cookie).await["authenticated"],
        false
    );
    let (body, _) = root.store.get_with_etag_bounded(&path, 1024).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["completed"],
        true
    );
    assert_eq!(
        post(&h, &signed(&h, &super::backchannel_logout::claims(&h)))
            .await
            .status(),
        StatusCode::OK
    );
    h.close().await;
}

#[tokio::test]
async fn logout_revokes_all_sessions_and_completed_replay_preserves_new_login() {
    let h = Harness::new(false).await;
    let first = h.login().await;
    let second = h.login().await;
    let session = h.json("/api/session", &first).await;
    let issued = h
        .http
        .post(format!("{}/api/git-token", h.origin))
        .header(header::COOKIE, &first)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header(header::CONTENT_TYPE, "application/json")
        .body(json!({"owner":"team","repository":"private","access":"read"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(issued.status(), StatusCode::OK);
    let issued: Value = serde_json::from_slice(&issued.bytes().await.unwrap()).unwrap();
    let replica = Authentication::new_durable(
        crate::OidcConfig {
            provider: crate::AuthProvider::Oidc,
            issuer: openidconnect::IssuerUrl::new(h.provider.issuer.clone()).unwrap(),
            public_url: Url::parse(&h.origin).unwrap(),
            client_id: "crab-browser".into(),
            client_secret_file: None,
            state_key_file: None,
            github: None,
        },
        h.server.catalog.as_ref().unwrap().root(),
    )
    .await
    .unwrap();
    h.provider.rotated.store(true, Ordering::SeqCst);
    let token = signed(&h, &claims(&h));
    assert_eq!(post(&h, &token).await.status(), StatusCode::OK);
    for cookie in [&first, &second] {
        assert_eq!(h.json("/api/session", cookie).await["authenticated"], false);
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::COOKIE, cookie.parse().unwrap());
        assert!(!replica.principal(&headers).await.authenticated());
    }
    assert_eq!(
        h.http
            .get(format!(
                "{}/git/team/private.git/info/refs?service=git-upload-pack",
                h.origin
            ))
            .basic_auth("crab", issued["token"].as_str())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let fresh = h.login().await;
    assert_eq!(post(&h, &token).await.status(), StatusCode::OK);
    assert_eq!(h.json("/api/session", &fresh).await["authenticated"], true);
    h.close().await;
}

#[tokio::test]
async fn invalid_claims_do_not_revoke_and_jti_cannot_be_reused() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    for (field, value) in [
        ("iss", json!("https://other.invalid")),
        ("aud", json!("other-client")),
        ("exp", json!(1)),
        ("iat", json!(u64::MAX)),
        ("jti", json!("")),
        ("sub", json!("")),
        ("events", json!({})),
        (
            "events",
            json!({"http://schemas.openid.net/event/backchannel-logout":true}),
        ),
        ("nonce", Value::Null),
    ] {
        let mut invalid = claims(&h);
        invalid[field] = value;
        assert_eq!(
            post(&h, &signed(&h, &invalid)).await.status(),
            StatusCode::BAD_REQUEST,
            "{field}"
        );
        assert_eq!(h.json("/api/session", &cookie).await["authenticated"], true);
    }
    for field in ["jti", "sub", "iat", "exp", "aud"] {
        let mut invalid = claims(&h);
        invalid.as_object_mut().unwrap().remove(field);
        assert_eq!(
            post(&h, &signed(&h, &invalid)).await.status(),
            StatusCode::BAD_REQUEST,
            "missing {field}"
        );
    }
    let mut original = claims(&h);
    assert_eq!(
        post(&h, &signed(&h, &original)).await.status(),
        StatusCode::OK
    );
    let fresh = h.login().await;
    original["iat"] = json!(original["iat"].as_u64().unwrap() - 1);
    assert_eq!(
        post(&h, &signed(&h, &original)).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(h.json("/api/session", &fresh).await["authenticated"], true);
    h.close().await;
}

#[tokio::test]
async fn invalid_headers_signatures_and_transport_have_no_side_effect() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    for case in [
        "signature",
        "unknown-key",
        "missing-key",
        "typ",
        "embedded-key",
        "algorithm",
        "critical",
        "ambiguous-logout-key",
        "wrong-logout-key-operation",
    ] {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some("one".into());
        header.typ = Some("logout+jwt".into());
        match case {
            "unknown-key" => header.kid = Some("missing".into()),
            "missing-key" => header.kid = None,
            "typ" => header.typ = Some("JWT".into()),
            "embedded-key" => {
                header.jwk = Some(
                    serde_json::from_value(
                        serde_json::to_value(h.provider.signing_key().as_verification_key())
                            .unwrap(),
                    )
                    .unwrap(),
                )
            }
            "algorithm" => header.alg = Algorithm::HS256,
            "critical" => header.crit = Some(vec!["unknown".into()]),
            "ambiguous-logout-key" | "wrong-logout-key-operation" => {
                *h.provider.mode.lock().await = case.into()
            }
            _ => {}
        }
        let key = if case == "algorithm" {
            EncodingKey::from_secret(b"not-a-provider-secret")
        } else {
            EncodingKey::from_ed_pem(
                if case == "signature" {
                    KEY_TWO
                } else {
                    KEY_ONE
                }
                .as_bytes(),
            )
            .unwrap()
        };
        let token = jsonwebtoken::encode(&header, &claims(&h), &key).unwrap();
        assert_eq!(
            post(&h, &token).await.status(),
            StatusCode::BAD_REQUEST,
            "{case}"
        );
        assert_eq!(h.json("/api/session", &cookie).await["authenticated"], true);
    }
    *h.provider.mode.lock().await = String::new();
    for body in [
        "".to_owned(),
        "logout_token=one&logout_token=two".into(),
        "logout_token=".into(),
        "x".repeat(65537),
    ] {
        assert_eq!(
            h.http
                .post(format!("{}/auth/backchannel-logout", h.origin))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(body)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(h.json("/api/session", &cookie).await["authenticated"], true);
    h.close().await;
}
