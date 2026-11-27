use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn personal_fork_is_retired_on_removal_and_restored_only_with_previous_access() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "personal-fork").await;
    h.initialize_directory().await;
    let (organization, _, bob_actor) =
        crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    h.choose_username(&bob, "bob").await;
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    let org = format!("/api/organizations/{organization}");
    let invitation = format!("{org}/invitations/{bob_actor}");
    let invite = async |version: u64, previous: Option<u64>| {
        let mut input = json!({"version":version,"role":"member"});
        if let Some(previous) = previous {
            input["reinstate_version"] = json!(previous);
        }
        h.write(Method::PUT, &invitation, &alice, input)
            .await
            .error_for_status()
            .unwrap();
    };
    let accept = async |version| {
        h.write(
            Method::POST,
            &format!("{org}/invitation"),
            &bob,
            json!({"version":version,"accept":true}),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    invite(0, None).await;
    accept(1).await;

    let created: Value = h
        .write(
            Method::POST,
            &format!("{org}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"library","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let owner = created["owner"].as_str().unwrap();
    let source = format!("/api/repos/{owner}/library");

    let catalog = h.server.catalog().unwrap();
    let read = crate::RepositoryMember {
        subject: "bob-id".into(),
        name: "Bob".into(),
        access: crate::RepositoryAccess::Read,
    };
    catalog
        .set_members(
            owner,
            "library",
            vec![
                read.clone(),
                crate::RepositoryMember {
                    subject: "outsider-id".into(),
                    name: "Outside collaborator".into(),
                    access: crate::RepositoryAccess::Read,
                },
            ],
            true,
        )
        .await
        .unwrap();
    let graph = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({
                "owner":owner, "repository":"library","access":"write",
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let mut git_url = url::Url::parse(&format!("{}/git/{owner}/library.git", h.origin)).unwrap();
    git_url.set_username("crab").unwrap();
    git_url.set_password(graph["token"].as_str()).unwrap();
    let local = tempfile::tempdir().unwrap();
    let head = git::push_review_branches(local.path(), git_url.as_str()).await;

    let policy = format!("{org}/repository-policy");
    h.write(
        Method::PUT,
        &policy,
        &alice,
        json!({"version":0,"member_creation":false,"private_forks":true,"baseline":"read"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let upstream = h
        .json(&format!("/api/repos?owner={owner}&name=library"), &bob)
        .await;
    assert_eq!(upstream["repositories"][0]["access"], "read");
    assert_eq!(upstream["repositories"][0]["can_fork"], true);
    assert_eq!(
        upstream["repositories"][0]["can_fork_to_organization"],
        false
    );
    let request = Uuid::now_v7();
    repository_write(
        &h,
        Method::POST,
        &format!("{source}/forks"),
        &bob,
        id,
        json!({
            "request_id":request, "name":"library", "destination":"personal",
        }),
    )
    .await
    .error_for_status()
    .unwrap();

    let job = tokio::time::timeout(std::time::Duration::from_secs(45), async {
        loop {
            let status = h.json(&format!("/api/imports/git/{request}"), &bob).await;
            match status["state"].as_str() {
                Some("succeeded") => break status,
                Some("queued" | "running") => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await
                }
                _ => panic!("Personal fork failed: {status}"),
            }
        }
    })
    .await
    .unwrap();
    let namespace = "bob";
    assert_eq!(job["repository"]["owner"], namespace);
    let personal = h.json("/api/repos/personal", &bob).await;
    let fork = &personal["repositories"][0];
    assert_eq!(fork["owner"], namespace);
    assert_eq!(fork["fork"], true);
    let fork_id = fork["id"].clone();
    let forks = format!("{source}/forks");
    let network = h.json(&forks, &bob).await;
    assert_eq!(network["repositories"][0]["id"], fork_id);
    assert_eq!(network["next"], Value::Null);
    assert_eq!(h.json(&forks, &outsider).await["repositories"], json!([]));
    h.write(
        Method::PUT,
        &policy,
        &alice,
        json!({"version":1,"member_creation":false,"private_forks":true,"baseline":"admin"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let fork_access = catalog.membership(namespace, "library").await.unwrap();
    assert_eq!(
        fork_access.baseline,
        crate::catalog::RepositoryBaseline::Read
    );
    let detail = h
        .json(&format!("/api/repos?owner={namespace}&name=library"), &bob)
        .await;
    assert_eq!(detail["repositories"][0]["access"], "write");
    assert_eq!(detail["repositories"][0]["can_admin"], false);
    assert_eq!(detail["repositories"][0]["forked_from"]["id"], json!(id));
    let visible = h
        .json(&format!("/api/repos?organization={organization}"), &alice)
        .await;
    assert_eq!(visible["repositories"].as_array().unwrap().len(), 1);

    let fork_route = format!("/api/repos/{namespace}/library");
    let search = "/api/code-search?q=path:README.md+is:fork";
    let hits = h.json(search, &bob).await;
    assert_eq!(hits["groups"].as_array().unwrap().len(), 1);
    assert_eq!(hits["groups"][0]["id"], fork_id);
    let local_hits = h
        .json(
            &format!("{fork_route}/code-search?q=path:README.md+is:fork"),
            &bob,
        )
        .await;
    assert_eq!(local_hits["items"][0]["path"], "README.md");
    let check_fork = async |cookie: &str| {
        h.http
            .get(format!("{}{fork_route}/refs", h.origin))
            .header(header::COOKIE, cookie)
            .send()
            .await
            .unwrap()
    };
    let fork_refs: Value = check_fork(&bob)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        fork_refs["refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["oid"] == head)
    );
    h.write(
        Method::PUT,
        &policy,
        &alice,
        json!({"version":2,"member_creation":false,"private_forks":true,"baseline":null}),
    )
    .await
    .error_for_status()
    .unwrap();
    catalog
        .set_members(owner, "library", vec![], true)
        .await
        .unwrap();
    assert_eq!(check_fork(&bob).await.status(), StatusCode::OK);
    let detail = h
        .json(&format!("/api/repos?owner={namespace}&name=library"), &bob)
        .await;
    let fork = &detail["repositories"][0];
    assert_eq!(fork["id"], fork_id);
    assert_eq!(fork["fork"], true);
    assert!(fork.get("forked_from").is_none());
    let primary = "refs%2Fheads%2Fmain";
    for path in [
        forks.clone(),
        format!("{fork_route}/fork-sync?branch={primary}"),
        format!("{fork_route}/changes?rev={primary}&base={primary}&merge_base=true&upstream={id}"),
    ] {
        let response = h
            .http
            .get(format!("{}{path}", h.origin))
            .header(header::COOKIE, &bob)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    let fork = Uuid::parse_str(fork_id.as_str().unwrap()).unwrap();
    let main = fork_refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "refs/heads/main")
        .unwrap();
    assert_eq!(
        repository_write(
            &h,
            Method::POST,
            &format!("{fork_route}/fork-sync"),
            &bob,
            fork,
            json!({"branch":"refs/heads/main","fork_oid":main["oid"],"upstream_oid":main["oid"]}),
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    catalog
        .set_members(owner, "library", vec![read.clone()], true)
        .await
        .unwrap();
    let remove = async |version| {
        h.write(
            Method::DELETE,
            &format!("{org}/members/{bob_actor}"),
            &alice,
            json!({"version":version}),
        )
        .await
        .error_for_status()
        .unwrap();
    };

    remove(1).await;
    assert_eq!(h.json(search, &bob).await["groups"], json!([]));
    assert_eq!(check_fork(&alice).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(check_fork(&bob).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(h.json(&forks, &alice).await["repositories"], json!([]));
    assert_eq!(
        h.json(&format!("{org}/deleted-repositories"), &alice).await["items"],
        json!([])
    );
    invite(2, Some(2)).await;
    assert_eq!(check_fork(&bob).await.status(), StatusCode::NOT_FOUND);
    accept(3).await;
    let current = h.json("/api/repos/personal", &bob).await;
    assert_eq!(current["repositories"][0]["id"], fork_id);
    let current_refs: Value = check_fork(&bob)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(current_refs["refs"], fork_refs["refs"]);

    remove(3).await;
    invite(4, None).await;
    accept(5).await;
    catalog
        .set_members(owner, "library", vec![read], true)
        .await
        .unwrap();
    assert_eq!(check_fork(&bob).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        h.json("/api/repos/personal", &bob).await["repositories"],
        json!([])
    );
    h.close().await;
}
