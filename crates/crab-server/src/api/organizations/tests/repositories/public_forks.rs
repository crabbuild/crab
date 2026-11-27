use super::*;
use crate::cells::organization::{Bootstrap, BootstrapInput};

#[tokio::test(flavor = "multi_thread")]
async fn public_forks_keep_destination_grants_and_survive_upstream_changes() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "public-forks").await;
    h.initialize_directory().await;
    let catalog = h.server.catalog().unwrap();
    catalog
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (source_organization, _, bob_actor) =
        crate::test_support::http::organizations::bootstrap(&h).await;
    let destination = Uuid::now_v7();
    let cells = h.server.cells().unwrap();
    cells
        .initialize(Address::Organization(destination))
        .await
        .unwrap();
    let destination_handle = crate::catalog::derive_organization_handle("destination", destination);
    cells
        .command::<crate::cells::directory::organizations::ClaimOrganizationHandle>(
            Address::Directory,
            app::mutation_identity().unwrap(),
            crate::cells::directory::organizations::HandleClaimInput {
                organization: destination,
                handle: destination_handle.clone(),
            },
        )
        .await
        .unwrap();
    cells
        .command::<Bootstrap>(
            Address::Organization(destination),
            app::mutation_identity().unwrap(),
            BootstrapInput {
                actor: bob_actor,
                handle: destination_handle.clone(),
                name: "Destination".into(),
                description: String::new(),
                website: String::new(),
                email: String::new(),
                location: String::new(),
                avatar: None,
            },
        )
        .await
        .unwrap();
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let source: Value = h
        .write(
            Method::POST,
            &format!("/api/organizations/{source_organization}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"library","description":"","visibility":"public"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let source_id: Uuid = serde_json::from_value(source["id"].clone()).unwrap();
    let source_namespace = source["owner"].as_str().unwrap();
    let source_detail = format!("/api/repos?owner={source_namespace}&name=library");
    let source_repo = &h.json(&source_detail, &bob).await["repositories"][0];
    assert_eq!(source_repo["public_read_only"], true);
    assert_eq!(source_repo["can_fork"], true);
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":source_namespace,"repository":"library","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote =
        url::Url::parse(&format!("{}/git/{source_namespace}/library.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let local = tempfile::tempdir().unwrap();
    let feature = git::push_review_branches(local.path(), remote.as_str()).await;
    let completed = async |path: &str| {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                let status = h.json(path, &bob).await;
                match status["state"].as_str() {
                    Some("succeeded") => break status,
                    Some("queued" | "running") => {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await
                    }
                    _ => panic!("Public fork failed: {status}"),
                }
            }
        })
        .await
        .unwrap()
    };
    let forks = format!("/api/repos/{source_namespace}/library/forks");
    let org_job = Uuid::now_v7();
    let target_path = format!("/api/organizations/{destination}");
    repository_write(
        &h,
        Method::POST,
        &forks,
        &bob,
        source_id,
        json!({"request_id":org_job,"name":"library","destination":"organization",
            "organization":destination,"default_branch_only":true}),
    )
    .await
    .error_for_status()
    .unwrap();
    let org = completed(&format!("{target_path}/repositories/imports/git/{org_job}")).await;
    assert_eq!(org["visibility"], "public");
    let org_namespace = org["repository"]["owner"].as_str().unwrap();
    let org_detail = format!("/api/repos?owner={org_namespace}&name=library");
    let org_id: Uuid =
        serde_json::from_value(h.json(&org_detail, &bob).await["repositories"][0]["id"].clone())
            .unwrap();
    assert_eq!(
        h.json(&org_detail, &alice).await["repositories"][0]["can_admin"],
        false
    );
    let copied: Value = repository_write(
        &h, Method::POST, &format!("/api/repos/{org_namespace}/library/fork-sync"), &bob, org_id,
        json!({"branch":"refs/heads/feature","fork_oid":null,"upstream_oid":feature,"request_id":Uuid::now_v7()}),
    ).await.error_for_status().unwrap().json().await.unwrap();
    assert_eq!(copied["commit"], feature);

    let personal_job = Uuid::now_v7();
    repository_write(
        &h, Method::POST, &forks, &bob, source_id,
        json!({"request_id":personal_job,"name":"library","destination":"personal","default_branch_only":true}),
    ).await.error_for_status().unwrap();
    let personal = completed(&format!("/api/imports/git/{personal_job}")).await;
    assert_eq!(personal["visibility"], "public");
    let personal_namespace = personal["repository"]["owner"].as_str().unwrap();
    let personal_detail = format!("/api/repos?owner={personal_namespace}&name=library");
    let personal_repo = &h.json(&personal_detail, &bob).await["repositories"][0];
    let personal_id: Uuid = serde_json::from_value(personal_repo["id"].clone()).unwrap();
    assert_eq!(personal_repo["can_admin"], true);
    let anonymous = async |path: &str| -> Value {
        h.http
            .get(format!("{}{path}", h.origin))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    };
    assert_eq!(
        anonymous(&forks).await["repositories"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        anonymous(&personal_detail).await["repositories"][0]["forked_from"]["id"],
        json!(source_id)
    );
    let public_remote = format!("{}/git/{personal_namespace}/library.git", h.origin);
    let refs = git::git(local.path(), &["ls-remote", &public_remote]).await;
    assert!(refs.status.success());
    assert!(!refs.stdout.is_empty());

    let pending = Uuid::now_v7();
    catalog
        .reserve_git_import(
            BrowserNamespace::Organization {
                id: destination,
                owner: destination_handle,
            },
            crate::catalog::GitImportReservation {
                id: pending,
                name: "pending-fork".into(),
                description: String::new(),
                visibility: crate::catalog::visibility::Visibility::Public,
                forked_from: Some(source_id),
            },
            MembershipActor {
                issuer: h.provider.issuer.clone(),
                subject: "bob-id".into(),
            },
            Vec::new(),
        )
        .await
        .unwrap();
    h.write(
        Method::PUT,
        &format!("/api/repositories/{source_id}/visibility"),
        &alice,
        json!({"version":1,"value":"private","confirmation":format!("{source_namespace}/library")}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert!(
        catalog
            .load()
            .await
            .unwrap()
            .0
            .repository_by_id(pending)
            .is_none()
    );
    assert_eq!(anonymous(&source_detail).await["repositories"], json!([]));
    let org = anonymous(&org_detail).await;
    assert_eq!(org["repositories"][0]["id"], json!(org_id));
    assert_eq!(org["repositories"][0]["fork"], false);
    assert_eq!(
        anonymous(&personal_detail).await["repositories"][0]["forked_from"]["id"],
        json!(org_id)
    );

    let deletion = format!("/api/repositories/{org_id}/deletion");
    let estimate = h.json(&deletion, &bob).await;
    assert_eq!(estimate["forks"], 0);
    assert_eq!(estimate["public_forks"], 1);
    h.write(
        Method::DELETE,
        &deletion,
        &bob,
        json!({"request_id":Uuid::now_v7(),"version":0,
            "confirmation":format!("{org_namespace}/library")}),
    )
    .await
    .error_for_status()
    .unwrap();
    let personal = anonymous(&personal_detail).await;
    assert_eq!(personal["repositories"][0]["id"], json!(personal_id));
    assert_eq!(personal["repositories"][0]["visibility"], "public");
    assert_eq!(personal["repositories"][0]["fork"], false);

    // An independent personal repository can also be the upstream.
    let personal_source_forks = format!("/api/repos/{personal_namespace}/library/forks");
    let another_job = Uuid::now_v7();
    repository_write(
        &h, Method::POST, &personal_source_forks, &bob, personal_id,
        json!({"request_id":another_job,"name":"public-copy","destination":"organization","organization":destination}),
    ).await.error_for_status().unwrap();
    let stored = completed(&format!(
        "{target_path}/repositories/imports/git/{another_job}"
    ))
    .await;
    assert_eq!(stored["repository"]["name"], "public-copy");
    assert_eq!(
        anonymous(&personal_source_forks).await["repositories"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    h.close().await;
}
