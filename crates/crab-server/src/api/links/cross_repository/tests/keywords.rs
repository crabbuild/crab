use super::*;

fn closing_description(issue: &Repo) -> String {
    format!(
        "Fixes {}/{}#1.\n\n`Fixes {}/{}#2`",
        issue.owner, issue.name, issue.owner, issue.name
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_queued_description_projection_recovers_and_stale_candidates_fail_closed() {
    use crate::{
        cells::repository::{GetPull, UpdatePull, UpdatePullInput, UpdatePullOutcome},
        repository::development_references,
    };
    let (h, alice, organization, issue, pull) = setup().await;
    let checkout = tempfile::tempdir().unwrap();
    git_pull(&h, &alice, &pull, checkout.path(), "").await;
    let repository = h.server.repositories.by_id(pull.id).unwrap();
    let cell = app::repository_cell(&h.server, &repository).unwrap();
    let commit_description = async |body: String| {
        let current = cell
            .client
            .query::<GetPull>(cell.target, None, 1)
            .await
            .unwrap()
            .output
            .unwrap();
        let changed = cell
            .client
            .command::<UpdatePull>(
                cell.target,
                app::mutation_identity().unwrap(),
                UpdatePullInput {
                    number: 1,
                    actor: current.author,
                    can_triage: false,
                    version: current.version,
                    title: None,
                    body: Some(body),
                    state: None,
                    reopen_available: false,
                    label_ids: None,
                    assignees: None,
                },
            )
            .await
            .unwrap()
            .output;
        assert!(matches!(changed, UpdatePullOutcome::Updated(_)));
    };
    let issue_links = issue.api("/issues/1/links");
    commit_description(closing_description(&issue)).await;
    assert_eq!(
        h.json(&issue_links, &alice).await["cross_repository"],
        json!([])
    );
    development_references::recover(&h.server, &repository)
        .await
        .unwrap();
    assert_eq!(
        h.json(&issue_links, &alice).await["cross_repository"][0]["from_description"],
        true
    );

    commit_description("Removed reference".into()).await;
    assert_eq!(
        h.json(&issue_links, &alice).await["cross_repository"],
        json!([])
    );
    development_references::recover(&h.server, &repository)
        .await
        .unwrap();
    let candidates = cell
        .client
        .query::<graph::descriptions::Pulls>(
            Address::Organization(organization),
            None,
            Destination {
                repository: issue.id,
                number: 1,
            },
        )
        .await
        .unwrap()
        .output;
    assert!(candidates.is_empty());
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn qualified_descriptions_follow_edits_and_the_default_branch_then_keep_merge_history() {
    let (h, alice, _, issue, pull) = setup().await;
    let checkout = tempfile::tempdir().unwrap();
    let description = format!(
        "{}\n\nFixes {}/{}#2",
        closing_description(&issue),
        issue.owner,
        issue.name
    );
    let (opened, remote) = git_pull(&h, &alice, &pull, checkout.path(), &description).await;
    let issue_links = issue.api("/issues/1/links");
    let pull_links = pull.api("/pulls/1/links");
    let expect_description = async |path: &str| {
        let page = h.json(path, &alice).await;
        let rows = page["cross_repository"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{page}");
        assert_eq!(rows[0]["from_description"], true);
        assert_eq!(rows[0]["closes_on_merge"], true);
        assert!(rows[0].get("version").is_none());
    };
    expect_description(&pull_links).await;
    expect_description(&issue_links).await;

    issue.set_bob_access(&h, Some(RepositoryAccess::Read)).await;
    pull.set_bob_access(&h, Some(RepositoryAccess::Read)).await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        h.json(&issue_links, &bob).await["cross_repository"][0]["from_description"],
        true
    );
    pull.set_bob_access(&h, None).await;
    assert_eq!(
        h.json(&issue_links, &bob).await["cross_repository"],
        json!([])
    );

    let edit = async |body: &str| {
        let version = h.json(&pull.api("/pulls/1"), &alice).await["version"].clone();
        pull.write(
            &h,
            Method::PATCH,
            "/pulls/1",
            &alice,
            json!({"body":body,"version":version}),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    edit("Removed reference").await;
    assert_eq!(
        h.json(&issue_links, &alice).await["cross_repository"],
        json!([])
    );
    assert_eq!(
        h.json(&pull_links, &alice).await["cross_repository"],
        json!([])
    );
    edit(&description).await;
    expect_description(&issue_links).await;

    git::success(
        checkout.path(),
        &["push", remote.as_str(), "main:refs/heads/release"],
    )
    .await;
    for base in ["refs/heads/release", "refs/heads/main"] {
        let latest = h.json(&pull.api("/pulls/1"), &alice).await;
        let response = pull
            .write(
                &h,
                Method::POST,
                "/pulls/1/base",
                &alice,
                json!({"request_id":Uuid::now_v7(),"version":latest["version"],"base_ref":base,
              "base_oid":opened["base_oid"],"head_oid":opened["head_oid"]}),
            )
            .await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{}",
            response.text().await.unwrap()
        );
        if base.ends_with("/release") {
            assert_eq!(
                h.json(&issue_links, &alice).await["cross_repository"],
                json!([])
            );
            assert_eq!(
                h.json(&pull_links, &alice).await["cross_repository"],
                json!([])
            );
        }
    }
    expect_description(&issue_links).await;
    let latest = h.json(&pull.api("/pulls/1"), &alice).await;
    let merge = pull
        .write(
            &h,
            Method::POST,
            "/pulls/1/merge",
            &alice,
            json!({"request_id":Uuid::now_v7(),"version":latest["version"],"method":"squash",
              "base_oid":latest["base_oid"],"head_oid":latest["head_oid"]}),
        )
        .await;
    assert_eq!(
        merge.status(),
        StatusCode::OK,
        "{}",
        merge.text().await.unwrap()
    );
    assert_eq!(
        h.json(&issue.api("/issues/1"), &alice).await["state_reason"],
        "completed"
    );
    edit("Changed after publication").await;
    issue
        .write(
            &h,
            Method::POST,
            "/issues",
            &alice,
            json!({"request_id":Uuid::now_v7(),"title":"Created after merge","body":""}),
        )
        .await
        .error_for_status()
        .unwrap();
    expect_description(&issue_links).await;
    expect_description(&pull_links).await;
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn browser_shows_a_qualified_reference_on_both_sidebars() {
    let (h, alice, _, issue, pull) = setup().await;
    let checkout = tempfile::tempdir().unwrap();
    git_pull(
        &h,
        &alice,
        &pull,
        checkout.path(),
        &closing_description(&issue),
    )
    .await;
    let browser = Browser::new("qualified");
    browser
        .run(&["open", &format!("{}{}/pull/1", h.origin, pull.path())])
        .await;
    browser.sign_in().await;
    browser
        .run(&["wait", "--text", "From PR description"])
        .await;
    let title = format!("{}/{}#1 Coordinate rollout", issue.owner, issue.name);
    browser
        .run(&["find", "role", "link", "click", "--name", &title, "--exact"])
        .await;
    browser
        .run(&["wait", "--url", &format!("**{}/issues/1*", issue.path())])
        .await;
    browser
        .run(&["wait", "--text", "From PR description"])
        .await;
    drop(browser);
    h.close().await;
}
