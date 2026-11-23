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
    browser
        .run(&["wait", "--text", "Continue to sign in"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Continue to sign in",
            "--exact",
        ])
        .await;
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
    drop(browser);
    h.close().await;
}
