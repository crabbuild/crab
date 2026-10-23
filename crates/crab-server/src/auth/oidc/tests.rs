use super::*;
use serde_json::json;

#[tokio::test]
async fn verified_groups_survive_durable_session_reload_with_a_fifteen_minute_ceiling() {
    let h = crate::test_support::http::Harness::new(false).await;
    h.provider
        .claims
        .lock()
        .await
        .insert("groups".into(), json!(["Engineering"]));
    let cookie = h.login().await;
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(axum::http::header::COOKIE, cookie.parse().unwrap());
    let auth = h.server.auth.as_ref().unwrap();
    let principal = auth.principal(&headers).await;
    let session = principal.user_session().unwrap();
    assert_eq!(session.groups, vec!["Engineering"]);
    assert!(session.expires_at <= super::super::now_epoch().unwrap() + 900);
    auth.sessions.lock().await.clear();
    assert_eq!(
        auth.principal(&headers)
            .await
            .user_session()
            .unwrap()
            .groups,
        session.groups
    );
    h.close().await;
}

#[test]
fn signed_groups_are_bounded_exact_identifiers_not_display_names_or_remote_claims() {
    for (input, expected) in [
        (json!({}), vec![]),
        (
            json!({"groups":["/Engineering","Readers","Readers"]}),
            vec!["/Engineering", "Readers"],
        ),
        (json!({"groups":["Admins"],"hasgroups":true}), vec![]),
        (
            json!({"groups":["Admins"],"_claim_names":{"groups":"src1"}}),
            vec![],
        ),
    ] {
        let claims: Claims = serde_json::from_value(input).unwrap();
        assert_eq!(claims.groups().unwrap(), expected);
    }
    for input in [
        json!({"groups":"Admins"}),
        json!({"groups":[1]}),
        json!({"groups":null}),
    ] {
        assert!(serde_json::from_value::<Claims>(input).is_err());
    }
    for values in [
        vec!["".into()],
        vec![" Admins".into()],
        vec!["Admin\n".into()],
        vec!["x".repeat(513)],
        vec!["a".into(); 257],
        vec!["x".repeat(512); 65],
    ] {
        assert!(groups(values).is_err());
    }
}
