use super::*;
use crate::cells::{
    directory::{IdentityKey, RegisterAccounts},
    organization::{ChangeOutcome, DecideInvitation, Decision, Invite, InviteInput, Role},
};
use crate::test_support::http::Harness;
use reqwest::Method;

#[tokio::test]
async fn organization_projects_require_csrf_and_do_not_grant_source_access() {
    let h = Harness::new(false).await;
    let (id, _, _) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    let root = format!("/api/organizations/{id}/projects");
    let project = Uuid::now_v7();
    let input = json!({"request_id":Uuid::now_v7(),"command":{"kind":"create","id":project,"name":"Cross-repository release","template":"board"}});
    let saved = h
        .write(
            Method::POST,
            &format!("{root}/commands"),
            &alice,
            input.clone(),
        )
        .await;
    assert!(
        saved.status().is_success(),
        "{}",
        saved.text().await.unwrap()
    );
    assert_eq!(
        h.json(&format!("{root}/{project}"), &alice).await["repositories"],
        json!([])
    );
    assert_eq!(
        h.http
            .post(format!("{}{root}/commands", h.origin))
            .header(axum::http::header::COOKIE, &alice)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        axum::http::StatusCode::FORBIDDEN
    );
    let rejected=h.write(Method::POST,&format!("{root}/commands"),&alice,json!({"request_id":Uuid::now_v7(),"command":{"kind":"repository","project":project,"version":1,"repository":Uuid::now_v7(),"linked":true}})).await.json::<Value>().await.unwrap();
    assert_eq!(rejected["outcome"], "rejected");
    h.close().await;
}

#[tokio::test]
async fn shared_work_is_live_for_repository_readers_and_redacted_for_other_organization_members() {
    let h = Harness::new(true).await;
    let (organization, owner, _) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    let repository = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap()
        .id;
    let root = format!("/api/organizations/{organization}/projects");
    let project = Uuid::now_v7();
    for command in [
        json!({"kind":"create","id":project,"name":"Shared release","template":"board"}),
        json!({"kind":"repository","project":project,"version":1,"repository":repository,"linked":true}),
    ] {
        let response = h
            .write(
                Method::POST,
                &format!("{root}/commands"),
                &alice,
                json!({"request_id":Uuid::now_v7(),"command":command}),
            )
            .await;
        let response = response
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(response["outcome"], "applied", "{response}");
    }
    h.write(
        Method::POST,
        "/api/repos/team/private/issues",
        &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Confidential source title","body":"Private source body"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let candidates = h
        .json(
            &format!("{root}/{project}/candidates?repository={repository}"),
            &alice,
        )
        .await;
    assert_eq!(candidates["items"][0]["title"], "Confidential source title");
    let command = json!({"kind":"item","project":project,"version":2,"repository":repository,"item":{"id":Uuid::now_v7(),"subject":{"kind":"issue","number":1},"title":"","body":"","status":"Todo","priority":0,"start":null,"end":null,"values":{}}});
    let saved = h
        .write(
            Method::POST,
            &format!("{root}/commands"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"command":command}),
        )
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(saved["outcome"], "applied", "{saved}");
    let visible = h.json(&format!("{root}/{project}/items"), &alice).await;
    assert_eq!(
        visible["page"]["items"][0]["work"]["title"],
        "Confidential source title"
    );
    let cells = h.server.cells().unwrap();
    let guest = app::command_output(
        cells
            .command::<RegisterAccounts>(
                Address::Directory,
                app::mutation_identity().unwrap(),
                vec![IdentityKey {
                    issuer: h.provider.issuer.clone(),
                    subject: "outsider-id".into(),
                }],
            )
            .await,
    )
    .unwrap()[0];
    let offer = app::command_output(
        cells
            .command::<Invite>(
                Address::Organization(organization),
                app::mutation_identity().unwrap(),
                InviteInput {
                    actor: owner,
                    recipient: guest,
                    version: 0,
                    role: Role::Member,
                },
            )
            .await,
    )
    .unwrap();
    let ChangeOutcome::Applied(offer) = offer else {
        panic!("invitation failed");
    };
    cells
        .command::<DecideInvitation>(
            Address::Organization(organization),
            app::mutation_identity().unwrap(),
            Decision {
                actor: guest,
                version: offer.version,
                accept: true,
            },
        )
        .await
        .unwrap();
    *h.provider.mode.lock().await = "outsider".into();
    let guest_session = h.login().await;
    let hidden = h
        .json(&format!("{root}/{project}/items"), &guest_session)
        .await;
    let row = &hidden["page"]["items"][0];
    assert_eq!(
        (
            row["restricted"].clone(),
            row["repository"].clone(),
            row["work"].clone(),
            row["item"]["subject"].clone()
        ),
        (json!(true), Value::Null, Value::Null, Value::Null)
    );
    for private in [
        "Confidential source title",
        "Private source body",
        "team/private",
        "alice-id",
    ] {
        assert!(!hidden.to_string().contains(private));
    }
    let detail = h.json(&format!("{root}/{project}"), &guest_session).await;
    assert_eq!(detail["repositories"], json!([]));
    h.close().await;
}
