use super::*;
use crate::cells::{
    Address,
    organization::projects as workspace,
    repository::{RepositoryAuthor, issues::issue_type, projects::organization_issue as source},
};

fn create(project: Uuid, repo: Uuid, title: &str, type_id: Uuid) -> Value {
    json!({
        "request_id":Uuid::now_v7(),
        "command":{
            "kind":"create_issue","project":project,"repository":repo,"type_id":type_id,
            "item":{
                "id":Uuid::now_v7(),"subject":null,"title":title,"body":"Track release work",
                "status":"Todo","priority":1,"start":null,"end":null,"values":{}
            }
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn organization_project_recovers_the_issue_after_the_repository_commits() {
    let (h, alice, org, repo, key) = fixture().await;
    let root = format!("{org}/projects");
    let project = project_with_issue(&h, &alice, &root, repo, true, false).await;
    let organization = org.rsplit('/').next().unwrap().parse().unwrap();
    let actor = h.json(&org, &alice).await["actor"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let definitions = h.json(&format!("{org}/issue-types"), &alice).await;
    let type_id = definitions["definitions"][2]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let epoch = h.server.repositories.by_id(repo).unwrap().ownership_epoch;
    let input = create(project, repo, "Release from shared planning", type_id);
    let request_id = input["request_id"].as_str().unwrap().parse().unwrap();
    let cells = h.server.cells().unwrap();
    let begin = async |body: &Value| {
        cells
            .command::<workspace::Change>(
                Address::Organization(organization),
                app::mutation_identity().unwrap(),
                workspace::Request {
                    actor,
                    request_id: body["request_id"].as_str().unwrap().parse().unwrap(),
                    command: serde_json::from_value(body["command"].clone()).unwrap(),
                    source_allowed: true,
                    source_epoch: Some(epoch),
                    cancel: false,
                },
            )
            .await
            .unwrap()
            .output
    };
    let send = async |action: &str, body: Value| {
        h.write(Method::POST, &format!("{root}/{action}"), &alice, body)
            .await
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    };
    assert_eq!(
        begin(&input).await,
        workspace::Outcome::PendingIssue {
            ownership_epoch: epoch
        }
    );
    let repo_cell = cells.repository(repo).unwrap();
    let committed = repo_cell
        .client
        .command::<source::Publish>(
            &repo_cell.target,
            app::mutation_identity().unwrap(),
            source::Request {
                organization,
                project,
                request_id,
                actor: RepositoryAuthor {
                    issuer: h.provider.issuer.clone(),
                    subject: "alice-id".into(),
                    name: "Alice".into(),
                },
                title: "Release from shared planning".into(),
                body: "Track release work".into(),
                type_id: Some(type_id),
                ownership_epoch: epoch,
                issue_type: Some(issue_type::Selection {
                    id: type_id,
                    organization,
                    ownership_epoch: epoch,
                }),
                can_write: true,
                cancel: false,
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(committed, source::Outcome::Created { number: 2 });
    let items = format!("{root}/{project}/items");
    assert_eq!(h.json(&items, &alice).await["page"]["total"], 1);
    let recovered = send("cancel", input.clone()).await;
    assert_eq!(recovered["outcome"], "applied");
    assert_eq!(send("commands", input.clone()).await, recovered);
    let groups = h
        .json(&format!("{items}?group_by=issue_type"), &alice)
        .await;
    assert_eq!(
        groups["page"]["group_counts"],
        json!({(type_id.to_string()):1,"none":1})
    );
    let issue = h.json(&format!("/api/repos/{key}/issues/2"), &alice).await;
    assert_eq!(issue["issue_type"]["id"], type_id.to_string());

    let cancelled = create(project, repo, "Cancelled before publishing", type_id);
    assert!(matches!(
        begin(&cancelled).await,
        workspace::Outcome::PendingIssue { .. }
    ));
    assert_eq!(
        send("cancel", cancelled.clone()).await["outcome"],
        "cancelled"
    );
    assert_eq!(send("commands", cancelled).await["outcome"], "cancelled");

    let forged = create(project, Uuid::now_v7(), "Not a linked repository", type_id);
    assert_eq!(send("commands", forged).await["outcome"], "rejected");
    let unavailable = create(project, repo, "Unavailable issue type", Uuid::now_v7());
    assert_eq!(send("commands", unavailable).await["outcome"], "rejected");
    let mut attachment = create(project, repo, "Unavailable attachment", type_id);
    attachment["command"]["item"]["body"] = json!(format!(
        "[Missing attachment](/api/discussion-attachments/{repo}/{})",
        Uuid::now_v7()
    ));
    let rejection = send("commands", attachment.clone()).await;
    assert_eq!(rejection["outcome"], "rejected");
    assert_eq!(send("cancel", attachment.clone()).await, rejection);
    assert_eq!(h.json(&items, &alice).await["page"]["total"], 2);

    // A corrected submission can reuse the released Project item ID.
    let mut success = create(project, repo, "Create from shared planning", type_id);
    success["command"]["item"]["id"] = attachment["command"]["item"]["id"].clone();
    assert_eq!(send("commands", success).await["outcome"], "applied");
    assert_eq!(
        h.json(&format!("/api/repos/{key}/issues/3"), &alice).await["title"],
        "Create from shared planning"
    );
    assert_eq!(h.json(&items, &alice).await["page"]["total"], 3);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and built embedded UI; uses real OIDC/native Cells"]
async fn native_browser_creates_an_issue_in_an_organization_project_group() {
    let (h, alice, org, repo, key) = fixture().await;
    let project =
        project_with_issue(&h, &alice, &format!("{org}/projects"), repo, true, false).await;
    let organization = org.rsplit('/').next().unwrap();
    let browser = Browser::new("shared-project-issue");
    let url = format!(
        "{}/-/organizations/{organization}/projects/{project}?group_by=issue_type",
        h.origin
    );
    browser.run(&["open", &url]).await;
    browser.sign_in().await;
    browser
        .run(&["wait", ".project-table tbody[aria-label='Feature']"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Create issue in Feature",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "[role=dialog] input[id$='-title']"])
        .await;
    browser
        .run(&[
            "find",
            "label",
            "Title",
            "fill",
            "Create in the linked repository",
            "--exact",
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
    browser.run(&["wait", "--fn", "document.querySelector('.project-table tbody[aria-label=\"Feature\"]')?.textContent.includes('Create in the linked repository')"]).await;
    assert_eq!(
        h.json(&format!("/api/repos/{key}/issues/2"), &alice).await["issue_type"]["name"],
        "Feature"
    );
}
