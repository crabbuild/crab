use std::path::Path;

use crate::test_support::{
    git,
    http::{Harness, browser::Browser},
};

use super::branch_restore::remote;

const ENDPOINT: &str = "/api/repos/team/private/pulls/templates";
const DEFAULT: &str = "## Review checklist\n<!-- Replace these notes -->\n";
const RELEASE: &str = "## Release checklist\n";

async fn publish(h: &Harness, writer: &str, path: &Path) {
    let url = remote(h, writer).await;
    let git = async |args: &[&str]| git::success(path, args).await;
    git(&["init", "--initial-branch=main", "--object-format=sha1", "."]).await;
    for directory in [
        ".github/PULL_REQUEST_TEMPLATE",
        "docs/PULL_REQUEST_TEMPLATE",
        "PULL_REQUEST_TEMPLATE",
    ] {
        std::fs::create_dir_all(path.join(directory)).unwrap();
    }
    for (file, body) in [
        (".github/pull_request_template.md", DEFAULT),
        ("pull_request_template.md", "Lower precedence default"),
        (
            "docs/PULL_REQUEST_TEMPLATE/release.md",
            "Lower precedence release template",
        ),
        (".github/PULL_REQUEST_TEMPLATE/release.md", RELEASE),
        ("PULL_REQUEST_TEMPLATE/fix.md", "## Fix checklist\n"),
        (
            "docs/PULL_REQUEST_TEMPLATE/docs.md",
            "## Documentation checklist\n",
        ),
        ("PULL_REQUEST_TEMPLATE/binary.md", "prefix\0suffix"),
        ("PULL_REQUEST_TEMPLATE/config.yml", "ignore"),
    ] {
        std::fs::write(path.join(file), body).unwrap();
    }
    std::fs::write(
        path.join("PULL_REQUEST_TEMPLATE/large.md"),
        vec![b'x'; 32 * 1024 + 1],
    )
    .unwrap();
    git(&["add", "."]).await;
    git(&["commit", "-m", "Publish pull request templates"]).await;
    git(&["push", url.as_str(), "main", "main:refs/heads/stable"]).await;
    git(&["switch", "-c", "feature"]).await;
    std::fs::write(
        path.join(".github/pull_request_template.md"),
        "# Feature-only replacement",
    )
    .unwrap();
    std::fs::write(
        path.join(".github/PULL_REQUEST_TEMPLATE/feature.md"),
        "# Feature-only template",
    )
    .unwrap();
    git(&["add", "."]).await;
    git(&["commit", "-m", "Draft updated templates"]).await;
    git(&["push", url.as_str(), "feature"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_templates_resolve_only_safe_markdown_on_the_default_branch() {
    let h = Harness::new(false).await;
    let writer = h.login().await;
    assert!(h.json(ENDPOINT, &writer).await["default"].is_null());
    let checkout = tempfile::tempdir().unwrap();
    publish(&h, &writer, checkout.path()).await;
    let catalog = h.json(ENDPOINT, &writer).await;
    assert_eq!(catalog["default"]["body"], DEFAULT);
    assert_eq!(
        catalog["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["file"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["docs.md", "fix.md", "release.md"]
    );
    assert_eq!(catalog["items"][2]["body"], RELEASE);
    assert_eq!(
        catalog["items"][2]["path"],
        ".github/PULL_REQUEST_TEMPLATE/release.md"
    );
    assert_eq!(catalog["truncated"], false);
    assert!(catalog["source"].is_null());
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn native_browser_uses_pull_templates_and_preserves_the_edited_description() {
    let h = Harness::new(false).await;
    let writer = h.login().await;
    let checkout = tempfile::tempdir().unwrap();
    publish(&h, &writer, checkout.path()).await;
    let browser = Browser::new("pull-template");
    let compare = format!("{}/team/private/compare/main...feature", h.origin);
    browser.run(&["open", &compare]).await;
    browser.sign_in().await;
    let wait_for_body = async |text: &str| {
        let text = serde_json::to_string(text).unwrap();
        browser
            .run(&[
                "wait",
                "--fn",
                &format!(
                    "document.querySelector('#discussion-description')?.value.includes({text})"
                ),
            ])
            .await;
    };
    wait_for_body(DEFAULT).await;
    browser
        .run(&[
            "open",
            &format!("{compare}?template=release.md&body=Explicit+description"),
        ])
        .await;
    wait_for_body("Explicit description").await;
    browser
        .run(&[
            "open",
            &format!("{compare}?template=release.md&title=Prepare+release"),
        ])
        .await;
    wait_for_body(RELEASE).await;
    browser
        .run(&["select", "#pull-template", "PULL_REQUEST_TEMPLATE/fix.md"])
        .await;
    wait_for_body(RELEASE).await;
    let click = async |name| {
        browser
            .run(&["find", "role", "button", "click", "--name", name, "--exact"])
            .await;
    };
    click("Replace description").await;
    wait_for_body("## Fix checklist").await;
    let edited = "## Fix checklist\n\nReviewed the rollout.";
    browser
        .run(&["fill", "#discussion-description", edited])
        .await;
    browser
        .run(&["select", "#pull-base", "refs/heads/stable"])
        .await;
    wait_for_body(edited).await;
    browser.run(&["wait", "--fn", "Array.from(document.querySelectorAll('button')).some(button => button.textContent.trim() === 'Create pull request' && !button.disabled)"]).await;
    click("Create pull request").await;
    browser.run(&["wait", "--url", "**/pull/1"]).await;
    let pull = h.json("/api/repos/team/private/pulls/1", &writer).await;
    assert_eq!(pull["title"], "Prepare release");
    assert_eq!(pull["body"], edited);
    assert_eq!(pull["base_ref"], "refs/heads/stable");
    drop(browser);
    h.close().await;
}
