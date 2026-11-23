use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{Method, Response};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;
use crate::{
    auth::Authentication,
    catalog::CatalogStore,
    cells::Address,
    server::Server,
    test_support::http::{Harness, organizations::bootstrap_on},
};

async fn replica(h: &Harness, host: &Host) -> (Arc<Server>, String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let root = h.server.catalog().unwrap().root().clone();
    let auth = Authentication::new(
        crate::OidcConfig {
            provider: crate::AuthProvider::Oidc,
            issuer: openidconnect::IssuerUrl::new(h.provider.issuer.clone()).unwrap(),
            public_url: url::Url::parse(&origin).unwrap(),
            client_id: "crab-browser".into(),
            client_secret_file: None,
            state_key_file: None,
            scim_token_file: None,
            github: None,
        },
        &root,
    )
    .await
    .unwrap();
    let repo = h.server.repositories.values()[0].clone();
    let mut server = Server::for_test(
        BTreeMap::from([((repo.config.owner.clone(), repo.config.name.clone()), repo)]).into(),
        crate::transfer_admission::TransferAdmission::new(
            root.store.clone(),
            "test/grants/admission".into(),
            4,
        ),
    );
    server.auth = Some(auth);
    server.catalog = Some(CatalogStore::new(root));
    server.cells = Some(host.cells.client.clone());
    let server = Arc::new(server);
    let router = crate::server::http::router(server.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (server, origin, task)
}

async fn write(
    h: &Harness,
    origin: &str,
    cookie: &str,
    method: Method,
    path: &str,
    input: Value,
) -> Response {
    let session = h.json("/api/session", cookie).await;
    h.http
        .request(method, format!("{origin}{path}"))
        .header("cookie", cookie)
        .header("origin", origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header(
            "x-crab-repository-id",
            h.server.repositories.values()[0].id.to_string(),
        )
        .json(&input)
        .send()
        .await
        .unwrap()
}

async fn publication(origin: &str, credential: &str) {
    let workspace = tempfile::tempdir().unwrap();
    let remote = format!("{origin}/git/team/private.git");
    let authorization = format!(
        "Authorization: Basic {}",
        STANDARD.encode(format!("crab:{credential}"))
    );
    for arguments in [
        vec!["init", "-b", "team-proof"],
        vec![
            "-c",
            "user.name=Team fixture",
            "-c",
            "user.email=team@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "Publish as team member",
        ],
        vec!["push", &remote, "HEAD:refs/heads/team-proof"],
    ] {
        let result = tokio::process::Command::new("git")
            .current_dir(workspace.path())
            .args(arguments)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_COUNT", "2")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", &authorization)
            .env("GIT_CONFIG_KEY_1", "protocol.version")
            .env("GIT_CONFIG_VALUE_1", "2")
            .output()
            .await
            .unwrap();
        assert!(
            result.status.success(),
            "Git action failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn organization_team_removal_revokes_existing_browser_git_and_lfs_access_across_replicas() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "repository-grants").await;
    let local = tempfile::TempDir::new().unwrap();
    let files = crate::test_support::tls::IdentityFiles::generate();
    let bucket = Bucket::open_dev(&local.path().join("objects.sqlite")).unwrap();
    let first = Host::start(
        local.path(),
        "grant-first",
        &files,
        bucket.clone(),
        crate::CellDurabilityMode::Object,
    )
    .await;
    let second = Host::start(
        local.path(),
        "grant-second",
        &files,
        bucket,
        crate::CellDurabilityMode::Object,
    )
    .await;
    first
        .cells
        .client
        .initialize(Address::Directory)
        .await
        .unwrap();
    let (organization, _, member) = bootstrap_on(&h, &first.cells.client).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let repo = h.server.repositories.values()[0].clone();
    first
        .cells
        .client
        .repository(repo.id)
        .unwrap()
        .client
        .initialize(repo.id)
        .await
        .unwrap();
    let catalog = h.server.catalog().unwrap();
    catalog.mark_cell_ready(repo.id).await.unwrap();
    catalog
        .set_members(
            "team",
            "private",
            vec![crate::RepositoryMember {
                subject: "other-admin".into(),
                name: "Other Admin".into(),
                access: crate::RepositoryAccess::Admin,
            }],
            true,
        )
        .await
        .unwrap();
    let (server_a, origin_a, task_a) = replica(&h, &first).await;
    let (server_b, origin_b, task_b) = replica(&h, &second).await;
    let revision = catalog
        .membership("team", "private")
        .await
        .unwrap()
        .revision;
    let binding = crate::server::management::router(server_a.clone(), Router::new())
        .oneshot(Request::put("/internal/repositories/team/private/organization")
            .header("content-type", "application/json")
            .body(Body::from(json!({"repository_id":repo.id,"expected_revision":revision,"organization":{"id":organization,"teams":[]}}).to_string())).unwrap())
        .await.unwrap();
    assert_eq!(binding.status(), StatusCode::OK);
    // Replicas without the background loop need the same catalog refresh
    // as production before they can authorize the new organization binding.
    let (document, _) = catalog.load().await.unwrap();
    server_b.install_catalog(document).await.unwrap();
    let org_path = format!("/api/organizations/{organization}");
    let offer: Value = write(
        &h,
        &origin_a,
        &alice,
        Method::PUT,
        &format!("{org_path}/invitations/{member}"),
        json!({"version":0,"role":"member"}),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    write(
        &h,
        &origin_a,
        &bob,
        Method::POST,
        &format!("{org_path}/invitation"),
        json!({"version":offer["version"],"accept":true}),
    )
    .await
    .error_for_status()
    .unwrap();
    let commands = format!("{org_path}/teams/commands");
    let team = Uuid::now_v7();
    write(&h, &origin_a, &alice, Method::POST, &commands,
        json!({"request_id":team,"command":{"kind":"create","name":"Publishers","description":"","visibility":"secret"}}))
        .await.error_for_status().unwrap();
    write(&h, &origin_a, &alice, Method::POST, &commands,
        json!({"request_id":Uuid::now_v7(),"command":{"kind":"member","team":team,"member":member,"version":0,"role":"member"}}))
        .await.error_for_status().unwrap();
    let team_access = "/api/repos/team/private/team-access";
    let revision = catalog
        .membership("team", "private")
        .await
        .unwrap()
        .revision;
    for grant in [
        json!({"team":team,"access":"admin"}),
        json!({"team":Uuid::now_v7(),"access":"write"}),
    ] {
        let denied = write(
            &h,
            &origin_a,
            &alice,
            Method::PUT,
            team_access,
            json!({"expected_revision":revision,"teams":[grant]}),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::BAD_REQUEST);
    }
    write(
        &h,
        &origin_a,
        &alice,
        Method::PUT,
        team_access,
        json!({"expected_revision":revision,"teams":[{"team":team,"access":"write"}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let token: Value = write(
        &h,
        &origin_b,
        &bob,
        Method::POST,
        "/api/git-token",
        json!({"owner":"team","repository":"private","access":"write"}),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    let token = token["token"].as_str().unwrap();
    publication(&origin_b, token).await;
    let probes = |origin: &str| {
        [
            h.http
                .get(format!("{origin}/api/repos/team/private/labels"))
                .header("cookie", &bob),
            h.http
                .get(format!(
                    "{origin}/git/team/private.git/info/refs?service=git-upload-pack"
                ))
                .header("Git-Protocol", "version=2")
                .basic_auth("crab", Some(token)),
            h.http
                .post(format!(
                    "{origin}/git/team/private.git/info/lfs/objects/batch"
                ))
                .basic_auth("crab", Some(token))
                .json(&json!({"operation":"download","objects":[]})),
        ]
    };
    for origin in [&origin_a, &origin_b] {
        for request in probes(origin) {
            assert_eq!(request.send().await.unwrap().status(), StatusCode::OK);
        }
        let visible: Value = h
            .http
            .get(format!("{origin}/api/repos"))
            .header("cookie", &bob)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(visible["repositories"][0]["access"], "write");
        assert_eq!(
            visible["repositories"][0]["organization_id"],
            json!(organization)
        );
    }
    assert_eq!(
        h.http
            .get(format!("{origin_b}{team_access}"))
            .header("cookie", &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    write(&h, &origin_a, &alice, Method::POST, &commands,
        json!({"request_id":Uuid::now_v7(),"command":{"kind":"member","team":team,"member":member,"version":1,"role":null}}))
        .await.error_for_status().unwrap();
    for origin in [&origin_b, &origin_a] {
        for request in probes(origin) {
            assert_eq!(
                request.send().await.unwrap().status(),
                StatusCode::NOT_FOUND
            );
        }
        let hidden: Value = h
            .http
            .get(format!("{origin}/api/repos"))
            .header("cookie", &bob)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(hidden["repositories"], json!([]));
    }
    let deploy_path = "/api/repos/team/private/settings/deploy-tokens";
    let created: Value = write(
        &h,
        &origin_a,
        &alice,
        Method::POST,
        deploy_path,
        json!({"expected_version":0,"edit":{"kind":"create","name":"CI checkout","days":1}}),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    let deploy = created["token"].as_str().unwrap();
    let checkout = |origin: &str| {
        h.http
            .get(format!(
                "{origin}/git/team/private.git/info/refs?service=git-upload-pack"
            ))
            .header("Git-Protocol", "version=2")
            .basic_auth("crab", Some(deploy))
    };
    let settings: Value = h
        .http
        .get(format!("{origin_b}{deploy_path}"))
        .header("cookie", &alice)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        checkout(&origin_b).send().await.unwrap().status(),
        StatusCode::OK
    );
    write(
        &h,
        &origin_a,
        &alice,
        Method::POST,
        deploy_path,
        json!({"expected_version":1,"edit":{"kind":"revoke","id":settings["tokens"][0]["id"]}}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        checkout(&origin_b).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    for task in [task_a, task_b] {
        task.abort();
        let _ = task.await;
    }
    crate::test_support::server::close(&server_b).await;
    crate::test_support::server::close(&server_a).await;
    second.stop(ShutdownMode::Preserve).await;
    first.stop(ShutdownMode::Preserve).await;
    h.close().await;
}
