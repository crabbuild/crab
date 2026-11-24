use reqwest::Method;
use uuid::Uuid;

use super::*;
use crate::{
    catalog::{MembershipActor, OrganizationAccess},
    test_support::http::{browser::Browser, organizations::bootstrap},
};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cell fixtures"]
async fn organization_owner_grants_and_revokes_a_team_members_repository_access_in_the_browser() {
    let h = Harness::new(false).await;
    let (organization, _, member) = bootstrap(&h).await;
    let repo = h.server.repositories.values()[0].clone();
    let catalog = h.server.catalog().unwrap();
    catalog
        .set_members(
            "team",
            "private",
            vec![crate::RepositoryMember {
                subject: "other-admin".into(),
                name: "Other administrator".into(),
                access: crate::RepositoryAccess::Admin,
            }],
            true,
        )
        .await
        .unwrap();
    let revision = catalog
        .membership("team", "private")
        .await
        .unwrap()
        .revision;
    catalog
        .set_organization_access(
            "team",
            "private",
            repo.id,
            revision,
            OrganizationAccess {
                id: organization,
                teams: vec![],
            },
            MembershipActor {
                issuer: h.provider.issuer.clone(),
                subject: "test-operator".into(),
            },
        )
        .await
        .unwrap();

    catalog.mark_cell_ready(repo.id).await.unwrap();
    let (document, _) = catalog.load().await.unwrap();
    h.server.install_catalog(document).await.unwrap();
    let alice = h.login().await;
    let org = format!("/api/organizations/{organization}");
    let offer: Value = h
        .write(
            Method::PUT,
            &format!("{org}/invitations/{member}"),
            &alice,
            json!({"version":0,"role":"member"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let owner = Browser::new("grant-owner");
    let repository = format!("{}/team/private", h.origin);
    sign_in(&owner, &repository).await;
    owner
        .run(&["wait", "--text", "This repository is empty"])
        .await;
    click(&owner, "link", "Settings").await;
    owner.run(&["wait", "--text", "Team access"]).await;
    click(&owner, "link", "Team access").await;
    owner
        .run(&[
            "wait",
            "--text",
            "No teams have access through a team grant.",
        ])
        .await;

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    h.write(
        Method::POST,
        &format!("{org}/invitation"),
        &bob,
        json!({"version":offer["version"],"accept":true}),
    )
    .await
    .error_for_status()
    .unwrap();
    let team = Uuid::now_v7();
    let commands = format!("{org}/teams/commands");
    for body in [
        json!({"request_id":team,"command":{"kind":"create","name":"Release engineers","description":"","visibility":"secret"}}),
        json!({"request_id":Uuid::now_v7(),"command":{"kind":"member","team":team,"member":member,"version":0,"role":"member"}}),
    ] {
        h.write(Method::POST, &commands, &alice, body)
            .await
            .error_for_status()
            .unwrap();
    }

    let teammate = Browser::new("grant-teammate");
    sign_in(&teammate, &repository).await;
    teammate
        .run(&["wait", "--text", "Repository not found"])
        .await;

    click(&owner, "button", "Add team").await;
    owner.run(&["wait", "--text", "Find a team"]).await;
    owner
        .run(&["find", "label", "Find a team", "fill", "Release"])
        .await;
    click(&owner, "button", "Search").await;
    owner.run(&["wait", "--text", "Release engineers"]).await;
    click(&owner, "button", "Add Release engineers").await;
    owner
        .run(&["wait", "--fn", "!document.querySelector('[role=dialog]')"])
        .await;
    let role = "[aria-label='Role for Release engineers']";
    owner.run(&["wait", role]).await;
    owner.run(&["select", role, "admin"]).await;
    click(&owner, "button", "Save team access").await;
    owner.run(&["wait", "--text", "Team access saved."]).await;
    teammate.run(&["open", &repository]).await;
    teammate
        .run(&["wait", "--text", "This repository is empty"])
        .await;
    click(&teammate, "link", "Settings").await;
    teammate.run(&["wait", "--text", "Members"]).await;
    click(&teammate, "link", "Members").await;
    teammate
        .run(&["wait", "--text", "Other administrator"])
        .await;

    owner.run(&["reload"]).await;
    owner
        .run(&[
            "wait",
            "--fn",
            "document.querySelector('[aria-label=\"Role for Release engineers\"]')?.value === 'admin'",
        ])
        .await;
    owner.run(&["select", role, "write"]).await;
    click(&owner, "button", "Save team access").await;
    owner.run(&["wait", "--text", "Team access saved."]).await;
    teammate.run(&["open", &repository]).await;
    teammate
        .run(&["wait", "--text", "This repository is empty"])
        .await;
    assert!(
        !teammate
            .run(&["snapshot"])
            .await
            .contains("link \"Settings\"")
    );

    click(&owner, "button", "Remove Release engineers").await;
    owner
        .run(&[
            "wait",
            "--text",
            "No teams have access through a team grant.",
        ])
        .await;
    click(&owner, "button", "Save team access").await;
    owner.run(&["wait", "--text", "Team access saved."]).await;
    teammate.run(&["open", &repository]).await;
    teammate
        .run(&["wait", "--text", "Repository not found"])
        .await;

    owner.run(&["reload"]).await;
    owner
        .run(&[
            "wait",
            "--text",
            "No teams have access through a team grant.",
        ])
        .await;
    drop(teammate);
    drop(owner);
    h.close().await;
}

async fn click(browser: &Browser, role: &str, name: &str) {
    browser
        .run(&["find", "role", role, "click", "--name", name, "--exact"])
        .await;
}

async fn sign_in(browser: &Browser, url: &str) {
    browser.run(&["open", url]).await;
    browser.sign_in().await;
}
