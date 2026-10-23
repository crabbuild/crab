use crate::test_support::http::Harness;
use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};

const ROOT: &str = "/api/repos/team/private";

async fn replace(h: &Harness, admin: &str, groups: Value) -> reqwest::Response {
    let current = h.json(&format!("{ROOT}/members"), admin).await;
    h.write(
        Method::PUT,
        &format!("{ROOT}/members"),
        admin,
        json!({
            "expected_revision":current["revision"], "members":current["members"], "groups":groups,
        }),
    )
    .await
}

#[tokio::test]
async fn signed_group_access_and_revocation_apply_to_browser_git_and_lfs_without_routing_refresh() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    replace(
        &h,
        &alice,
        json!([{"group":"/Engineering","access":"write"}]),
    )
    .await
    .error_for_status()
    .unwrap();
    *h.provider.mode.lock().await = "outsider".into();
    h.provider
        .claims
        .lock()
        .await
        .insert("groups".into(), json!(["/Engineering"]));
    let member = h.login().await;
    assert_eq!(
        h.json("/api/repos", &member).await["repositories"][0]["access"],
        "write"
    );
    assert!(
        h.json("/api/session", &member).await["user"]
            .get("groups")
            .is_none()
    );
    let created = h.write(Method::POST, &format!("{ROOT}/issues"), &member,
        json!({"request_id":uuid::Uuid::now_v7(),"title":"Created through SSO access","body":"Native write"}))
        .await.error_for_status().unwrap().json::<Value>().await.unwrap();
    assert_eq!(
        h.json(&format!("{ROOT}/issues/1"), &member).await["title"],
        created["title"]
    );
    let issued = h
        .write(
            Method::POST,
            "/api/git-token",
            &member,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let token = issued["token"].as_str().unwrap();
    replace(
        &h,
        &alice,
        json!([{"group":"/Engineering","access":"read"}]),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.server.repositories.values()[0].config.groups.len(), 0);
    for (method, path, body) in [
        (Method::GET, "info/refs?service=git-receive-pack", None),
        (
            Method::POST,
            "info/lfs/objects/batch",
            Some(json!({"operation":"upload","objects":[]})),
        ),
    ] {
        let mut request = h
            .http
            .request(method, format!("{}/git/team/private.git/{path}", h.origin))
            .basic_auth("crab", Some(token));
        if let Some(body) = body {
            request = request.json(&body);
        }
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    replace(&h, &alice, json!([]))
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        h.json("/api/repos", &member).await["repositories"],
        json!([])
    );
    for path in [format!("{ROOT}/issues/1"), format!("{ROOT}/members")] {
        assert_eq!(
            h.http
                .get(format!("{}{path}", h.origin))
                .header(header::COOKIE, &member)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        h.http
            .get(format!(
                "{}/git/team/private.git/info/refs?service=git-upload-pack",
                h.origin
            ))
            .basic_auth("crab", Some(token))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}

#[tokio::test]
async fn group_admins_cannot_remove_the_last_individual_admin_or_use_overage_claims() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    replace(&h, &alice, json!([{"group":"admins","access":"admin"}]))
        .await
        .error_for_status()
        .unwrap();
    *h.provider.mode.lock().await = "outsider".into();
    h.provider
        .claims
        .lock()
        .await
        .insert("groups".into(), json!(["admins"]));
    let admin = h.login().await;
    let before = h.json(&format!("{ROOT}/members"), &admin).await;
    let denied = h
        .write(
            Method::PUT,
            &format!("{ROOT}/members"),
            &admin,
            json!({
                "expected_revision":before["revision"],"members":[],"groups":before["groups"],
            }),
        )
        .await;
    assert_eq!(denied.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(h.json(&format!("{ROOT}/members"), &admin).await, before);
    h.provider
        .claims
        .lock()
        .await
        .insert("_claim_names".into(), json!({"groups":"external"}));
    let overage = h.login().await;
    assert_eq!(
        h.json("/api/repos", &overage).await["repositories"],
        json!([])
    );
    h.close().await;
}
