use std::collections::HashSet;

use super::*;
use crate::test_support::http::{Harness, organizations::bootstrap};
use reqwest::Method;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn incoming_team_requests_resume_across_organizations_and_hide_after_revocation() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "team-requests-inbox").await;
    h.initialize_directory().await;
    let (first, _, bob_id) = bootstrap(&h).await;
    let (second, _, _) = bootstrap(&h).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;

    let mut submitted = HashSet::new();
    for (organization, count) in [(first, 21), (second, 1)] {
        let root = format!("/api/organizations/{organization}");
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
        let commands = format!("{root}/teams/commands");
        let create = async |cookie: &str, name: String| {
            let id = Uuid::now_v7();
            h.write(
                Method::POST,
                &commands,
                cookie,
                json!({"request_id":id,"command":{
                    "kind":"create","name":name,"description":"","visibility":"closed",
                }}),
            )
            .await
            .error_for_status()
            .unwrap();
            id
        };
        let parent = create(&bob, "Receiving team".into()).await;
        for number in 0..count {
            let child = create(&alice, format!("Child {number}")).await;
            let id = Uuid::now_v7();
            h.write(Method::POST, &commands, &alice, json!({"request_id":id,"command":{
                "kind":"propose_hierarchy","team":child,"parent":parent,"version":1,"side":"child",
            }})).await.error_for_status().unwrap();
            submitted.insert(id.to_string());
        }
    }

    let path = "/api/notifications/team-requests";
    assert_eq!(h.json(path, &alice).await["items"], json!([]));
    let initial = h.json(path, &bob).await;
    let next = initial["next"].as_str().unwrap();
    let later = h.json(&format!("{path}?after={next}"), &bob).await;
    assert_eq!(later["next"], serde_json::Value::Null);
    assert_eq!(initial["items"].as_array().unwrap().len(), 20);
    assert_eq!(later["items"].as_array().unwrap().len(), 2);
    let mut received = HashSet::new();
    for item in initial["items"]
        .as_array()
        .unwrap()
        .iter()
        .chain(later["items"].as_array().unwrap())
    {
        assert_eq!(item["organization_name"], "Crab engineering");
        assert_eq!(item["request"]["can_approve"], true);
        assert!(received.insert(item["request"]["id"].as_str().unwrap().to_owned()));
    }
    assert_eq!(received, submitted);
    let item = &initial["items"][0];
    let org = item["organization"].as_str().unwrap();
    let request = item["request"]["id"].as_str().unwrap();
    let focused = h
        .json(
            &format!("/api/organizations/{org}/teams/hierarchy-requests?request={request}"),
            &bob,
        )
        .await;
    assert_eq!(focused["items"], json!([item["request"].clone()]));

    h.write(
        Method::DELETE,
        &format!("/api/organizations/{first}/members/{bob_id}"),
        &alice,
        json!({"version":1}),
    )
    .await
    .error_for_status()
    .unwrap();
    let visible = h.json(path, &bob).await;
    assert_eq!(visible["items"].as_array().unwrap().len(), 1);
    assert_eq!(visible["items"][0]["organization"], json!(second));
    assert_eq!(visible["next"], serde_json::Value::Null);
    h.close().await;
}
