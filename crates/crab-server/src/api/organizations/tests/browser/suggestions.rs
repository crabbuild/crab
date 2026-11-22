use std::path::Path;

use super::{Browser, Harness, Method, StatusCode, Uuid, Value, json};
use crate::api::organizations::tests::repositories::repository_write;
use crate::test_support::git::success as git;

pub(super) async fn apply_to_fork(
    h: &Harness,
    browser: &Browser,
    (upstream, fork): (Uuid, Uuid),
    (writer, reviewer): (&str, &str),
    (checkout, upstream_git, fork_git): (&Path, &str, &str),
) -> String {
    let api_path = |id| {
        let repo = h.server.repositories.by_id(id).unwrap();
        format!("/api/repos/{}/{}", repo.config.owner, repo.config.name)
    };
    let upstream_api = api_path(upstream);
    let fork_api = api_path(fork);
    let pull = format!("{upstream_api}/pulls/1");
    let members = format!("{upstream_api}/members");
    let acl = h.json(&members, writer).await;
    repository_write(h, Method::PUT, &members, writer, upstream,
        json!({"expected_revision":acl["revision"],"members":[{"subject":"bob-id","name":"Bob","access":"read"}],"groups":[]}))
        .await.error_for_status().unwrap();
    let selected = h.json(&pull, writer).await;
    let comparison_base = git(
        checkout,
        &[
            "merge-base",
            selected["base_oid"].as_str().unwrap(),
            selected["head_oid"].as_str().unwrap(),
        ],
    )
    .await;
    let mut threads = Vec::new();
    for (path, text) in [
        ("CONTRIBUTION.txt", "reviewed fork contribution"),
        ("NOTES.txt", "reviewed notes"),
    ] {
        let thread: Value = repository_write(h, Method::POST, &format!("{pull}/threads"), reviewer, upstream,
            json!({"request_id":Uuid::now_v7(),"body":"Suggested wording",
                "base_oid":comparison_base,"head_oid":selected["head_oid"],"path_hex":crate::api::encode_hex(path.as_bytes()),
                "side":"new","start_line":1,"end_line":1,"suggested_text":text}))
            .await.error_for_status().unwrap().json().await.unwrap();
        threads.push(json!({"number":thread["number"],"version":thread["version"]}));
    }
    let action = format!("{fork_api}/upstream-pulls/1/suggestions");
    let input = json!({"expected_head":selected["head_oid"],"threads":threads});
    assert_eq!(
        repository_write(h, Method::POST, &action, reviewer, fork, input.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        repository_write(
            h,
            Method::POST,
            &format!("{pull}/suggestions"),
            writer,
            upstream,
            input.clone()
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    let mut stale = input.clone();
    stale["threads"][0]["version"] = json!(2);
    assert_eq!(
        repository_write(h, Method::POST, &action, writer, fork, stale)
            .await
            .status(),
        StatusCode::CONFLICT
    );

    let protection = format!("{fork_api}/settings/branch-protections");
    repository_write(h, Method::PUT, &protection, writer, fork,
        json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":1,"required_checks":[]}]}))
        .await.error_for_status().unwrap();
    let rejected = repository_write(h, Method::POST, &action, writer, fork, input).await;
    assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        rejected.json::<Value>().await.unwrap()["error"]["code"],
        "protected_branch"
    );
    repository_write(
        h,
        Method::PUT,
        &protection,
        writer,
        fork,
        json!({"expected_version":1,"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();

    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Files changed",
            "--exact",
        ])
        .await;
    for thread in &threads {
        let selector = format!(
            "[aria-label='Select suggestion from thread #{}']",
            thread["number"]
        );
        browser.run(&["wait", &selector]).await;
        browser.run(&["click", &selector]).await;
    }
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Commit suggestions (2)",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Commit 2 suggestions"])
        .await;
    let dialogue = browser.run(&["get", "text", "[role=dialog]"]).await;
    assert!(dialogue.contains("library-fork"));
    browser
        .run(&[
            "find",
            "label",
            "Commit message",
            "fill",
            "Polish both fork files",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Commit changes",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Committed suggestions in"])
        .await;

    git(checkout, &["fetch", upstream_git, "main"]).await;
    assert_eq!(
        git(checkout, &["rev-parse", "FETCH_HEAD"]).await,
        selected["base_oid"].as_str().unwrap()
    );
    git(checkout, &["fetch", fork_git, "main"]).await;
    assert_eq!(
        git(checkout, &["rev-parse", "FETCH_HEAD^"]).await,
        selected["head_oid"].as_str().unwrap()
    );
    assert_eq!(
        git(checkout, &["show", "FETCH_HEAD:CONTRIBUTION.txt"]).await,
        "reviewed fork contribution"
    );
    assert_eq!(
        git(checkout, &["show", "FETCH_HEAD:NOTES.txt"]).await,
        "reviewed notes"
    );
    assert_eq!(
        git(checkout, &["show", "-s", "--format=%s", "FETCH_HEAD"]).await,
        "Polish both fork files"
    );
    let credits = git(
        checkout,
        &[
            "show",
            "-s",
            "--format=%(trailers:key=Co-authored-by,valueonly)",
            "FETCH_HEAD",
        ],
    )
    .await;
    assert_eq!(credits.lines().count(), 1);
    assert!(credits.starts_with("Bob <") && credits.ends_with("@users.crab.invalid>"));
    git(checkout, &["merge", "--ff-only", "FETCH_HEAD"]).await;
    git(checkout, &["rev-parse", "HEAD"]).await
}
