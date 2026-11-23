use super::*;

use crate::test_support::http::{Harness, TEST_SCIM_TOKEN, organizations};
use reqwest::Method;

async fn scim(h: &Harness, method: Method, path: &str, value: Value) -> reqwest::Response {
    h.http
        .request(method, format!("{}{ROOT}{path}", h.origin))
        .bearer_auth(TEST_SCIM_TOKEN)
        .header(header::CONTENT_TYPE, "application/scim+json")
        .json(&value)
        .send()
        .await
        .unwrap()
}

async fn user(h: &Harness, subject: &str) -> Uuid {
    let response: Value = scim(
        h,
        Method::POST,
        "/Users",
        json!({
            "schemas":[USER],"externalId":subject,"userName":format!("{subject}@example.test"),
        }),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    Uuid::parse_str(response["id"].as_str().unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn synced_roster_revokes_existing_git_credentials_and_recovers_interrupted_projection() {
    let h = Harness::scim().await;
    let alice_id = user(&h, "alice-id").await;
    let bob_id = user(&h, "bob-id").await;
    let outsider_id = user(&h, "outsider-id").await;
    let (org, _, bob_actor) = organizations::bootstrap(&h).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let base = format!("/api/organizations/{org}");
    h.write(
        Method::PUT,
        &format!("{base}/invitations/{bob_actor}"),
        &alice,
        json!({"role":"member","version":0}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{base}/invitation"),
        &bob,
        json!({"accept":true,"version":1}),
    )
    .await
    .error_for_status()
    .unwrap();

    let linked = scim(
        &h,
        Method::POST,
        "/Groups",
        json!({
            "schemas":[GROUP],"displayName":"Release engineers","externalId":"release-engineers",
            "members":[{"value":bob_id},{"value":outsider_id}],
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    let etag = linked.headers()[header::ETAG].clone();
    let value: Value = linked.json().await.unwrap();
    let group = Uuid::parse_str(value["id"].as_str().unwrap()).unwrap();
    let resource = format!("/Groups/{group}");
    let found: Value = h
        .http
        .get(format!("{}{ROOT}/Groups", h.origin))
        .bearer_auth(TEST_SCIM_TOKEN)
        .query(&[("filter", "displayName eq \"RELEASE ENGINEERS\"")])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found["Resources"][0]["id"], json!(group));

    let commands = format!("{base}/teams/commands");
    let team = Uuid::now_v7();
    h.write(Method::POST, &commands, &alice,
        json!({"request_id":team,"command":{"kind":"create","name":"Release","description":"","visibility":"closed"}}))
        .await.error_for_status().unwrap();
    let auxiliary = Uuid::now_v7();
    h.write(Method::POST, &commands, &alice,
        json!({"request_id":auxiliary,"command":{"kind":"create","name":"Review","description":"","visibility":"closed"}}))
        .await.error_for_status().unwrap();

    let binding = format!("{base}/teams/{team}/identity-group");
    let status = h.json(&binding, &alice).await;
    assert_eq!(status["enabled"], true);
    assert_eq!(
        h.write(
            Method::PUT,
            &binding,
            &bob,
            json!({"group":group,"version":status["version"]})
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    for target in [team, auxiliary] {
        let path = format!("{base}/teams/{target}/identity-group");
        let selected = h.json(&path, &alice).await;
        h.write(
            Method::PUT,
            &path,
            &alice,
            json!({"group":group,"version":selected["version"]}),
        )
        .await
        .error_for_status()
        .unwrap();
        let roster = h
            .json(&format!("{base}/teams/{target}/members"), &alice)
            .await;
        assert_eq!(roster["can_manage"], false);
        assert_eq!(roster["items"].as_array().unwrap().len(), 1);
        assert_eq!(roster["items"][0]["actor"], json!(bob_actor));
    }
    assert_eq!(
        h.write(Method::POST, &commands, &alice, json!({
            "request_id":Uuid::now_v7(),"command":{"kind":"member","team":team,"member":bob_actor,"version":1,"role":"maintainer"},
        })).await.status(),
        StatusCode::CONFLICT
    );

    let repository: Value = h
        .write(
            Method::POST,
            &format!("{base}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"release-library","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let owner = repository["owner"].as_str().unwrap();
    let acl = format!("/api/repos/{owner}/release-library/team-access");
    let grants = h.json(&acl, &alice).await;
    let session = h.json("/api/session", &alice).await;
    h.http.put(format!("{}{acl}",h.origin))
        .header(header::COOKIE,&alice).header(header::ORIGIN,&h.origin)
        .header("x-csrf-token",session["csrf"].as_str().unwrap())
        .header("x-crab-repository-id", repository["id"].as_str().unwrap())
        .json(&json!({"expected_revision":grants["revision"],"teams":[{"team":team,"access":"read"}]}))
        .send().await.unwrap().error_for_status().unwrap();

    let issued: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &bob,
            json!({"owner":owner,"repository":"release-library","access":"read"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = issued["token"].as_str().unwrap();
    let git_access = async || {
        h.http
            .get(format!(
                "{}/git/{owner}/release-library.git/info/refs?service=git-upload-pack",
                h.origin
            ))
            .header("git-protocol", "version=2")
            .basic_auth("crab", Some(token))
            .send()
            .await
            .unwrap()
            .status()
    };
    assert_eq!(git_access().await, StatusCode::OK);

    let patch = |operation: Value| json!({"schemas":[PATCH],"Operations":[operation]});
    let removed = scim(
        &h,
        Method::PATCH,
        &resource,
        patch(json!({"op":"remove","path":format!("members[value eq \"{bob_id}\"]")})),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_ne!(removed.headers()[header::ETAG], etag);
    let invalidated = git_access().await;
    assert!(matches!(
        invalidated,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
    ));
    assert_eq!(h.json("/api/session", &bob).await["authenticated"], true);

    scim(
        &h,
        Method::PATCH,
        &resource,
        patch(json!({"op":"add","path":"members","value":[{"value":bob_id}]})),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(git_access().await, StatusCode::OK);
    let directory = Groups::new(&h.server).unwrap();
    let (mut pending, cas) = directory.load().await.unwrap();
    let entry = pending.groups.get_mut(&group).unwrap();
    entry.members.remove(&bob_id);
    entry.members.insert(alice_id);
    entry.revision += 1;
    pending.pending = Some(Pending { group, edit: None });
    assert!(directory.save(&pending, cas).await.unwrap());
    assert_eq!(git_access().await, StatusCode::OK);
    scim(&h, Method::GET, &resource, Value::Null)
        .await
        .error_for_status()
        .unwrap();
    assert!(directory.load().await.unwrap().0.pending.is_none());
    assert!(!git_access().await.is_success());
    for target in [team, auxiliary] {
        let roster = h
            .json(&format!("{base}/teams/{target}/members"), &alice)
            .await;
        assert_eq!(roster["items"].as_array().unwrap().len(), 1);
        assert_ne!(roster["items"][0]["actor"], json!(bob_actor));
    }
    scim(&h, Method::DELETE, &resource, Value::Null)
        .await
        .error_for_status()
        .unwrap();
    let empty = h
        .json(&format!("{base}/teams/{team}/members"), &alice)
        .await;
    assert_eq!(empty["items"], json!([]));
    let status = h.json(&binding, &alice).await;
    h.write(
        Method::PUT,
        &binding,
        &alice,
        json!({"group":null,"version":status["version"]}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(&binding, &alice).await["group"], Value::Null);
    h.close().await;
}
