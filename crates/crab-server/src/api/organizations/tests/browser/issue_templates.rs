use super::super::repositories::repository_write;
use super::{Browser, Harness, StatusCode, Uuid, Value, json};
use crate::test_support::git::success as git;
use reqwest::Method;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cells"]
async fn native_browser_creates_an_issue_from_the_default_branch_form() {
    let h = Harness::new(false).await;
    let admin = h.login().await;
    let root = "/api/repos/team/private";
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &admin,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let path = checkout.path();
    git(
        path,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    let templates = path.join(".github/ISSUE_TEMPLATE");
    std::fs::create_dir_all(&templates).unwrap();
    std::fs::write(templates.join("feature.md"), "---\nname: Request a feature\nabout: Describe an improvement\ntitle: '[Feature] '\n---\n\n## Proposal\n").unwrap();
    std::fs::write(templates.join("config.yml"), "blank_issues_enabled: false\ncontact_links:\n  - name: Support\n    url: https://support.example.test/\n    about: Ask how to use the project\n").unwrap();
    std::fs::write(
        templates.join("bug.yml"),
        r#"name: Report a bug
description: Help us reproduce a defect
title: "[Bug] "
labels: [bug, unknown]
body:
  - type: markdown
    attributes:
      value: Check the troubleshooting guide first.
  - type: textarea
    id: steps
    attributes:
      label: Steps to reproduce
      render: text
    validations:
      required: true
  - type: dropdown
    attributes:
      label: Repeatability
      options: [Always, Sometimes]
      default: 0
  - type: checkboxes
    attributes:
      label: Confirmation
      options:
        - label: I checked the guide
          required: true
"#,
    )
    .unwrap();
    git(path, &["add", ".github"]).await;
    git(path, &["commit", "-m", "Publish issue templates"]).await;
    git(path, &["push", remote.as_str(), "main"]).await;
    git(path, &["checkout", "-b", "feature"]).await;
    std::fs::write(
        templates.join("private-branch.md"),
        "---\nname: Only on the feature branch\n---\n## Draft\n",
    )
    .unwrap();
    git(path, &["add", ".github"]).await;
    git(path, &["commit", "-m", "Draft a template on feature"]).await;
    git(path, &["push", remote.as_str(), "feature"]).await;
    for (name, color) in [("bug", "c0442b"), ("staff", "2a5dc3")] {
        h.write(
            Method::POST,
            &format!("{root}/labels"),
            &admin,
            json!({"request_id":Uuid::now_v7(),"name":name,"color":color}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let endpoint = format!("{root}/issues/templates");
    assert_eq!(
        h.json(&endpoint, &admin).await["blank_maintainers_only"],
        true
    );
    assert_eq!(h.json(&endpoint, &admin).await["blank_allowed"], true);
    *h.provider.mode.lock().await = "member".into();
    let reader = h.login().await;
    let data = h.json(&endpoint, &reader).await;
    assert_eq!(data["blank_allowed"], false);
    assert_eq!(data["items"].as_array().unwrap().len(), 2);
    assert_eq!(data["items"][0]["name"], "Report a bug");
    assert_eq!(data["items"][1]["content"]["body"], "## Proposal\n");
    let forged = h.write(Method::POST, &format!("{root}/issues"), &reader,
        json!({"request_id":Uuid::now_v7(),"title":"Rejected metadata","body":"","metadata":{"label_ids":[2]},"template_grant":data["items"][0]["grant"]})).await;
    assert_eq!(forged.status(), StatusCode::FORBIDDEN);

    let browser = Browser::new("issue-templates");
    browser
        .run(&["open", &format!("{}/team/private/issues/new", h.origin)])
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
    browser
        .run(&["wait", "--text", "Choose how to open an issue"])
        .await;
    assert!(
        !browser
            .run(&["snapshot"])
            .await
            .contains("Open blank issue")
    );
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Use Report a bug",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Check the troubleshooting guide first"])
        .await;
    browser
        .run(&[
            "find",
            "label",
            "Title",
            "fill",
            "[Bug] The operation stalls",
        ])
        .await;
    browser
        .run(&[
            "find",
            "label",
            "Steps to reproduce",
            "fill",
            "Run the operation twice.",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "checkbox",
            "check",
            "--name",
            "I checked the guide Required",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Create issue",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--url", "**/issues/1"]).await;
    let issue = h.json(&format!("{root}/issues/1"), &reader).await;
    assert_eq!(issue["title"], "[Bug] The operation stalls");
    assert_eq!(issue["labels"][0]["name"], "bug");
    let body = issue["body"].as_str().unwrap();
    assert!(body.contains("```text\nRun the operation twice.\n```"));
    assert!(body.contains("### Repeatability\n\nAlways"));
    assert!(body.contains("- [x] I checked the guide"));
    assert!(!body.contains("Check the troubleshooting guide first"));
    drop(browser);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cells"]
async fn native_browser_inherits_issue_templates_and_rechecks_source_access() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "community-defaults").await;
    h.initialize_directory().await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, _) = crate::test_support::http::organizations::bootstrap(&h).await;
    let admin = h.login().await;
    let create = async |name: &str| -> Value {
        h.write(
            Method::POST,
            &format!("/api/organizations/{organization}/repositories/creation"),
            &admin,
            json!({"request_id":Uuid::now_v7(),"name":name,"description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
    };
    let defaults = create(".github").await;
    let application = create("app").await;
    let owner = application["owner"].as_str().unwrap();
    let defaults_id = Uuid::parse_str(defaults["id"].as_str().unwrap()).unwrap();
    let application_id = Uuid::parse_str(application["id"].as_str().unwrap()).unwrap();
    let source = format!("/api/repos/{owner}/.github");
    let target = format!("/api/repos/{owner}/app");
    let publish = async |name: &str, files: &[(&str, &str)]| {
        let token: Value = h
            .write(
                Method::POST,
                "/api/git-token",
                &admin,
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
        let checkout = tempfile::tempdir().unwrap();
        let directory = checkout.path().join(".github/ISSUE_TEMPLATE");
        std::fs::create_dir_all(&directory).unwrap();
        for (file, content) in files {
            std::fs::write(directory.join(file), content).unwrap();
        }
        git(
            checkout.path(),
            &["init", "--initial-branch=main", "--object-format=sha1", "."],
        )
        .await;
        git(checkout.path(), &["add", ".github"]).await;
        git(checkout.path(), &["commit", "-m", "Publish issue settings"]).await;
        git(checkout.path(), &["push", remote.as_str(), "main"]).await;
    };
    publish(".github", &[
        ("report.md", "---\nname: Shared issue report\nabout: Describe a problem\nlabels: bug\n---\n## Details\n"),
        ("config.yml", "blank_issues_enabled: false\n"),
    ]).await;
    // The inherited name must resolve to a label in the destination.
    for name in ["internal", "bug"] {
        repository_write(
            &h,
            Method::POST,
            &format!("{target}/labels"),
            &admin,
            application_id,
            json!({"request_id":Uuid::now_v7(),"name":name,"color":"c0442b"}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let access = async |repo: &str, id, visible| {
        let path = format!("{repo}/members");
        let acl = h.json(&path, &admin).await;
        let members = if visible {
            json!([{"subject":"bob-id","name":"Bob","access":"read"}])
        } else {
            json!([])
        };
        repository_write(
            &h,
            Method::PUT,
            &path,
            &admin,
            id,
            json!({"expected_revision":acl["revision"],"members":members,"groups":[]}),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    access(&target, application_id, true).await;
    *h.provider.mode.lock().await = "member".into();
    let reader = h.login().await;
    let templates = format!("{target}/issues/templates");
    let private = h.json(&templates, &reader).await;
    assert_eq!(private["items"], json!([]));
    assert_eq!(private["source"], Value::Null);
    access(&source, defaults_id, true).await;
    let inherited = h.json(&templates, &reader).await;
    assert_eq!(inherited["source"], json!({"owner":owner,"name":".github"}));
    assert_eq!(inherited["blank_allowed"], false);
    assert_eq!(inherited["items"][0]["labels"][0]["id"], 2);

    let browser = Browser::new("inherited-issues");
    browser
        .run(&["open", &format!("{}/{owner}/app/issues/new", h.origin)])
        .await;
    let click = async |role: &str, name: &str| {
        browser
            .run(&["find", "role", role, "click", "--name", name, "--exact"])
            .await;
    };
    browser
        .run(&["wait", "--text", "Continue to sign in"])
        .await;
    click("button", "Continue to sign in").await;
    browser.run(&["wait", "--text", "Templates from"]).await;
    click("link", "Use Shared issue report").await;
    browser
        .run(&["wait", "--text", "Shared issue report"])
        .await;
    browser
        .run(&["find", "label", "Title", "fill", "Inherited report"])
        .await;
    click("button", "Create issue").await;
    browser.run(&["wait", "--url", "**/app/issues/1"]).await;
    assert_eq!(
        h.json(&format!("{target}/issues/1"), &reader).await["labels"][0]["name"],
        "bug"
    );
    drop(browser);

    let issues = format!("{target}/issues");
    let mut request = json!({"request_id":Uuid::now_v7(),"title":"Retry after revocation","body":"",
        "metadata":{"label_ids":[2]},"template_grant":inherited["items"][0]["grant"]});
    let create_issue = async |request| -> reqwest::Response {
        repository_write(&h, Method::POST, &issues, &reader, application_id, request).await
    };
    assert_eq!(
        create_issue(request.clone()).await.status(),
        StatusCode::CREATED
    );
    access(&source, defaults_id, false).await;
    let revoked = h.json(&templates, &reader).await;
    assert_eq!(revoked["source"], Value::Null);
    assert_eq!(revoked["items"], json!([]));
    let receipt: Value = create_issue(request.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(receipt["number"], 2);
    request["request_id"] = json!(Uuid::now_v7());
    assert_eq!(create_issue(request).await.status(), StatusCode::FORBIDDEN);
    access(&source, defaults_id, true).await;
    publish("app", &[("config.yml", "{}\n")]).await;
    let local = h.json(&templates, &reader).await;
    assert_eq!(local["source"], Value::Null);
    assert_eq!(local["items"], json!([]));
    assert_eq!(local["blank_allowed"], true);
    h.close().await;
}
