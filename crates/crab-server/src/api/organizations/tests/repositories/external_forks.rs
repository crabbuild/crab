use super::*;
use crate::cells::{
    directory::{
        IdentityKey, RegisterAccounts,
        organizations::{Candidate, RecordCandidate},
    },
    organization::{Bootstrap, BootstrapInput},
};

#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_private_fork_keeps_destination_ownership_and_inherits_source_readers() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "external-fork").await;
    h.initialize_directory().await;
    let catalog = h.server.catalog().unwrap();
    catalog
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (source_org, _, bob_actor) = crate::test_support::http::organizations::bootstrap(&h).await;
    let destination = Uuid::now_v7();
    let cells = h.server.cells().unwrap();
    cells
        .initialize(Address::Organization(destination))
        .await
        .unwrap();
    cells
        .command::<Bootstrap>(
            Address::Organization(destination),
            app::mutation_identity().unwrap(),
            BootstrapInput {
                actor: bob_actor,
                handle: crate::catalog::derive_organization_handle(
                    "release-engineering",
                    destination,
                ),
                name: "Release engineering".into(),
                description: String::new(),
                website: String::new(),
                email: String::new(),
                location: String::new(),
                avatar: None,
            },
        )
        .await
        .unwrap();
    cells
        .command::<RecordCandidate>(
            Address::Directory,
            app::mutation_identity().unwrap(),
            Candidate {
                actor: bob_actor,
                organization: destination,
            },
        )
        .await
        .unwrap();
    let outsider_actor = cells
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
    let outsider = h.login().await;
    let source_org_path = format!("/api/organizations/{source_org}");
    let destination_path = format!("/api/organizations/{destination}");
    h.write(
        Method::PUT,
        &format!("{destination_path}/invitations/{outsider_actor}"),
        &bob,
        json!({"version":0,"role":"member"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{destination_path}/invitation"),
        &outsider,
        json!({"version":1,"accept":true}),
    )
    .await
    .error_for_status()
    .unwrap();
    let destination_policy = format!("{destination_path}/repository-policy");
    let mut policy = h.json(&destination_policy, &bob).await;
    policy["baseline"] = json!("admin");
    h.write(Method::PUT, &destination_policy, &bob, policy)
        .await
        .error_for_status()
        .unwrap();

    let source: Value = h
        .write(
            Method::POST,
            &format!("{source_org_path}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"library","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let source_id: Uuid = serde_json::from_value(source["id"].clone()).unwrap();
    let source_namespace = source["owner"].as_str().unwrap();
    let guest = crate::RepositoryMember {
        subject: "outsider-id".into(),
        name: "Outside collaborator".into(),
        access: crate::RepositoryAccess::Read,
    };
    catalog
        .set_members(
            source_namespace,
            "library",
            vec![
                guest.clone(),
                crate::RepositoryMember {
                    subject: "bob-id".into(),
                    name: "Bob".into(),
                    access: crate::RepositoryAccess::Read,
                },
            ],
            true,
        )
        .await
        .unwrap();
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

    let request_id = Uuid::now_v7();
    let source_route = format!("/api/repos/{source_namespace}/library");
    let fork_route = format!("{source_route}/forks");
    let request = json!({"request_id":request_id,"name":"library","organization":destination,"default_branch_only":true});
    crate::test_support::http::organizations::set_private_forks(&h, source_org, &alice, true).await;
    let denied = repository_write(
        &h,
        Method::POST,
        &fork_route,
        &bob,
        source_id,
        request.clone(),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        denied.json::<Value>().await.unwrap()["error"]["code"],
        "external_forking_restricted"
    );
    let source_policy = format!("{source_org_path}/repository-policy");
    let mut policy = h.json(&source_policy, &alice).await;
    policy["external_forks"] = json!(true);
    h.write(Method::PUT, &source_policy, &alice, policy)
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        repository_write(
            &h,
            Method::POST,
            &fork_route,
            &alice,
            source_id,
            request.clone()
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    repository_write(&h, Method::POST, &fork_route, &bob, source_id, request)
        .await
        .error_for_status()
        .unwrap();
    let job_path = format!("{destination_path}/repositories/imports/git/{request_id}");
    let completed = async |path: &str, cookie: &str| {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                let status = h.json(path, cookie).await;
                match status["state"].as_str() {
                    Some("succeeded") => break status,
                    Some("queued" | "running") => {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await
                    }
                    _ => panic!("External fork failed: {status}"),
                }
            }
        })
        .await
        .unwrap()
    };
    let job = completed(&job_path, &bob).await;
    let namespace = crate::catalog::derive_organization_handle("release-engineering", destination);
    assert_eq!(job["repository"]["owner"], namespace);
    let detail = format!("/api/repos?owner={namespace}&name=library");
    let fork = h.json(&detail, &bob).await["repositories"][0].clone();
    let fork_id: Uuid = serde_json::from_value(fork["id"].clone()).unwrap();
    assert_eq!(fork["organization_id"], json!(destination));
    assert_eq!(fork["can_admin"], true);
    assert_eq!(
        h.json(&format!("{source_route}/pulls/head-forks"), &bob)
            .await["items"][0]["id"],
        json!(fork_id)
    );
    assert_eq!(h.json(&detail, &outsider).await["repositories"], json!([]));
    let upstream_owner_view = &h.json(&detail, &alice).await["repositories"][0];
    assert_eq!(upstream_owner_view["access"], "read");
    assert_eq!(upstream_owner_view["can_admin"], false);
    let deletion = format!("/api/repositories/{fork_id}/deletion");
    let denied = h
        .http
        .get(format!("{}{deletion}", h.origin))
        .header(header::COOKIE, &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    assert_eq!(h.json(&deletion, &bob).await["can_delete"], true);
    let inspector = h
        .json(
            &format!("{destination_path}/members/{bob_actor}/repository-access"),
            &bob,
        )
        .await;
    assert_eq!(inspector["items"][0]["access"], "admin");
    let refs = h
        .json(&format!("/api/repos/{namespace}/library/refs"), &bob)
        .await;
    assert_eq!(refs["refs"].as_array().unwrap().len(), 1);
    let copied: Value = repository_write(
        &h, Method::POST, &format!("/api/repos/{namespace}/library/fork-sync"), &bob, fork_id,
        json!({"branch":"refs/heads/feature","fork_oid":null,"upstream_oid":feature,"request_id":Uuid::now_v7()}),
    ).await.error_for_status().unwrap().json().await.unwrap();
    assert_eq!(copied["commit"], feature);

    let mut policy = h.json(&destination_policy, &bob).await;
    policy["member_creation"] = json!(true);
    policy["member_creation_visibility"] = json!("public");
    h.write(Method::PUT, &destination_policy, &bob, policy)
        .await
        .error_for_status()
        .unwrap();
    let member_job = Uuid::now_v7();
    let member_request = json!({
        "request_id": member_job, "name":"member-library", "organization":destination,
        "default_branch_only":true,
    });
    assert_eq!(
        repository_write(
            &h,
            Method::POST,
            &fork_route,
            &outsider,
            source_id,
            member_request.clone()
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let mut policy = h.json(&destination_policy, &bob).await;
    policy["member_creation_visibility"] = json!("private");
    h.write(Method::PUT, &destination_policy, &bob, policy)
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        h.json("/api/organizations", &outsider).await["items"][0]["can_create_private"],
        true
    );
    repository_write(
        &h,
        Method::POST,
        &fork_route,
        &outsider,
        source_id,
        member_request,
    )
    .await
    .error_for_status()
    .unwrap();
    let member_job_path = format!("{destination_path}/repositories/imports/git/{member_job}");
    let member_fork = completed(&member_job_path, &outsider).await;
    assert_eq!(member_fork["repository"]["owner"], namespace);
    let member_detail = format!("/api/repos?owner={namespace}&name=member-library");
    let visible = &h.json(&member_detail, &outsider).await["repositories"][0];
    assert_eq!(visible["access"], "write");
    assert_eq!(visible["can_admin"], false);
    let member_fork_id: Uuid = serde_json::from_value(visible["id"].clone()).unwrap();
    repository_write(
        &h, Method::POST, &format!("/api/repos/{namespace}/member-library/fork-sync"), &outsider, member_fork_id,
        json!({"branch":"refs/heads/feature","fork_oid":null,"upstream_oid":feature,"request_id":Uuid::now_v7()}),
    ).await.error_for_status().unwrap();
    let inspector = h
        .json(
            &format!("{destination_path}/members/{outsider_actor}/repository-access"),
            &bob,
        )
        .await;
    let row = inspector["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == json!(member_fork_id))
        .unwrap();
    assert_eq!(row["fork_creator"], "write");
    let imported = h.server.repositories.by_id(member_fork_id).unwrap();
    let issues = format!("/api/repos/{namespace}/member-library/issues");
    let mention_creator = async |title: &str| {
        let issue: Value = repository_write(
            &h,
            Method::POST,
            &issues,
            &bob,
            member_fork_id,
            json!({"request_id":Uuid::now_v7(),"title":title,
                "body":format!("Review this, [@creator](/-/users/{outsider_actor}).")}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
        crate::server::mentions::drain_repository(&h.server, &imported)
            .await
            .unwrap();
        issue["number"].as_u64().unwrap()
    };
    let mentions = "/api/notifications?reason=mention";
    let first_issue = mention_creator("Fork creator review").await;
    let inbox = h.json(mentions, &outsider).await;
    let [notice] = inbox["items"].as_array().unwrap().as_slice() else {
        panic!("the fork creator must receive one mention");
    };
    assert_eq!(notice["repository"]["id"], json!(member_fork_id));
    assert_eq!(notice["title"], "Fork creator review");
    assert_eq!(
        h.json(&format!("{issues}/{first_issue}/subscription"), &outsider)
            .await["mode"],
        "follow"
    );
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &outsider,
            json!({"owner":namespace,"repository":"member-library","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut member_remote =
        url::Url::parse(&format!("{}/git/{namespace}/member-library.git", h.origin)).unwrap();
    member_remote.set_username("crab").unwrap();
    member_remote.set_password(token["token"].as_str()).unwrap();
    let list_refs = async || git::git(local.path(), &["ls-remote", member_remote.as_str()]).await;
    assert!(list_refs().await.status.success());

    catalog
        .set_members(source_namespace, "library", vec![], true)
        .await
        .unwrap();
    let fork = &h.json(&detail, &bob).await["repositories"][0];
    assert_eq!(fork["can_admin"], true);
    assert!(fork.get("forked_from").is_none());
    assert_eq!(
        h.json(&member_detail, &outsider).await["repositories"],
        json!([])
    );
    assert_eq!(
        h.json(&member_detail, &bob).await["repositories"][0]["can_admin"],
        true
    );
    let hidden_job = h
        .http
        .get(format!("{}{job_path}", h.origin))
        .header(header::COOKIE, &bob)
        .send()
        .await
        .unwrap();
    assert_eq!(hidden_job.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        h.write(Method::POST, &format!("{job_path}/resume"), &bob, json!({}))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert!(!list_refs().await.status.success());
    mention_creator("Mention after access removal").await;
    assert_eq!(h.json(mentions, &outsider).await["items"], json!([]));

    // Upstream group access can qualify this creator only. The inbox still
    // requires a current signed group before exposing the queued mention.
    let source_acl = format!("{source_route}/members");
    let current = h.json(&source_acl, &alice).await;
    repository_write(
        &h,
        Method::PUT,
        &source_acl,
        &alice,
        source_id,
        json!({"expected_revision":current["revision"],"members":[],
            "groups":[{"group":"release-reviewers","access":"read"}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    mention_creator("Fork creator group review").await;
    assert_eq!(h.json(mentions, &outsider).await["items"], json!([]));
    h.provider
        .claims
        .lock()
        .await
        .insert("groups".into(), json!(["release-reviewers"]));
    let signed_outsider = h.login().await;
    let inbox = h.json(mentions, &signed_outsider).await;
    let titles: Vec<_> = inbox["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Fork creator group review", "Fork creator review"]);
    let issue = mention_creator("Verified fork creator review").await;
    assert_eq!(
        h.json(&format!("{issues}/{issue}/subscription"), &signed_outsider)
            .await["mode"],
        "follow"
    );
    let current = h.json(&source_acl, &alice).await;
    repository_write(
        &h,
        Method::PUT,
        &source_acl,
        &alice,
        source_id,
        json!({"expected_revision":current["revision"],"members":[guest],"groups":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&member_detail, &outsider).await["repositories"][0]["access"],
        "write"
    );
    h.write(
        Method::DELETE,
        &format!("{destination_path}/members/{outsider_actor}"),
        &bob,
        json!({"version":1}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&member_detail, &outsider).await["repositories"],
        json!([])
    );
    assert!(!list_refs().await.status.success());
    let root_deletion = format!("/api/repositories/{source_id}/deletion");
    let deletion_id = Uuid::now_v7();
    h.write(
        Method::DELETE,
        &root_deletion,
        &alice,
        json!({"request_id":deletion_id,"version":0,
        "confirmation":format!("{source_namespace}/library")}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(&detail, &bob).await["repositories"], json!([]));
    let deleted = format!("{source_org_path}/deleted-repositories");
    assert_eq!(h.json(&deleted, &alice).await["items"][0]["forks"], 2);
    h.write(
        Method::PUT,
        &format!("{deleted}/{source_id}"),
        &alice,
        json!({"deletion_id":deletion_id}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&detail, &bob).await["repositories"][0]["id"],
        json!(fork_id)
    );
    h.write(
        Method::PUT,
        &format!("/api/repositories/{source_id}/visibility"),
        &alice,
        json!({"version":0,"value":"public",
        "confirmation":format!("{source_namespace}/library")}),
    )
    .await
    .error_for_status()
    .unwrap();
    let detached = &h.json(&detail, &bob).await["repositories"][0];
    assert_eq!(detached["fork"], false);
    assert_eq!(detached["organization_id"], json!(destination));
    assert_eq!(detached["visibility"], "private");
    assert_eq!(h.json(&detail, &alice).await["repositories"], json!([]));
    assert_eq!(h.json(&detail, &outsider).await["repositories"], json!([]));
    h.close().await;
}
