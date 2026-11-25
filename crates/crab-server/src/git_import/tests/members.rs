use super::*;
use crate::{
    cells::{
        Address,
        directory::{IdentityKey, RegisterAccounts},
    },
    git_import::workflow::reserve_destination,
    test_support::http::organizations::bootstrap,
};
use reqwest::Method;

#[tokio::test(flavor = "multi_thread")]
async fn member_import_preserves_destination_policy_and_creator_during_owner_recovery() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "member-import").await;
    h.initialize_directory().await;
    let catalog = h.server.catalog().unwrap();
    catalog
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, bob_actor) = bootstrap(&h).await;
    let cells = h.server.cells().unwrap();
    let peer_actor = cells
        .command::<RegisterAccounts>(
            Address::Directory,
            app::mutation_identity().unwrap(),
            vec![IdentityKey {
                issuer: h.provider.issuer.clone(),
                subject: "outsider-id".into(),
            }],
        )
        .await
        .unwrap()
        .output[0];
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    *h.provider.mode.lock().await = "outsider".into();
    let peer = h.login().await;
    let org = format!("/api/organizations/{organization}");
    for (actor, cookie) in [(bob_actor, &bob), (peer_actor, &peer)] {
        h.write(
            Method::PUT,
            &format!("{org}/invitations/{actor}"),
            &alice,
            json!({"version":0,"role":"member"}),
        )
        .await
        .error_for_status()
        .unwrap();
        h.write(
            Method::POST,
            &format!("{org}/invitation"),
            cookie,
            json!({"version":1,"accept":true}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let imports = format!("{org}/repositories/imports/git");
    let id = Uuid::now_v7();
    let job_path = format!("{imports}/{id}");
    let resume_path = format!("{job_path}/resume");
    let mut input = json!({
        "request_id": id, "source": "https://127.0.0.1:1/offline-source.git",
        "name": "member-imported", "visibility":"private"
    });
    assert_eq!(
        h.write(Method::POST, &imports, &bob, input.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let policy_path = format!("{org}/repository-policy");
    let mut policy = h.json(&policy_path, &alice).await;
    policy["member_creation"] = json!(true);
    policy["member_creation_visibility"] = json!("public");
    h.write(Method::PUT, &policy_path, &alice, policy)
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        h.write(Method::POST, &imports, &bob, input.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    input["visibility"] = json!("public");
    assert_eq!(
        h.write(Method::POST, &imports, &bob, input).await.status(),
        StatusCode::ACCEPTED
    );
    wait_for_state(&h, &bob, &job_path, "failed").await;
    assert_eq!(
        h.http
            .get(format!("{}{job_path}", h.origin))
            .header("cookie", &peer)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.write(Method::POST, &resume_path, &peer, json!({}))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    // Prime the stopped worker's exact refs with local Git, then recover
    // publication through the normal owner-facing HTTP route.
    let auth = h.server.auth.as_ref().unwrap();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(axum::http::header::COOKIE, alice.parse().unwrap());
    let principal = auth.principal(&headers).await;
    let account = ImportAccount::resolve(&h.server, &principal, Some(organization))
        .await
        .unwrap();
    let jobs = account.jobs(catalog.root());
    let (_, request) = jobs
        .resume_request(id, None, &["127.0.0.1".into()])
        .await
        .unwrap();
    let request = request.unwrap();
    let account = account.for_request(&request);
    let writer = Arc::new(jobs.claim(id, &request).await.unwrap().unwrap());
    let fixture = fixture_mirror().await;
    let mirror = fixture.path().join(".git");
    let cancellation = CancellationToken::new();
    let refs = mirror_refs(&mirror, &cancellation).await.unwrap();
    let snapshot = jobs::Snapshot::new("trunk".into(), &refs);
    writer.pin(&snapshot).await.unwrap();
    let repository_id = jobs.repository_id(id);
    let record = reserve_destination(
        &h.server,
        &account,
        &writer,
        &catalog,
        BrowserNamespace::Organization(organization),
        GitImportReservation {
            id: repository_id,
            name: request.name.clone(),
            description: request.description.clone(),
            visibility: request.visibility,
            forked_from: None,
        },
    )
    .await
    .unwrap();
    headers.insert(axum::http::header::COOKIE, bob.parse().unwrap());
    let principal = auth.principal(&headers).await;
    let member = ImportAccount::resolve(&h.server, &principal, Some(organization))
        .await
        .unwrap()
        .for_request(&request)
        .organization
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
        Some(member),
        None,
    )
    .await
    .unwrap();
    member
        .authorize_publication(&h.server, repository_id, &snapshot, 0)
        .await
        .unwrap();
    let mut policy = h.json(&policy_path, &alice).await;
    policy["member_creation"] = json!(false);
    h.write(Method::PUT, &policy_path, &alice, policy)
        .await
        .error_for_status()
        .unwrap();
    assert!(matches!(
        finish_destination(
            &h.server,
            &catalog,
            &writer,
            repository_id,
            Some(member),
            None,
            &snapshot,
        )
        .await,
        Err(ImportError::CreationRestricted)
    ));
    assert!(h.server.repositories.by_id(repository_id).is_none());
    jobs.expire(id).await;
    assert_eq!(h.json(&job_path, &bob).await["state"], "interrupted");
    assert_eq!(
        h.write(Method::POST, &resume_path, &bob, json!({}))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.write(Method::POST, &resume_path, &alice, json!({}))
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    wait_for_state(&h, &bob, &job_path, "succeeded").await;
    let listing = h
        .json(&format!("/api/repos?organization={organization}"), &bob)
        .await;
    assert_eq!(listing["repositories"][0]["id"], json!(repository_id));
    assert_eq!(listing["repositories"][0]["can_admin"], true);
    let imported = h.server.repositories.by_id(repository_id).unwrap();
    assert_eq!(imported.open_ref_view().await.unwrap().refs(), &refs);
    h.close().await;
}
