use axum::{
    Json, Router,
    extract::State,
    http::header,
    response::Redirect,
    routing::{get, post},
};
use openidconnect::{
    AccessToken, JsonWebKeyId, PkceCodeChallenge, PkceCodeVerifier, PrivateSigningKey,
    core::{CoreEdDsaPrivateSigningKey, CoreIdToken, CoreIdTokenClaims, CoreJwsSigningAlgorithm},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;
use url::Url;

// Public deterministic keys used only by the local test identity provider.
pub(crate) const KEY_ONE: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIBERERERERERERERERERERERERERERERERERERERERER\n-----END PRIVATE KEY-----";
pub(crate) const KEY_TWO: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEICIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIi\n-----END PRIVATE KEY-----";

pub(crate) struct Provider {
    pub(crate) issuer: String,
    pub(crate) rotated: AtomicBool,
    pub(crate) confidential: AtomicBool,
    pub(crate) mode: Mutex<String>,
    codes: Mutex<HashMap<String, HashMap<String, String>>>,
}

impl Provider {
    pub(crate) fn signing_key(&self) -> CoreEdDsaPrivateSigningKey {
        let rotated = self.rotated.load(Ordering::SeqCst);
        CoreEdDsaPrivateSigningKey::from_ed25519_pem(
            if rotated { KEY_TWO } else { KEY_ONE },
            Some(JsonWebKeyId::new(
                if rotated { "two" } else { "one" }.into(),
            )),
        )
        .unwrap()
    }
}

async fn metadata(State(provider): State<Arc<Provider>>) -> Json<Value> {
    Json(
        json!({"issuer":provider.issuer,"authorization_endpoint":format!("{}/authorize",provider.issuer),"token_endpoint":format!("{}/token",provider.issuer),"jwks_uri":format!("{}/jwks",provider.issuer),"response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["EdDSA"]}),
    )
}

async fn keys(State(provider): State<Arc<Provider>>) -> Json<Value> {
    if provider.mode.lock().await.as_str() == "wrong-logout-key-operation" {
        let mut key = serde_json::to_value(provider.signing_key().as_verification_key()).unwrap();
        key["key_ops"] = json!(["encrypt"]);
        return Json(json!({"keys":[key]}));
    }
    if provider.mode.lock().await.as_str() == "ambiguous-logout-key" {
        return Json(
            json!({"keys":[provider.signing_key().as_verification_key(),provider.signing_key().as_verification_key()]}),
        );
    }
    Json(json!({"keys":[provider.signing_key().as_verification_key()]}))
}

async fn authorize(
    State(provider): State<Arc<Provider>>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Redirect {
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["client_id"], "crab-browser");
    assert_eq!(params["code_challenge_method"], "S256");
    assert!(params["scope"].split(' ').any(|scope| scope == "openid"));
    let code = openidconnect::CsrfToken::new_random().secret().clone();
    let mut target = Url::parse(&params["redirect_uri"]).unwrap();
    target
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &params["state"]);
    provider.codes.lock().await.insert(code, params);
    Redirect::to(target.as_str())
}

async fn token(
    State(provider): State<Arc<Provider>>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Json<Value> {
    let params: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    let flow = provider.codes.lock().await.remove(&params["code"]).unwrap();
    assert_eq!(params["grant_type"], "authorization_code");
    assert_eq!(params["redirect_uri"], flow["redirect_uri"]);
    if provider.confidential.load(Ordering::SeqCst) {
        assert_eq!(
            headers[header::AUTHORIZATION],
            "Basic Y3JhYi1icm93c2VyOmZpeHR1cmUtb25seS1zZWNyZXQ="
        );
    } else {
        assert_eq!(params["client_id"], "crab-browser");
    }
    let challenge = PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(
        params["code_verifier"].clone(),
    ));
    assert_eq!(challenge.as_str(), flow["code_challenge"]);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mode = provider.mode.lock().await.clone();
    let mut claims = json!({"iss":provider.issuer,"aud":"crab-browser","exp":now+3600,"iat":now,"sub":"alice-id","preferred_username":"Alice","nonce":flow["nonce"]});
    match mode.as_str() {
        "member" => {
            claims["sub"] = json!("bob-id");
            claims["preferred_username"] = json!("Bob");
        }
        "nonce" => claims["nonce"] = json!("another-nonce"),
        "issuer" => claims["iss"] = json!("https://other.invalid"),
        "audience" => claims["aud"] = json!("other-client"),
        "authorized_party" => claims["azp"] = json!("other-client"),
        "expired" => claims["exp"] = json!(now - 1),
        "future" => claims["iat"] = json!(now + 600),
        "outsider" => claims["sub"] = json!("outsider-id"),
        _ => {}
    }
    let claims: CoreIdTokenClaims = serde_json::from_value(claims).unwrap();
    let key = if mode == "signature" {
        CoreEdDsaPrivateSigningKey::from_ed25519_pem(KEY_TWO, Some(JsonWebKeyId::new("one".into())))
            .unwrap()
    } else {
        provider.signing_key()
    };
    let signed = CoreIdToken::new(
        claims,
        &key,
        CoreJwsSigningAlgorithm::EdDsa,
        Some(&AccessToken::new("test-access".into())),
        None,
    )
    .unwrap();
    Json(
        json!({"access_token":if mode == "access_hash" {"substituted"} else {"test-access"},"token_type":"Bearer","id_token":signed}),
    )
}

async fn github_authorize(
    State(provider): State<Arc<Provider>>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Redirect {
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["client_id"], "crab-browser");
    assert_eq!(params["scope"], "read:user");
    assert_eq!(params["code_challenge_method"], "S256");
    let code = openidconnect::CsrfToken::new_random().secret().clone();
    let mut target = Url::parse(&params["redirect_uri"]).unwrap();
    target
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &params["state"]);
    provider.codes.lock().await.insert(code, params);
    Redirect::to(target.as_str())
}

async fn github_token(State(provider): State<Arc<Provider>>, body: String) -> Json<Value> {
    let params: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    let flow = provider.codes.lock().await.remove(&params["code"]).unwrap();
    assert_eq!(params["client_id"], "crab-browser");
    assert_eq!(params["redirect_uri"], flow["redirect_uri"]);
    let challenge = PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(
        params["code_verifier"].clone(),
    ));
    assert_eq!(challenge.as_str(), flow["code_challenge"]);
    Json(json!({"access_token":"github-access-token","token_type":"bearer"}))
}

async fn github_user(headers: axum::http::HeaderMap) -> Json<Value> {
    assert_eq!(headers[header::AUTHORIZATION], "Bearer github-access-token");
    Json(json!({"id":4242,"login":"octocat","name":"Octo Cat"}))
}

pub(crate) async fn start_provider(port: u16) -> (Arc<Provider>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let provider = Arc::new(Provider {
        issuer: format!("http://{}", listener.local_addr().unwrap()),
        rotated: AtomicBool::new(false),
        confidential: AtomicBool::new(false),
        mode: Mutex::new(String::new()),
        codes: Mutex::new(HashMap::new()),
    });
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(metadata))
        .route("/jwks", get(keys))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .route("/github/authorize", get(github_authorize))
        .route("/github/token", post(github_token))
        .route("/github/user", get(github_user))
        .with_state(Arc::clone(&provider));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (provider, task)
}
