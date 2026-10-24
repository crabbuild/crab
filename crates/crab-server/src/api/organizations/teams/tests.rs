use super::*;
use crate::test_support::http::{Harness, organizations::bootstrap};
use reqwest::{Method, Response};
async fn join(h: &Harness, id: Uuid, alice: &str, bob: &str, person: Uuid, version: u64) {
    let offer: Value = h
        .write(
            Method::PUT,
            &format!("/api/organizations/{id}/invitations/{person}"),
            alice,
            json!({"version":version,"role":"member"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    h.write(
        Method::POST,
        &format!("/api/organizations/{id}/invitation"),
        bob,
        json!({"version":offer["version"],"accept":true}),
    )
    .await
    .error_for_status()
    .unwrap();
}
async fn login(h: &Harness) -> (String, String) {
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    (alice, bob)
}
async fn change(h: &Harness, id: Uuid, cookie: &str, command: Value) -> Response {
    h.write(
        Method::POST,
        &format!("/api/organizations/{id}/teams/commands"),
        cookie,
        json!({"request_id":Uuid::now_v7(),"command":command}),
    )
    .await
}
#[tokio::test]
async fn public_team_routes_preserve_secret_visibility_and_current_maintainer_authority() {
    let h = Harness::new(false).await;
    let (id, _, bob_id) = bootstrap(&h).await;
    let (alice, bob) = login(&h).await;
    join(&h, id, &alice, &bob, bob_id, 0).await;
    let root = format!("/api/organizations/{id}/teams");
    let team = Uuid::now_v7();
    let response=h.write(Method::POST,&format!("{root}/commands"),&alice,json!({"request_id":team,"command":{"kind":"create","name":"Security","description":"Confidential group","visibility":"secret"}})).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"outcome":"applied","id":team})
    );
    let path = format!("{root}/{team}");
    assert_eq!(
        h.http
            .get(format!("{}{path}", h.origin))
            .header("cookie", &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let page = h.json(&root, &bob).await;
    assert_eq!(page["items"], json!([]));
    assert!(!page.to_string().contains("Confidential"));
    let request=h.http.post(format!("{}{root}/commands",h.origin)).header("cookie",&bob).json(&json!({"request_id":Uuid::now_v7(),"command":{"kind":"create","name":"No CSRF","description":"","visibility":"closed"}})).send().await.unwrap();
    assert_eq!(request.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        h.json(&root, &alice).await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    change(
        &h,
        id,
        &alice,
        json!({"kind":"member","team":team,"member":bob_id,"version":0,"role":"member"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let view = h.json(&path, &bob).await;
    assert_eq!(
        (view["role"].clone(), view["can_manage"].clone()),
        (json!("member"), json!(false))
    );
    assert_eq!(change(&h,id,&bob,json!({"kind":"edit","team":team,"version":view["team"]["version"],"name":"Rejected","description":"","visibility":"closed"})).await.status(),StatusCode::FORBIDDEN);
    change(
        &h,
        id,
        &alice,
        json!({"kind":"member","team":team,"member":bob_id,"version":1,"role":"maintainer"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let view = h.json(&path, &bob).await;
    change(&h,id,&bob,json!({"kind":"edit","team":team,"version":view["team"]["version"],"name":"Security reviewers","description":"Updated by a maintainer","visibility":"closed"})).await.error_for_status().unwrap();
    let people = h.json(&format!("{path}/members"), &bob).await;
    assert_eq!(people["items"].as_array().unwrap().len(), 2);
    assert!(
        people["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|person| person["profile"].is_null()
                && person.get("issuer").is_none()
                && person.get("subject").is_none())
    );
    let denied=change(&h,id,&alice,json!({"kind":"edit","team":team,"version":1,"name":"Stale","description":"","visibility":"closed"})).await;
    assert_eq!(denied.status(), StatusCode::CONFLICT);
    assert_eq!(denied.headers()["x-crab-write-outcome"], "rejected");
    h.close().await;
}
#[tokio::test]
async fn minimal_acknowledgements_recover_after_org_removal_without_restoring_team_membership() {
    let h = Harness::new(false).await;
    let (id, _, bob_id) = bootstrap(&h).await;
    let (alice, bob) = login(&h).await;
    join(&h, id, &alice, &bob, bob_id, 0).await;
    let root = format!("/api/organizations/{id}/teams");
    let team = Uuid::now_v7();
    let input = json!({"request_id":team,"command":{"kind":"create","name":"Private work","description":"Owned draft","visibility":"secret"}});
    h.write(
        Method::POST,
        &format!("{root}/commands"),
        &bob,
        input.clone(),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::DELETE,
        &format!("/api/organizations/{id}/members/{bob_id}"),
        &alice,
        json!({"version":1}),
    )
    .await
    .error_for_status()
    .unwrap();
    let replay = h
        .write(
            Method::POST,
            &format!("{root}/commands"),
            &bob,
            input.clone(),
        )
        .await;
    assert_eq!(replay.status(), StatusCode::CREATED);
    assert_eq!(
        replay.json::<Value>().await.unwrap(),
        json!({"outcome":"applied","id":team})
    );
    let recovered = h
        .write(Method::POST, &format!("{root}/cancel"), &bob, input)
        .await;
    assert_eq!(
        recovered.json::<Value>().await.unwrap(),
        json!({"outcome":"applied","id":team})
    );
    assert_eq!(
        h.http
            .get(format!("{}{root}/{team}", h.origin))
            .header("cookie", &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    join(&h, id, &alice, &bob, bob_id, 2).await;
    assert_eq!(
        h.http
            .get(format!("{}{root}/{team}", h.origin))
            .header("cookie", &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let state = h
        .json(&format!("{root}/{team}/members/{bob_id}"), &alice)
        .await;
    assert_eq!(
        (state["role"].clone(), state["version"].clone()),
        (Value::Null, json!(1))
    );
    change(&h,id,&alice,json!({"kind":"member","team":team,"member":bob_id,"version":state["version"],"role":"member"})).await.error_for_status().unwrap();
    assert_eq!(
        h.json(&format!("{root}/{team}"), &bob).await["can_manage"],
        false
    );
    h.json("/api/repos?owner=team&name=private", &bob).await;
    h.close().await;
}
#[tokio::test]
async fn cancellation_fences_late_requests_and_role_denials_do_not_trap_an_unknown_submission() {
    let h = Harness::new(false).await;
    let (id, _, bob_id) = bootstrap(&h).await;
    let (alice, bob) = login(&h).await;
    join(&h, id, &alice, &bob, bob_id, 0).await;
    let root = format!("/api/organizations/{id}/teams");
    change(
        &h,
        id,
        &alice,
        json!({"kind":"policy","version":1,"member_creation":false}),
    )
    .await
    .error_for_status()
    .unwrap();
    let team = Uuid::now_v7();
    let input = json!({"request_id":team,"command":{"kind":"create","name":"Cancelled draft","description":"","visibility":"secret"}});
    assert_eq!(
        h.write(
            Method::POST,
            &format!("{root}/commands"),
            &bob,
            input.clone()
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let cancelled = h
        .write(Method::POST, &format!("{root}/cancel"), &bob, input.clone())
        .await;
    assert_eq!(
        cancelled.json::<Value>().await.unwrap(),
        json!({"outcome":"cancelled","id":team})
    );
    change(
        &h,
        id,
        &alice,
        json!({"kind":"policy","version":2,"member_creation":true}),
    )
    .await
    .error_for_status()
    .unwrap();
    let late = h
        .write(Method::POST, &format!("{root}/commands"), &bob, input)
        .await;
    assert_eq!(
        late.json::<Value>().await.unwrap(),
        json!({"outcome":"cancelled","id":team})
    );
    assert_eq!(h.json(&root, &alice).await["items"], json!([]));
    h.close().await;
}
#[tokio::test]
async fn policy_and_page_cursors_are_organization_scoped_and_creation_is_not_repo_administration() {
    let h = Harness::new(false).await;
    let (id, _, bob_id) = bootstrap(&h).await;
    let (alice, bob) = login(&h).await;
    join(&h, id, &alice, &bob, bob_id, 0).await;
    let root = format!("/api/organizations/{id}/teams");
    change(
        &h,
        id,
        &alice,
        json!({"kind":"policy","version":1,"member_creation":false}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        change(
            &h,
            id,
            &bob,
            json!({"kind":"create","name":"Denied","description":"","visibility":"closed"})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    for name in ["Alpha", "Zulu"] {
        change(
            &h,
            id,
            &alice,
            json!({"kind":"create","name":name,"description":"","visibility":"closed"}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let first = h.json(&format!("{root}?limit=1"), &bob).await;
    let cursor = first["next"].as_str().unwrap();
    let second = h
        .json(&format!("{root}?limit=1&after={cursor}"), &bob)
        .await;
    assert_eq!(second["items"][0]["team"]["name"], "Zulu");
    let changed = h
        .http
        .get(format!("{}{root}", h.origin))
        .header("cookie", &bob)
        .query(&[("search", "Alpha"), ("after", cursor)])
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), StatusCode::CONFLICT);
    let team = first["items"][0]["team"]["id"].as_str().unwrap();
    let next = h
        .json(&format!("{root}/{team}/candidates?limit=1"), &alice)
        .await;
    let cursor = next["next"].as_str().unwrap();
    assert_eq!(
        h.http
            .get(format!("{}{root}/{team}/members?after={cursor}", h.origin))
            .header("cookie", &alice)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    h.close().await;
}

#[tokio::test]
async fn invalid_team_fields_and_page_parameters_return_actionable_rejections() {
    let h = Harness::new(false).await;
    let (id, _, _) = bootstrap(&h).await;
    let alice = h.login().await;
    let root = format!("/api/organizations/{id}/teams");
    for name in ["", " surrounding whitespace "] {
        let response = change(
            &h,
            id,
            &alice,
            json!({"kind":"create","name":name,"description":"","visibility":"secret"}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["x-crab-write-outcome"], "rejected");
        assert!(
            response.json::<Value>().await.unwrap()["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("Team name")
        );
    }
    for params in [[("limit", "0")], [("limit", "51")], [("search", "\n")]] {
        let response = h
            .http
            .get(format!("{}{root}", h.origin))
            .header("cookie", &alice)
            .query(&params)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let cancelled = Uuid::now_v7();
    let body = json!({"request_id":cancelled,"command":{"kind":"create","name":"","description":"","visibility":"secret"}});
    let acknowledgement = h
        .write(
            Method::POST,
            &format!("{root}/cancel"),
            &alice,
            body.clone(),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(
        h.write(Method::POST, &format!("{root}/commands"), &alice, body)
            .await
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
        acknowledgement
    );
    assert!(
        h.json(&root, &alice).await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    h.close().await;
}
