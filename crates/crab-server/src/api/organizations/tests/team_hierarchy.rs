use super::{repositories::repository_write, *};
use crate::cells::organization::teams::{MentionMembers, MentionMembersInput, MentionRoster, Page};

#[tokio::test(flavor = "multi_thread")]
async fn nested_teams_inherit_access_and_drop_it_when_moved() {
    let storage = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, storage, "team-hierarchy").await;
    h.initialize_directory().await;
    let (organization, alice_id, bob_id) =
        crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let root = format!("/api/organizations/{organization}");
    let teams = format!("{root}/teams");
    let commands = format!("{teams}/commands");
    h.write(
        Method::PUT,
        &format!("{root}/invitations/{bob_id}"),
        &alice,
        json!({"version":0,"role":"member"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{root}/invitation"),
        &bob,
        json!({"version":1,"accept":true}),
    )
    .await
    .error_for_status()
    .unwrap();

    let create =
        async |name: &str, parent: Option<Uuid>| {
            let id = Uuid::now_v7();
            h.write(Method::POST, &commands, &alice, json!({"request_id":id,"command":{
            "kind":"create","name":name,"description":"","visibility":"closed","parent":parent,
        }})).await.error_for_status().unwrap();
            id
        };
    let parent = create("Engineering", None).await;
    let subteam = create("Infrastructure", Some(parent)).await;
    let child = create("Networking", Some(subteam)).await;
    let other = create("Support", None).await;
    let command = async |cookie: &str, command: Value| {
        h.write(
            Method::POST,
            &commands,
            cookie,
            json!({"request_id":Uuid::now_v7(),"command":command}),
        )
        .await
    };
    command(
        &alice,
        json!({"kind":"member","team":child,"member":bob_id,"version":0,"role":"maintainer"}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&format!("{teams}?parent={parent}"), &bob).await["items"][0]["team"]["id"],
        json!(subteam)
    );
    let people = h.json(&format!("{teams}/{parent}/members"), &bob).await;
    assert!(
        people["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|person| person["actor"] == json!(bob_id)
                && person["role"] == "member"
                && person["inherited"] == true)
    );
    let filtered = h
        .json(&format!("{root}/members?team={parent}&role=member"), &alice)
        .await;
    assert!(
        filtered["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|person| person["actor"] == json!(bob_id))
    );
    let mention_recipients = async || {
        let result = h
            .server
            .cells()
            .unwrap()
            .query::<MentionMembers>(
                Address::Organization(organization),
                None,
                MentionMembersInput {
                    actor: alice_id,
                    team: parent,
                    roster: MentionRoster::Candidates(vec![bob_id]),
                },
            )
            .await
            .unwrap()
            .output;
        let Page::Page { items, .. } = result else {
            panic!("Mention roster changed");
        };
        items
    };
    assert_eq!(mention_recipients().await, vec![bob_id]);

    let repository: Value = h
        .write(
            Method::POST,
            &format!("{root}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"network","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = Uuid::parse_str(repository["id"].as_str().unwrap()).unwrap();
    let owner = repository["owner"].as_str().unwrap();
    let access = format!("/api/repos/{owner}/network/team-access");
    let version = h.json(&access, &alice).await["revision"].clone();
    repository_write(
        &h,
        Method::PUT,
        &access,
        &alice,
        id,
        json!({"expected_revision":version,"teams":[{"team":parent,"access":"write"}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let detail = format!("/api/repos?owner={owner}&name=network");
    assert_eq!(
        h.json(&detail, &bob).await["repositories"][0]["access"],
        "write"
    );
    let listing = format!("{teams}/{child}/repositories");
    let listed = h.json(&listing, &bob).await;
    assert_eq!(listed["repositories"][0]["id"], json!(id));
    assert_eq!(listed["repositories"][0]["team_access_inherited"], true);

    let parent_view = h.json(&format!("{teams}/{parent}"), &alice).await;
    let parent_version = &parent_view["team"]["version"];
    assert_eq!(
        command(
            &alice,
            json!({"kind":"reparent","team":parent,"parent":child,"version":parent_version})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(command(&alice, json!({"kind":"edit","team":parent,"version":parent_version,"name":"Engineering","description":"","visibility":"secret"})).await.status(), StatusCode::BAD_REQUEST);

    command(
        &alice,
        json!({"kind":"member","team":subteam,"member":bob_id,"version":0,"role":"maintainer"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let subteam_view = h.json(&format!("{teams}/{subteam}"), &bob).await;
    let version = &subteam_view["team"]["version"];
    command(
        &bob,
        json!({"kind":"propose_hierarchy","team":subteam,"version":version,"parent":other,"side":"child"}),
    )
    .await.error_for_status().unwrap();
    let requests = format!("{teams}/hierarchy-requests");
    let pending = h.json(&requests, &bob).await;
    assert_eq!(pending["items"][0]["can_withdraw"], true);
    assert_eq!(pending["items"][0]["can_approve"], false);
    let request = &pending["items"][0]["id"];
    let approve =
        json!({"kind":"decide_hierarchy","team":subteam,"request":request,"decision":"approve"});
    assert_eq!(
        command(&bob, approve.clone()).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.json(&detail, &bob).await["repositories"][0]["access"],
        "write"
    );
    assert_eq!(
        h.json(&requests, &alice).await["items"][0]["can_approve"],
        true
    );
    command(&alice, approve).await.error_for_status().unwrap();
    assert_eq!(h.json(&requests, &alice).await["items"], json!([]));
    assert_eq!(h.json(&detail, &bob).await["repositories"], json!([]));
    assert_eq!(h.json(&listing, &bob).await["repositories"], json!([]));
    assert!(mention_recipients().await.is_empty());

    let current = h.json(&format!("{teams}/{other}"), &alice).await;
    let version = &current["team"]["version"];
    assert_eq!(
        command(
            &alice,
            json!({"kind":"delete","team":other,"version":version})
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    command(&alice, json!({"kind":"delete","team":other,"version":version,"hierarchy_version":current["tree_version"]}))
        .await.error_for_status().unwrap();
    assert_eq!(
        h.json(&format!("{teams}?search=Networking"), &alice).await["items"],
        json!([])
    );
    h.close().await;
}
