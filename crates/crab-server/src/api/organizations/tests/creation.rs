use super::*;

#[tokio::test]
async fn signed_in_profiles_found_organizations_as_their_first_owner_and_retries_replay() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let id = Uuid::now_v7();
    let create = json!({
        "id": id,
        "handle": "crab-platform",
        "name": "Crab platform",
        "description": "Shared tooling"
    });

    let unregistered = h
        .write(Method::POST, "/api/organizations", &alice, create.clone())
        .await;
    assert_eq!(unregistered.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        h.json("/api/organizations", &alice).await["items"],
        json!([])
    );

    h.write(
        Method::POST,
        "/api/profile",
        &alice,
        json!({"request_id":Uuid::now_v7(),"handle":"alice","name":"Alice"}),
    )
    .await
    .error_for_status()
    .unwrap();
    for invalid in [
        json!({"id":Uuid::now_v7(),"handle":"crab-platform","name":" Crab platform","description":""}),
        json!({"id":Uuid::now_v7(),"handle":"crab-platform","name":"","description":""}),
        json!({"id":Uuid::now_v7(),"handle":"alice","name":"Alice org","description":""}),
        json!({"id":Uuid::now_v7(),"handle":"api","name":"Reserved","description":""}),
        json!({"id":Uuid::now_v7(),"handle":"bad_handle","name":"Bad handle","description":""}),
        json!({"id":Uuid::nil(),"handle":"crab-platform","name":"Crab platform","description":""}),
    ] {
        let response = h
            .write(Method::POST, "/api/organizations", &alice, invalid)
            .await;
        assert!(matches!(
            response.status(),
            StatusCode::BAD_REQUEST | StatusCode::CONFLICT
        ));
    }

    let created: Value = h
        .write(Method::POST, "/api/organizations", &alice, create.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (
            &created["id"],
            &created["handle"],
            &created["name"],
            &created["description"]
        ),
        (
            &json!(id),
            &json!("crab-platform"),
            &json!("Crab platform"),
            &json!("Shared tooling")
        )
    );
    let by_handle = h
        .json("/api/organizations/by-handle/crab-platform", &alice)
        .await;
    assert_eq!(by_handle["organization"]["id"], json!(id));
    let replayed: Value = h
        .write(Method::POST, "/api/organizations", &alice, create.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replayed, created);
    let different = h
        .write(
            Method::POST,
            "/api/organizations",
            &alice,
            json!({"id":id,"handle":"crab-platform","name":"Another name","description":""}),
        )
        .await;
    assert_eq!(different.status(), StatusCode::CONFLICT);
    let duplicate_handle = h
        .write(
            Method::POST,
            "/api/organizations",
            &alice,
            json!({"id":Uuid::now_v7(),"handle":"crab-platform","name":"Duplicate","description":""}),
        )
        .await;
    assert_eq!(duplicate_handle.status(), StatusCode::CONFLICT);

    let root = format!("/api/organizations/{id}");
    let own = h.json(&root, &alice).await;
    assert_eq!(own["role"], "owner");
    let listed = h.json("/api/organizations", &alice).await;
    assert_eq!(
        (
            &listed["items"][0]["kind"],
            &listed["items"][0]["organization"]["id"],
            &listed["items"][0]["role"],
        ),
        (&json!("member"), &json!(id), &json!("owner"))
    );
    let members = h.json(&format!("{root}/members"), &alice).await;
    assert_eq!(
        (&members["items"][0]["actor"], &members["items"][0]["role"]),
        (&own["actor"], &json!("owner"))
    );

    // Another account cannot claim the UUID, even with the same details: the
    // existing Organization Cell denies non-members before Bootstrap runs.
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    h.write(
        Method::POST,
        "/api/profile",
        &bob,
        json!({"request_id":Uuid::now_v7(),"handle":"bob","name":"Bob"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let claimed = h
        .write(Method::POST, "/api/organizations", &bob, create)
        .await;
    assert_eq!(claimed.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        h.http
            .get(format!("{}{root}", h.origin))
            .header(header::COOKIE, &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(h.json("/api/organizations", &bob).await["items"], json!([]));
    h.close().await;
}
