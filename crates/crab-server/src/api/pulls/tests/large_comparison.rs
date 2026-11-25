use crate::test_support::{
    git::{success as git, with_http_token},
    http::{Harness, browser::Browser},
};
use crab_remote_git::{
    Error, GitPath, ObjectLimits, OperationKind, OperationLimits, RepositoryOptions, Revision,
};
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

const REPO: &str = "/api/repos/team/private";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires stock Git and Chromium with the embedded UI built"]
async fn native_large_pull_opens_a_file_and_publishes_an_exact_review_anchor() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let credential: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = credential["token"].as_str().unwrap();
    let remote = format!("{}/git/team/private.git", h.origin);
    let source = tempfile::tempdir().unwrap();
    let path = source.path();
    git(
        path,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    git(path, &["config", "core.filemode", "false"]).await;
    std::fs::create_dir(path.join("replacement")).unwrap();
    for (file, text) in [
        ("README.md", "base text\n"),
        ("tool.sh", "echo fixture\n"),
        ("unchanged.txt", "stable\n"),
        ("deleted.txt", "previous content\n"),
        ("replacement/child.txt", "directory member\n"),
    ] {
        std::fs::write(path.join(file), text).unwrap();
    }
    git(path, &["add", "."]).await;
    git(path, &["commit", "-m", "Start the comparison"]).await;
    let ancestor = git(path, &["rev-parse", "HEAD"]).await;
    git(path, &["switch", "-c", "feature"]).await;
    git(path, &["rm", "-r", "replacement", "deleted.txt"]).await;
    std::fs::write(path.join("replacement"), "replacement text\n").unwrap();
    std::fs::write(path.join("README.md"), "base  text\nnew review line\n").unwrap();
    for directory in 0..64 {
        let directory_path = path.join(format!("unrelated/part-{directory:02}"));
        std::fs::create_dir_all(&directory_path).unwrap();
        for index in 0..8 {
            std::fs::write(
                directory_path.join(format!("{index}.txt")),
                format!("{directory}:{index}\n"),
            )
            .unwrap();
        }
    }
    git(path, &["add", "."]).await;
    git(path, &["update-index", "--chmod=+x", "tool.sh"]).await;
    git(path, &["commit", "-m", "Make a large reviewable change"]).await;
    let head = git(path, &["rev-parse", "HEAD"]).await;
    git(path, &["switch", "main"]).await;
    std::fs::write(path.join("base-only.txt"), "base-only change\n").unwrap();
    git(path, &["add", "base-only.txt"]).await;
    git(path, &["commit", "-m", "Advance the base separately"]).await;
    let base_tip = git(path, &["rev-parse", "HEAD"]).await;
    assert!(
        with_http_token(
            path,
            token,
            &["push", "--atomic", &remote, "main", "feature"]
        )
        .await
    );

    let changes = h
        .json(
            &format!("{REPO}/changes?base=refs/heads/main&rev=refs/heads/feature&merge_base=true"),
            &alice,
        )
        .await;
    assert_eq!(changes["base"], ancestor);
    assert_eq!(changes["base_tip"], base_tip);
    assert_eq!(changes["commit"], head);
    let rows = changes["changes"].as_array().unwrap();
    assert_eq!(rows.len(), 516);
    assert!(!rows.iter().any(|row| row["path"] == "base-only.txt"));

    let repo = h.server.repositories.values().pop().unwrap();
    let cancellation = &h.server.cancellation;
    let options = RepositoryOptions::new(
        ObjectLimits::default(),
        OperationLimits {
            max_entries: 80,
            ..OperationLimits::default()
        },
    )
    .unwrap();
    let repository = repo
        .open_current(&h.server, options, cancellation)
        .await
        .unwrap();
    let operation = repository
        .operation(OperationKind::Compare, cancellation)
        .await
        .unwrap();
    let full = async {
        let base = repository
            .snapshot(&Revision::parse(&ancestor)?, &operation)
            .await?;
        let target = repository
            .snapshot(&Revision::parse(&head)?, &operation)
            .await?;
        target.compare(&base, &operation).await
    }
    .await;
    assert!(matches!(
        operation.finish(full).await,
        Err(Error::LimitExceeded {
            limit: "entries",
            ..
        })
    ));
    for (name, old, new) in [
        ("README.md", true, true),
        ("tool.sh", true, true),
        ("replacement", false, true),
        ("deleted.txt", true, false),
        ("unchanged.txt", true, true),
        ("unrelated", false, false),
    ] {
        let file = crate::api::comparison_blobs(
            &repository,
            &ancestor,
            &head,
            &GitPath::new(name).unwrap(),
            cancellation,
        )
        .await
        .unwrap();
        assert_eq!(
            file.changed,
            rows.iter().any(|row| row["path"] == name),
            "{name}"
        );
        assert_eq!(
            (file.old.is_some(), file.new.is_some()),
            (old, new),
            "{name}"
        );
        if name == "tool.sh" {
            let old = file.old.unwrap();
            let new = file.new.unwrap();
            assert_eq!(old.metadata.oid, new.metadata.oid);
            assert_ne!(old.metadata.mode, new.metadata.mode);
        }
    }

    h.write(
        Method::POST,
        &format!("{REPO}/pulls"),
        &alice,
        json!({
            "request_id":Uuid::now_v7(), "title":"Large comparison", "body":"",
            "base_ref":"refs/heads/main", "head_ref":"refs/heads/feature"
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let readme = rows.iter().find(|row| row["path"] == "README.md").unwrap();
    let diff = h
        .json(
            &format!(
                "{REPO}/diff?base={ancestor}&rev={head}&path_hex={}",
                readme["path_hex"].as_str().unwrap()
            ),
            &bob,
        )
        .await;
    assert_eq!(diff["old"]["text"], "base text\n");
    assert_eq!(diff["new"]["text"], "base  text\nnew review line\n");
    let review_path = format!("{REPO}/pulls/1/threads");
    h.write(
        Method::POST,
        &review_path,
        &bob,
        json!({
            "request_id":Uuid::now_v7(), "body":"Review the new line",
            "base_oid":ancestor, "head_oid":head, "path_hex":readme["path_hex"],
            "side":"new", "start_line":2, "end_line":2
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    let thread = h.json(&format!("{review_path}/1"), &alice).await;
    assert_eq!(thread["base_oid"], ancestor);
    assert_eq!(thread["head_oid"], head);
    assert_eq!(thread["new_blob_oid"], readme["new"]["oid"]);
    assert_eq!(thread["current"], true);

    let browser = Browser::new("large-pr-comparison");
    browser
        .run(&["open", &format!("{}/team/private/pull/1/files", h.origin)])
        .await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "516 files"]).await;
    browser
        .run(&["wait", "[data-change-path='README.md'] diffs-container"])
        .await;
    browser.run(&["wait", "[data-thread-number='1']"]).await;
    browser
        .run(&["fill", "input[aria-label='Filter changed files']", "readme"])
        .await;
    browser
        .run(&["wait", "--text", "1 of 516 files shown"])
        .await;
    browser
        .run(&["wait", "--fn", "document.querySelectorAll('.diff-panel').length === 1 && document.querySelector('.diff-panel').dataset.changePath === 'README.md'"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "checkbox",
            "click",
            "--name",
            "Hide whitespace",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--fn", "(() => { const diff = document.querySelector('.diff-panel diffs-container')?.shadowRoot; return diff?.querySelector('[data-column-number=\"2\"][data-line-type=\"change-addition\"]') && !diff.querySelector('[data-column-number=\"1\"][data-line-type=\"change-addition\"]'); })()"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "checkbox",
            "click",
            "--name",
            "Viewed README.md",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "1 of 516 files viewed"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "checkbox",
            "click",
            "--name",
            "Hide viewed",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "No changed files match these filters."])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Jump to diff",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--fn", "document.activeElement?.matches('[data-thread-number=\"1\"]') && document.querySelector('input[aria-label=\"Filter changed files\"]').value === 'readme'"])
        .await;

    let deleted = rows
        .iter()
        .find(|row| row["path"] == "deleted.txt")
        .unwrap();
    for number in 2..=34 {
        let (path, side) = if number == 2 {
            (&deleted["path_hex"], "old")
        } else {
            (&readme["path_hex"], "new")
        };
        let created: Value = h
            .write(
                Method::POST,
                &review_path,
                &bob,
                json!({
                    "request_id":Uuid::now_v7(), "body":format!("Review point {number}"),
                    "base_oid":ancestor, "head_oid":head, "path_hex":path,
                    "side":side, "start_line":1, "end_line":1,
                }),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(created["number"], number);
        if number > 2 {
            h.write(
                Method::PATCH,
                &format!("{review_path}/{number}"),
                &alice,
                json!({"version":created["version"], "resolved":true}),
            )
            .await
            .error_for_status()
            .unwrap();
        }
    }
    let page = h.json(&format!("{review_path}?outdated=false"), &bob).await;
    let page_items = page["items"].as_array().unwrap();
    assert_eq!(page_items.len(), 30);
    assert!(page_items.iter().all(|thread| thread["resolved"] == true));
    let navigation = format!("{review_path}/navigation");
    let summary = h.json(&navigation, &bob).await;
    assert_eq!(summary["base_oid"], ancestor);
    assert_eq!(summary["head_oid"], head);
    assert_eq!(
        summary["summary"],
        json!({"open":2,"resolved":32,"outdated":0,"outdated_unresolved":0,"previous":null,"next":2})
    );
    let resolved_anchor = h.json(&format!("{navigation}?around=3"), &bob).await;
    assert_eq!(resolved_anchor["summary"]["previous"], Value::Null);
    assert_eq!(resolved_anchor["summary"]["next"], 2);

    browser.run(&["reload"]).await;
    browser
        .run(&["wait", "--text", "2 open · 32 resolved · 0 outdated"])
        .await;
    browser.run(&["set", "viewport", "390", "844"]).await;
    browser
        .run(&[
            "wait", "--fn",
            "(() => { const nav = document.querySelector('nav[aria-label=\"Review conversation navigation\"]'); return nav && nav.scrollWidth <= nav.clientWidth && nav.querySelectorAll('button').length === 2; })()",
        ])
        .await;
    browser.run(&["set", "viewport", "1280", "900"]).await;
    browser
        .run(&["fill", "input[aria-label='Filter changed files']", "readme"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "checkbox",
            "click",
            "--name",
            "Hide viewed",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "No changed files match these filters."])
        .await;
    for (button, number) in [
        ("Next unresolved", 2),
        ("Next unresolved", 1),
        ("Previous unresolved", 2),
    ] {
        browser
            .run(&[
                "find", "role", "button", "click", "--name", button, "--exact",
            ])
            .await;
        browser
            .run(&[
                "wait",
                "--fn",
                &format!("document.activeElement?.matches('[data-thread-number=\"{number}\"]')"),
            ])
            .await;
    }
    browser
        .run(&["wait", "--url", "**/pull/1/files#review-diff-2"])
        .await;

    git(path, &["switch", "feature"]).await;
    std::fs::write(path.join("README.md"), "base  text\nnewer review line\n").unwrap();
    git(path, &["commit", "-am", "Advance the comparison"]).await;
    assert!(with_http_token(path, token, &["push", &remote, "feature"]).await);
    let advanced = h.json(&format!("{navigation}?around=2"), &bob).await;
    assert_ne!(advanced["head_oid"], head);
    assert_eq!(
        advanced["summary"],
        json!({"open":0,"resolved":0,"outdated":34,"outdated_unresolved":2,"previous":null,"next":1})
    );
    browser.run(&["reload"]).await;
    browser
        .run(&["wait", "--text", "0 open · 0 resolved · 34 outdated"])
        .await;
    browser
        .run(&[
            "wait", "--fn",
            "document.activeElement?.id === 'review-thread-2' && document.activeElement.closest('.inline-review-outdated[data-state=\"open\"]') !== null",
        ])
        .await;
    drop(browser);
    commit_suggestions_across_pages(&h, path, &remote, token, &bob).await;
    h.close().await;
}

async fn commit_suggestions_across_pages(
    h: &Harness,
    source: &std::path::Path,
    remote: &str,
    token: &str,
    reviewer: &str,
) {
    let comparison = h
        .json(
            &format!("{REPO}/changes?base=refs/heads/main&rev=refs/heads/feature&merge_base=true"),
            reviewer,
        )
        .await;
    let head = comparison["commit"].as_str().unwrap();
    let rows = comparison["changes"].as_array().unwrap();
    assert_eq!(rows.len(), 516);
    let threads = format!("{REPO}/pulls/1/threads");
    let readme = "README.md";
    let other = "unrelated/part-00/0.txt";
    for number in 35..=66 {
        let (path, line, replacement) = match number {
            35 => (readme, 2, Some("Reviewed change")),
            36 => (other, 1, Some("Reviewed second file")),
            _ => (readme, 2, None),
        };
        let row = rows.iter().find(|row| row["path"] == path).unwrap();
        let created: Value = h
            .write(
                Method::POST,
                &threads,
                reviewer,
                json!({
                    "request_id":Uuid::now_v7(), "body":format!("Follow-up {number}"),
                    "base_oid":comparison["base"], "head_oid":head, "path_hex":row["path_hex"],
                    "side":"new", "start_line":line, "end_line":line, "suggested_text":replacement,
                }),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(created["number"], number);
    }
    let page = h.json(&format!("{threads}?outdated=false"), reviewer).await;
    assert!(page["items"].as_array().unwrap().iter().all(|thread| {
        thread["number"].as_u64().unwrap() > 36 && thread["suggested_text"].is_null()
    }));

    // The reviewer writes the suggestions. A different browser account
    // applies the batch, which exercises the real co-author trailer path.
    h.provider.mode.lock().await.clear();
    let browser = Browser::new("pr-batch");
    browser
        .run(&[
            "open",
            &format!("{}/team/private/pull/1/files#review-diff-36", h.origin),
        ])
        .await;
    browser.sign_in().await;
    browser
        .run(&["wait", "--url", "**/pull/1/files#review-diff-36"])
        .await;
    click(&browser, "checkbox", "Select suggestion from thread #36").await;
    click(&browser, "button", "Next unresolved").await;
    click(&browser, "checkbox", "Select suggestion from thread #35").await;
    browser
        .run(&[
            "wait",
            "--fn",
            "document.getElementById('review-thread-36') === null",
        ])
        .await;
    click(&browser, "button", "Commit suggestions (2)").await;
    browser
        .run(&["wait", "--text", "Commit 2 suggestions"])
        .await;
    browser
        .run(&[
            "wait",
            "--fn",
            "(() => { const list = document.querySelector('ul[aria-label=\"Selected suggestions\"]'); return list?.children.length === 2 && list.textContent.includes('README.md') && list.textContent.includes('unrelated/part-00/0.txt'); })()",
        ])
        .await;
    h.write(
        Method::PATCH,
        &format!("{threads}/35"),
        reviewer,
        json!({"version":1,"body":"Refined suggestion after selection"}),
    )
    .await
    .error_for_status()
    .unwrap();
    click(&browser, "button", "Commit changes").await;
    browser
        .run(&["wait", "--text", "A selected suggestion changed."])
        .await;
    assert!(with_http_token(source, token, &["fetch", remote, "feature"]).await);
    assert_eq!(git(source, &["rev-parse", "FETCH_HEAD"]).await, head);
    click(&browser, "button", "Remove suggestion from thread #35").await;
    click(&browser, "button", "Cancel").await;
    browser
        .run(&["wait", "--fn", "!document.querySelector('[role=dialog]')"])
        .await;
    browser
        .run(&["wait", "--text", "Commit suggestions (1)"])
        .await;
    click(&browser, "button", "Refresh").await;
    browser
        .run(&["wait", "--text", "Refined suggestion after selection"])
        .await;
    click(&browser, "checkbox", "Select suggestion from thread #35").await;
    click(&browser, "button", "Commit suggestions (2)").await;
    let message = "Apply the cross-file review batch";
    browser
        .run(&["fill", "[role='dialog'] input[maxlength='256']", message])
        .await;
    h.drop_next_suggestion_reply
        .store(true, std::sync::atomic::Ordering::SeqCst);
    click(&browser, "button", "Commit changes").await;
    browser
        .run(&["wait", "--text", "Retry suggestion commit"])
        .await;
    assert!(
        !h.drop_next_suggestion_reply
            .load(std::sync::atomic::Ordering::SeqCst)
    );

    assert!(with_http_token(source, token, &["fetch", remote, "feature"]).await);
    let commit = git(source, &["rev-parse", "FETCH_HEAD"]).await;
    assert_eq!(git(source, &["rev-parse", "FETCH_HEAD^"]).await, head);
    assert_eq!(
        git(
            source,
            &[
                "diff-tree",
                "--no-commit-id",
                "--name-only",
                "-r",
                "FETCH_HEAD"
            ],
        )
        .await,
        format!("{readme}\n{other}")
    );
    assert_eq!(
        git(source, &["show", "FETCH_HEAD:README.md"]).await,
        "base  text\nReviewed change"
    );
    assert_eq!(
        git(source, &["show", "FETCH_HEAD:unrelated/part-00/0.txt"]).await,
        "Reviewed second file"
    );
    let body = git(source, &["log", "-1", "--format=%B", "FETCH_HEAD"]).await;
    assert!(body.starts_with(message));
    let coauthors: Vec<_> = body
        .lines()
        .filter(|line| line.starts_with("Co-authored-by: "))
        .collect();
    assert_eq!(coauthors.len(), 1);
    assert!(coauthors[0].starts_with("Co-authored-by: Bob <"));
    assert_eq!(
        git(source, &["log", "-1", "--format=%cn", "FETCH_HEAD"]).await,
        "Alice"
    );

    click(&browser, "button", "Back to review").await;
    browser
        .run(&["wait", "--fn", "!document.querySelector('[role=dialog]')"])
        .await;
    git(source, &["switch", "--detach", &commit]).await;
    std::fs::write(source.join("after-review.txt"), "later work\n").unwrap();
    git(source, &["add", "after-review.txt"]).await;
    git(source, &["commit", "-m", "Add later work"]).await;
    let later = git(source, &["rev-parse", "HEAD"]).await;
    assert!(with_http_token(source, token, &["push", remote, "HEAD:refs/heads/feature"]).await);
    browser.run(&["reload"]).await;
    browser.run(&["wait", "--text", "517 changed files"]).await;
    click(&browser, "button", "Resume suggestion commit").await;
    click(&browser, "button", "Retry suggestion commit").await;
    browser
        .run(&["wait", "--text", "Committed suggestions in"])
        .await;
    browser
        .run(&["wait", &format!("a[href*='/commit/{commit}']")])
        .await;
    assert!(with_http_token(source, token, &["fetch", remote, "feature"]).await);
    assert_eq!(git(source, &["rev-parse", "FETCH_HEAD"]).await, later);
    assert_eq!(git(source, &["rev-parse", "FETCH_HEAD^"]).await, commit);
    drop(browser);
}

async fn click(browser: &Browser, role: &str, name: &str) {
    let selector = if role == "checkbox" {
        "[role=checkbox]"
    } else {
        "button"
    };
    let ready = format!(
        "Array.from(document.querySelectorAll('{selector}')).some(el => (el.getAttribute('aria-label') ?? el.textContent.trim()) === {} && el.getClientRects().length && !el.disabled)",
        json!(name),
    );
    browser.run(&["wait", "--fn", &ready]).await;
    browser
        .run(&["find", "role", role, "click", "--name", name, "--exact"])
        .await;
}
