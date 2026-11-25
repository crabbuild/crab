use super::*;
use super::{
    git::{default_branch, mirror_refs},
    workflow::{finish_destination, open_destination, publish_snapshot},
};
use crate::test_support::{git::git, http::Harness};
use crate::{
    RepositoryAccess, RepositoryMember,
    auth::Identity,
    catalog::{BrowserNamespace, MembershipActor},
};
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

mod xet;

#[tokio::test(flavor = "multi_thread")]
async fn resumes_published_git_snapshot_after_worker_loss() {
    crate::test_support::server::init_test_tracing();
    let h = Harness::new_with_store_and_prefix(
        false,
        false,
        crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new())),
        "git-import-recovery",
    )
    .await;
    h.initialize_directory().await;
    let cookie = h.login().await;
    let session = h.json("/api/session", &cookie).await;
    let actor = Identity {
        issuer: session["user"]["issuer"].as_str().unwrap().into(),
        subject: session["user"]["subject"].as_str().unwrap().into(),
        name: session["user"]["name"].as_str().unwrap().into(),
    };
    let catalog = h.server.catalog().unwrap();
    catalog
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let context = h.server.git_import.as_ref().unwrap();
    let jobs = jobs::Jobs::new(catalog.root(), &actor);
    let id = Uuid::now_v7();
    let repository_id = jobs.repository_id(id);
    let request = ValidatedImport {
        fork_source: None,
        source: "https://127.0.0.1:1/offline-source.git".into(),
        owner: "team".into(),
        name: "imported".into(),
        description: "Imported repository".into(),
        token: None,
    };
    let job = Arc::new(jobs.create(id, &request).await.unwrap().unwrap());
    job.update(jobs::State::Running, "Publishing")
        .await
        .unwrap();

    let source = fixture_mirror().await;
    let mirror = source.path().join(".git");
    let cancellation = CancellationToken::new();
    let refs = mirror_refs(&mirror, &cancellation).await.unwrap();
    let snapshot = jobs::Snapshot::new(
        default_branch(&mirror, &refs, &cancellation).await.unwrap(),
        &refs,
    );
    job.pin(&snapshot).await.unwrap();
    let record = catalog
        .reserve_git_import(
            repository_id,
            BrowserNamespace::Personal(request.owner),
            request.name,
            request.description,
            MembershipActor {
                issuer: actor.issuer,
                subject: actor.subject.clone(),
            },
            vec![RepositoryMember {
                subject: actor.subject,
                name: actor.name,
                access: RepositoryAccess::Admin,
            }],
            None,
        )
        .await
        .unwrap();
    let private = open_destination(&catalog, &record, &snapshot)
        .await
        .unwrap();

    let grant = context.begin_repository(
        Arc::clone(&private),
        Arc::clone(&job),
        snapshot.clone(),
        None,
        None,
    );
    let request_for = |url: &str| {
        axum::extract::Request::builder()
            .uri(url)
            .header("host", h.origin.trim_start_matches("http://"))
            .header("x-crab-internal-import", grant.key.to_hex().as_str())
            .body(axum::body::Body::empty())
            .unwrap()
    };
    assert!(
        context
            .authorizes_git(&request_for(
                "/git/team/imported.git/info/refs?service=git-receive-pack"
            ))
            .is_some()
    );
    for route in [
        "/git/team/private.git/info/refs?service=git-receive-pack",
        "/git/team/imported.git/info/refs?service=git-upload-pack",
        "/git/team/imported.git/info/refs/extra?service=git-receive-pack",
        "/api/imports/git",
    ] {
        assert!(context.authorizes_git(&request_for(route)).is_none());
    }
    drop(grant);

    h.drop_next_import_reply.store(true, Ordering::SeqCst);
    publish_snapshot(
        context,
        &job,
        snapshot.clone(),
        private,
        &mirror,
        &cancellation,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(!h.drop_next_import_reply.load(Ordering::SeqCst));
    // Lose the worker after the stock Git push committed but before publishing
    // Cell readiness. The interrupted response must leave a private reservation.
    assert!(
        !h.json("/api/repos", &cookie).await["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|repo| repo["name"] == "imported")
    );
    jobs.expire(id).await;
    let path = format!("/api/imports/git/{id}");
    assert_eq!(h.json(&path, &cookie).await["state"], "interrupted");
    let response = h
        .write(
            reqwest::Method::POST,
            &format!("{path}/resume"),
            &cookie,
            json!({}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["id"],
        id.to_string()
    );
    wait_for_state(&h, &cookie, &path, "succeeded").await;
    let repository = h.server.repositories.by_id(repository_id).unwrap();
    let view = repository.open_ref_view().await.unwrap();
    assert_eq!(view.head(), "refs/heads/trunk");
    assert_eq!(view.refs(), &refs);
    assert!(
        h.json("/api/repos", &cookie).await["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|repo| repo["id"] == repository_id.to_string())
    );
    let token: serde_json::Value = h
        .write(
            reqwest::Method::POST,
            "/api/git-token",
            &cookie,
            json!({"owner":"team","repository":"imported","access":"read"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/team/imported.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote
        .set_password(Some(token["token"].as_str().unwrap()))
        .unwrap();
    let visible = git(
        source.path(),
        &[
            "-c",
            "protocol.version=2",
            "ls-remote",
            "--symref",
            remote.as_str(),
        ],
    )
    .await;
    assert!(visible.status.success());
    let listing = String::from_utf8(visible.stdout).unwrap();
    assert!(
        listing
            .lines()
            .any(|line| line == "ref: refs/heads/trunk\tHEAD")
    );
    assert!(
        refs.iter()
            .all(|(name, oid)| listing.lines().any(|line| line == format!("{oid}\t{name}")))
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn organization_import_requires_current_ownership_and_resumes_under_another_owner() {
    let h = Harness::new_with_store_and_prefix(
        false,
        false,
        crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new())),
        "organization-import-recovery",
    )
    .await;
    h.initialize_directory().await;
    let catalog = h.server.catalog().unwrap();
    catalog
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, alice_id, bob_id) =
        crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let root = format!("/api/organizations/{organization}");
    let endpoint = format!("{root}/repositories/imports/git");
    let id = Uuid::now_v7();
    let path = format!("{endpoint}/{id}");
    let resume_path = format!("{path}/resume");
    let destination = crate::catalog::organization_namespace(organization);
    let body = json!({"request_id": id, "source": "https://127.0.0.1:1/offline-source.git", "name": "imported"});
    let forbidden = h
        .write(reqwest::Method::POST, &endpoint, &bob, body.clone())
        .await;
    assert_eq!(forbidden.status(), StatusCode::NOT_FOUND);
    let mut spoofed = body.clone();
    spoofed["owner"] = json!(destination);
    let forbidden = h
        .write(reqwest::Method::POST, "/api/imports/git", &alice, spoofed)
        .await;
    assert_eq!(forbidden.status(), StatusCode::BAD_REQUEST);
    let start = h
        .write(reqwest::Method::POST, &endpoint, &alice, body)
        .await;
    assert_eq!(start.status(), StatusCode::ACCEPTED);
    let queued: serde_json::Value = start.json().await.unwrap();
    assert_eq!(queued["id"], json!(id));
    assert_eq!(queued["owner"], json!(destination));
    wait_for_state(&h, &alice, &path, "failed").await;

    use reqwest::Method;
    h.write(
        Method::PUT,
        &format!("{root}/invitations/{bob_id}"),
        &alice,
        json!({"version":0,"role":"owner"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{root}/invitation"),
        &bob,
        json!({"version":1,"accept":true}),
    )
    .await
    .error_for_status()
    .unwrap();

    let scope = jobs::Jobs::for_organization(catalog.root(), organization);
    let (_, request) = scope
        .resume_request(id, None, &["127.0.0.1".into()])
        .await
        .unwrap();
    let request = request.unwrap();
    let writer = Arc::new(scope.claim(id, &request).await.unwrap().unwrap());
    let alice_owner = import_owner(&h, &alice, organization).await;
    let source = fixture_mirror().await;
    let cancellation = CancellationToken::new();
    let mirror = source.path().join(".git");
    let refs = mirror_refs(&mirror, &cancellation).await.unwrap();
    let snapshot = jobs::Snapshot::new(
        default_branch(&mirror, &refs, &cancellation).await.unwrap(),
        &refs,
    );
    writer.pin(&snapshot).await.unwrap();
    let repository_id = scope.repository_id(id);
    let record = catalog
        .reserve_git_import(
            repository_id,
            BrowserNamespace::Organization(organization),
            request.name,
            request.description,
            MembershipActor {
                issuer: h.provider.issuer.clone(),
                subject: "alice-id".into(),
            },
            Vec::new(),
            None,
        )
        .await
        .unwrap();
    let repository = open_destination(&catalog, &record, &snapshot)
        .await
        .unwrap();
    publish_snapshot(
        h.server.git_import.as_ref().unwrap(),
        &writer,
        snapshot.clone(),
        repository,
        &mirror,
        &cancellation,
        Some(alice_owner),
        None,
    )
    .await
    .unwrap();

    // The committed refs stay private through application Cell preparation.
    h.write(
        Method::DELETE,
        &format!("{root}/members/{alice_id}"),
        &bob,
        json!({"version":1}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert!(matches!(
        finish_destination(
            &h.server,
            &catalog,
            &writer,
            repository_id,
            Some(alice_owner),
            None,
            &snapshot
        )
        .await,
        Err(ImportError::NotFound)
    ));
    assert!(h.server.repositories.by_id(repository_id).is_none());
    assert_eq!(
        h.json(&format!("/api/repos?organization={organization}"), &bob)
            .await["repositories"],
        json!([])
    );
    let policy = format!("{root}/rulesets");
    let property = Uuid::now_v7();
    h.write(
        Method::PUT,
        &format!("{root}/repository-properties"),
        &bob,
        json!({"expected_version":0,"definitions":[
            {"id":property,"name":"regulated","description":"","kind":"boolean","options":[],"default":true}
        ]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let rule = json!({
        "id":Uuid::now_v7(),"name":"Block imported branch creation","target":"branch","enforcement":"active",
        "include":["~ALL"],"exclude":[],"restrict_creations":true,"restrict_deletions":false,
        "require_pull_request":false,"required_approvals":0,"required_code_owner_reviews":false,
        "required_checks":[],"required_integrations":[],
    });
    h.write(
        Method::PUT,
        &policy,
        &bob,
        json!({"expected_version":0,"rulesets":[
            {"ruleset":rule,"repositories":{"kind":"properties","conditions":[
                {"property":property,"value":true},
                {"system":"fork","value":false},
                {"system":"name","value":"import*"}
            ]}}
        ]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let bob_owner = import_owner(&h, &bob, organization).await;
    assert!(matches!(
        finish_destination(
            &h.server,
            &catalog,
            &writer,
            repository_id,
            Some(bob_owner),
            None,
            &snapshot
        )
        .await,
        Err(ImportError::Ruleset)
    ));
    assert!(h.server.repositories.by_id(repository_id).is_none());
    h.write(
        Method::PUT,
        &policy,
        &bob,
        json!({"expected_version":1,"rulesets":[]}),
    )
    .await
    .error_for_status()
    .unwrap();

    scope.expire(id).await;
    assert_eq!(
        h.write(Method::POST, &resume_path, &alice, json!({}))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.http
            .get(format!("{}{path}", h.origin))
            .header("cookie", &alice)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let wrong_scope = h
        .write(
            Method::POST,
            &format!("/api/imports/git/{id}/resume"),
            &bob,
            json!({}),
        )
        .await;
    assert_eq!(wrong_scope.status(), StatusCode::NOT_FOUND);
    assert_eq!(h.json(&path, &bob).await["state"], "interrupted");
    let resumed = h.write(Method::POST, &resume_path, &bob, json!({})).await;
    assert_eq!(resumed.status(), StatusCode::ACCEPTED);
    wait_for_state(&h, &bob, &path, "succeeded").await;

    let listed = h
        .json(&format!("/api/repos?organization={organization}"), &bob)
        .await;
    assert_eq!(listed["repositories"][0]["id"], json!(repository_id));
    assert_eq!(
        listed["repositories"][0]["organization_id"],
        json!(organization)
    );
    assert_eq!(listed["repositories"][0]["can_admin"], json!(true));
    let imported = h.server.repositories.by_id(repository_id).unwrap();
    assert!(imported.config.members.is_empty());
    let view = imported.open_ref_view().await.unwrap();
    assert_eq!(view.head(), "refs/heads/trunk");
    assert_eq!(view.refs(), &refs);
    let bob_owner = import_owner(&h, &bob, organization).await;
    bob_owner
        .authorize_publication(&h.server, repository_id, &snapshot, 2)
        .await
        .unwrap();
    let changed = jobs::Snapshot::new("other".into(), &refs);
    assert!(matches!(
        bob_owner
            .authorize_publication(&h.server, repository_id, &changed, 2)
            .await,
        Err(ImportError::DestinationChanged)
    ));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn private_fork_resumes_after_missing_historical_lfs_payload_is_restored() {
    use crate::test_support::git::success as run_git;
    use reqwest::Method;
    use sha2::{Digest, Sha256};

    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "fork-lfs").await;
    h.initialize_directory().await;
    let catalog = h.server.catalog().unwrap();
    catalog
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, _) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    crate::test_support::http::organizations::set_private_forks(&h, organization, &alice, true)
        .await;
    let org = format!("/api/organizations/{organization}");
    let upstream: serde_json::Value = h
        .write(
            Method::POST,
            &format!("{org}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(), "name":"upstream", "description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let owner = upstream["owner"].as_str().unwrap();
    let source = h
        .server
        .repositories
        .get(&(owner.to_owned(), "upstream".into()))
        .unwrap();
    let token = |name, access| {
        h.write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":owner,"repository":name,"access":access}),
        )
    };
    let source_token: serde_json::Value = token("upstream", "write")
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let secret = source_token["token"].as_str().unwrap();
    let source_url = format!("{}/git/{owner}/upstream.git", h.origin);
    let mut remote = url::Url::parse(&source_url).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(Some(secret)).unwrap();
    let mut payload = vec![0x42; 8 * 1024 * 1024];
    payload.extend_from_slice(b"historical payload tail");
    let payload = bytes::Bytes::from(payload);
    let pointer = crab_git::LfsPointer {
        oid: Sha256::digest(&payload).into(),
        size: payload.len() as u64,
        extensions: Vec::new(),
    };
    let oid = crab_git::lfs_pointer::hex_encode(&pointer.oid);
    let object = format!("info/lfs/objects/{oid}?size={}", pointer.size);
    let upload = || {
        h.http
            .put(format!("{source_url}/{object}"))
            .basic_auth("crab", Some(secret))
            .body(payload.clone())
            .send()
    };
    upload().await.unwrap().error_for_status().unwrap();
    let local = tempfile::tempdir().unwrap();
    let path = local.path();
    run_git(
        path,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::write(path.join("asset.bin"), pointer.serialize()).unwrap();
    run_git(path, &["add", "asset.bin"]).await;
    run_git(path, &["commit", "-m", "Add large file"]).await;
    run_git(path, &["rm", "asset.bin"]).await;
    run_git(path, &["commit", "-m", "Remove large file from tip"]).await;
    run_git(path, &["push", remote.as_str(), "main"]).await;
    let source_lfs = crab_lfs::LfsObjectStore::new(source.store.clone(), &source.config.prefix);
    source_lfs.delete(&pointer.oid).await.unwrap();

    let id = Uuid::now_v7();
    let session = h.json("/api/session", &alice).await;
    let response = h
        .http
        .post(format!("{}/api/repos/{owner}/upstream/forks", h.origin))
        .header("cookie", &alice)
        .header("origin", &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header("x-crab-repository-id", source.id.to_string())
        .json(&json!({"request_id":id,"name":"fork"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let status = format!("{org}/repositories/imports/git/{id}");
    wait_for_state(&h, &alice, &status, "failed").await;
    let failed = h.json(&status, &alice).await;
    assert_eq!(failed["resumable"], true);
    assert!(
        failed["message"]
            .as_str()
            .unwrap()
            .contains("large-file payloads")
    );
    assert_eq!(token("fork", "read").await.status(), StatusCode::FORBIDDEN);

    upload().await.unwrap().error_for_status().unwrap();
    crate::test_support::http::organizations::set_private_forks(&h, organization, &alice, false)
        .await;
    assert_eq!(
        h.write(Method::POST, &format!("{status}/resume"), &alice, json!({}))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(token("fork", "read").await.status(), StatusCode::FORBIDDEN);
    crate::test_support::http::organizations::set_private_forks(&h, organization, &alice, true)
        .await;
    h.write(Method::POST, &format!("{status}/resume"), &alice, json!({}))
        .await
        .error_for_status()
        .unwrap();
    wait_for_state(&h, &alice, &status, "succeeded").await;
    // The fork must serve its verified copy without asking the source for data.
    source_lfs.delete(&pointer.oid).await.unwrap();
    let fork_token: serde_json::Value = token("fork", "read")
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let fork_url = format!("{}/git/{owner}/fork.git", h.origin);
    let fork_secret = fork_token["token"].as_str().unwrap();
    let batch: serde_json::Value = h
        .http
        .post(format!("{fork_url}/info/lfs/objects/batch"))
        .basic_auth("crab", Some(fork_secret))
        .json(&json!({"operation":"download","objects":[{"oid":oid,"size":pointer.size}]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let download = batch["objects"][0]["actions"]["download"]["href"]
        .as_str()
        .unwrap();
    let copied = h
        .http
        .get(download)
        .basic_auth("crab", Some(fork_secret))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(copied, payload);
    h.close().await;
}

async fn import_owner(h: &Harness, cookie: &str, organization: Uuid) -> OrganizationOwner {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(axum::http::header::COOKIE, cookie.parse().unwrap());
    let principal = h.server.auth.as_ref().unwrap().principal(&headers).await;
    ImportAccount::resolve(&h.server, &principal, Some(organization))
        .await
        .unwrap()
        .organization
        .unwrap()
}

async fn wait_for_state(h: &Harness, cookie: &str, path: &str, expected: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let status = h.json(path, cookie).await;
            let state = status["state"].as_str().unwrap();
            if state == expected {
                return;
            }
            assert!(
                !matches!(state, "failed" | "succeeded"),
                "job reached {state}: {}",
                status["message"]
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
}

async fn fixture_mirror() -> tempfile::TempDir {
    let source = tempfile::tempdir().unwrap();
    for args in [
        &["init", "-b", "trunk"][..],
        &["commit", "--allow-empty", "-m", "First commit"],
        &["branch", "alpha"],
        &["tag", "v1"],
    ] {
        let output = git(source.path(), args).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    source
}
