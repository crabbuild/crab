use super::{repositories::repository_write, *};
use crate::{
    cells::{Address, repository::RepositoryAuthor, repository::review_requests::reviewer_id},
    test_support::git,
};

#[tokio::test(flavor = "multi_thread")]
async fn team_review_routing_balances_and_rotates_across_organization_repositories() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h =
        Harness::new_with_store_and_prefix(false, false, store, "organization-review-load").await;
    h.initialize_directory().await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, bob_id) = crate::test_support::http::organizations::bootstrap(&h).await;
    let issuer = h.provider.issuer.clone();
    let carol_id = h
        .server
        .cells()
        .unwrap()
        .command::<RegisterAccounts>(
            Address::Directory,
            app::mutation_identity().unwrap(),
            vec![IdentityKey {
                issuer: issuer.clone(),
                subject: "carol-id".into(),
            }],
        )
        .await
        .unwrap()
        .output[0];
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    h.provider.claims.lock().await.extend([
        ("sub".into(), json!("carol-id")),
        ("preferred_username".into(), json!("Carol")),
    ]);
    let carol = h.login().await;
    h.provider.claims.lock().await.clear();

    let org = format!("/api/organizations/{organization}");
    let commands = format!("{org}/teams/commands");
    let team = Uuid::now_v7();
    let command = async |request_id: Uuid, command: Value| {
        h.write(
            Method::POST,
            &commands,
            &alice,
            json!({ "request_id": request_id, "command": command }),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    command(
        team,
        json!({"kind":"create","name":"Engineering","description":"","visibility":"closed"}),
    )
    .await;
    for (member, cookie) in [(bob_id, &bob), (carol_id, &carol)] {
        h.write(
            Method::PUT,
            &format!("{org}/invitations/{member}"),
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
        command(
            Uuid::now_v7(),
            json!({"kind":"member","team":team,"member":member,"version":0,"role":"member"}),
        )
        .await;
    }
    command(Uuid::now_v7(), json!({"kind":"review_routing","team":team,"version":0,"algorithm":"fewest_open","reviewers":1})).await;

    let create = async |name: &str, codeowners: bool| {
        let created: Value = h
            .write(
                Method::POST,
                &format!("{org}/repositories/creation"),
                &alice,
                json!({"request_id":Uuid::now_v7(),"name":name,"description":""}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
        let owner = created["owner"].as_str().unwrap();
        let repo = format!("/api/repos/{owner}/{name}");
        let access = format!("{repo}/team-access");
        let revision = h.json(&access, &alice).await["revision"].clone();
        repository_write(
            &h,
            Method::PUT,
            &access,
            &alice,
            id,
            json!({"expected_revision":revision,"teams":[{"team":team,"access":"write"}]}),
        )
        .await
        .error_for_status()
        .unwrap();
        let token: Value = h
            .write(
                Method::POST,
                "/api/git-token",
                &alice,
                json!({"owner":owner,"repository":name,"access":"write"}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut remote = url::Url::parse(&format!("{}/git/{owner}/{name}.git", h.origin)).unwrap();
        remote.set_username("crab").unwrap();
        remote.set_password(token["token"].as_str()).unwrap();
        let work = tempfile::tempdir().unwrap();
        let path = work.path();
        let head = git::push_review_branches(path, remote.as_str()).await;
        git::success(
            path,
            &[
                "push",
                remote.as_str(),
                "feature:refs/heads/feature-followup",
                "feature:refs/heads/feature-final",
            ],
        )
        .await;
        if codeowners {
            git::success(path, &["switch", "main"]).await;
            std::fs::write(path.join("CODEOWNERS"), format!("* @{owner}/engineering\n")).unwrap();
            git::success(path, &["add", "CODEOWNERS"]).await;
            git::success(path, &["commit", "-m", "Assign code owners"]).await;
            git::success(path, &["push", remote.as_str(), "main"]).await;
        }
        (id, repo, head)
    };
    let (source_id, source, head) = create("library", false).await;
    let (target_id, target, _) = create("consumer", true).await;
    let create_pull = async |repo: &str, id, head_ref: &str, draft| {
        let result: Value = repository_write(&h, Method::POST, &format!("{repo}/pulls"), &alice, id,
            json!({"request_id":Uuid::now_v7(),"title":"Load balance","body":"","base_ref":"refs/heads/main","head_ref":head_ref,"draft":draft}))
            .await.error_for_status().unwrap().json().await.unwrap();
        format!("{repo}/pulls/{}", result["number"].as_u64().unwrap())
    };
    let review = async |pull: &str, id, change| {
        let version = h.json(&format!("{pull}/reviewers"), &alice).await["version"].clone();
        let input = json!({"version":version,"change":change});
        repository_write(
            &h,
            Method::PUT,
            &format!("{pull}/review-requests"),
            &alice,
            id,
            input.clone(),
        )
        .await
        .error_for_status()
        .unwrap();
        input
    };
    let reviewers = [(bob_id, "bob-id", &bob), (carol_id, "carol-id", &carol)];
    let first = reviewers
        .iter()
        .min_by_key(|(_, subject, _)| {
            reviewer_id(&RepositoryAuthor {
                issuer: issuer.clone(),
                subject: (*subject).into(),
                name: String::new(),
            })
        })
        .unwrap();
    let alternate = reviewers
        .iter()
        .find(|candidate| candidate.0 != first.0)
        .unwrap();

    // The member that wins the local tie has already fulfilled a request
    // elsewhere. The first assignment in the empty repository should skip them.
    let pull = create_pull(&source, source_id, "refs/heads/feature", false).await;
    review(
        &pull,
        source_id,
        json!({"kind":"request","actor_id":first.0}),
    )
    .await;
    repository_write(
        &h,
        Method::POST,
        &format!("{pull}/reviews"),
        first.2,
        source_id,
        json!({"request_id":Uuid::now_v7(),"state":"approved","body":"Approved","commit_oid":head}),
    )
    .await
    .error_for_status()
    .unwrap();
    let pull = create_pull(&target, target_id, "refs/heads/feature", true).await;
    let manual = review(
        &pull,
        target_id,
        json!({"kind":"request_team","team_id":team}),
    )
    .await;
    let requests = h.json(&format!("{pull}/reviewers"), &alice).await;
    assert_eq!(requests["items"][0]["candidate_id"], json!(alternate.0));
    assert_eq!(requests["teams"][0]["state"], Value::Null);
    repository_write(
        &h,
        Method::PUT,
        &format!("{pull}/review-requests"),
        &alice,
        target_id,
        manual,
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(&format!("{pull}/reviewers"), &alice).await, requests);

    // The open request now contributes more than the fulfilled one. A new
    // CODEOWNERS request uses the same balance and keeps the team visible.
    let automatic = create_pull(&target, target_id, "refs/heads/feature-followup", false).await;
    let requests = h.json(&format!("{automatic}/reviewers"), &alice).await;
    assert_eq!(requests["items"][0]["candidate_id"], json!(first.0));
    assert_eq!(requests["items"][0]["request"]["code_owner"], true);
    assert_eq!(requests["teams"][0]["state"]["kind"], "requested");

    command(
        Uuid::now_v7(),
        json!({"kind":"review_routing","team":team,"version":1,"algorithm":"round_robin","reviewers":1}),
    )
    .await;
    let extra = create_pull(&source, source_id, "refs/heads/feature-followup", false).await;
    review(
        &extra,
        source_id,
        json!({"kind":"request","actor_id":alternate.0}),
    )
    .await;
    // Round robin favors the older team assignment even when that reviewer
    // now has more open work and the destination has no local rotation.
    let manual = create_pull(&source, source_id, "refs/heads/feature-final", false).await;
    review(
        &manual,
        source_id,
        json!({"kind":"request_team","team_id":team}),
    )
    .await;
    let requests = h.json(&format!("{manual}/reviewers"), &alice).await;
    assert_eq!(requests["items"][0]["candidate_id"], json!(alternate.0));

    // The newly rotated member was last used on a different repository.
    // CODEOWNERS must consider that assignment before the older local order.
    let automatic = create_pull(&target, target_id, "refs/heads/feature-final", false).await;
    let requests = h.json(&format!("{automatic}/reviewers"), &alice).await;
    assert_eq!(requests["items"][0]["candidate_id"], json!(first.0));
    assert_eq!(requests["items"][0]["request"]["code_owner"], true);
    h.close().await;
}
