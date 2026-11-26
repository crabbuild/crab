use super::*;

async fn invite(
    h: &Harness,
    root: &str,
    sender: &str,
    recipient: Uuid,
    version: u64,
    role: &str,
) -> Value {
    h.write(
        Method::PUT,
        &format!("{root}/invitations/{recipient}"),
        sender,
        json!({"version":version,"role":role}),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn offboarding_cancels_the_departing_senders_outstanding_invitations() {
    for converting in [false, true] {
        let h = Harness::new(false).await;
        let (id, _, bob_id) = crate::test_support::http::organizations::bootstrap(&h).await;
        let root = format!("/api/organizations/{id}");
        let alice = h.login().await;
        *h.provider.mode.lock().await = "member".into();
        let bob = h.login().await;
        let actors = h
            .server
            .cells()
            .unwrap()
            .command::<RegisterAccounts>(
                Address::Directory,
                app::mutation_identity().unwrap(),
                ["outsider-id", "independent-id"]
                    .into_iter()
                    .map(|subject| IdentityKey {
                        issuer: h.provider.issuer.clone(),
                        subject: subject.into(),
                    })
                    .collect(),
            )
            .await
            .unwrap()
            .output;
        let [outsider_id, independent_id] = actors.as_slice() else {
            panic!("expected two registered invitees");
        };
        *h.provider.mode.lock().await = "outsider".into();
        let outsider = h.login().await;
        let decision = format!("{root}/invitation");
        invite(&h, &root, &alice, bob_id, 0, "owner").await;
        h.write(
            Method::POST,
            &decision,
            &bob,
            json!({"accept":true,"version":1}),
        )
        .await
        .error_for_status()
        .unwrap();
        let sent = invite(&h, &root, &bob, *outsider_id, 0, "owner").await;
        let edited = invite(&h, &root, &alice, *outsider_id, 1, "member").await;
        assert_eq!(sent["expires_at_ms"], edited["expires_at_ms"]);
        invite(&h, &root, &alice, *independent_id, 0, "member").await;
        if converting {
            h.write(
                Method::POST,
                &format!("{root}/members/conversion"),
                &alice,
                json!({"members":[{"actor":bob_id,"version":1}]}),
            )
            .await
            .error_for_status()
            .unwrap();
        } else {
            let member = format!("{root}/members/{bob_id}");
            h.write(
                Method::PUT,
                &member,
                &alice,
                json!({"version":1,"role":"member"}),
            )
            .await
            .error_for_status()
            .unwrap();
            assert_eq!(
                h.json("/api/organizations", &outsider).await["items"][0]["kind"],
                "invitation"
            );
            h.write(Method::DELETE, &member, &alice, json!({"version":2}))
                .await
                .error_for_status()
                .unwrap();
        }

        let closed = h.json(&decision, &outsider).await;
        assert!(closed["organization"].is_null());
        assert_eq!(closed["invitation"]["state"], "cancelled");
        assert_eq!(closed["invitation"]["version"], 3);
        assert_eq!(
            h.json("/api/organizations", &outsider).await["items"],
            json!([])
        );
        assert_eq!(
            h.write(
                Method::POST,
                &decision,
                &outsider,
                json!({"accept":true,"version":2})
            )
            .await
            .status(),
            StatusCode::CONFLICT,
        );
        let pending = h
            .json(&format!("{root}/invitations?status=pending"), &alice)
            .await;
        assert_eq!(pending["items"].as_array().unwrap().len(), 1);
        assert_eq!(
            pending["items"][0]["invitation"]["recipient"],
            json!(independent_id)
        );
        let cancelled = h
            .json(&format!("{root}/invitations?status=cancelled"), &alice)
            .await;
        assert_eq!(cancelled["items"][0]["invitation"], closed["invitation"]);
        let audit = h
            .json(
                &format!("{root}/audit-log?action=organization_membership"),
                &alice,
            )
            .await;
        assert_eq!(
            audit["items"][0]["detail"],
            if converting {
                "Converted to outside collaborator · 1 invitation cancelled"
            } else {
                "Removed · 1 invitation cancelled"
            },
        );
        let replacement = invite(&h, &root, &alice, *outsider_id, 3, "member").await;
        assert_eq!(replacement["version"], 4);
        h.write(
            Method::POST,
            &decision,
            &outsider,
            json!({"accept":true,"version":4}),
        )
        .await
        .error_for_status()
        .unwrap();
        assert_eq!(
            h.json("/api/organizations", &outsider).await["items"][0]["kind"],
            "member"
        );
        h.close().await;
    }
}

async fn status(h: &Harness, path: &str, cookie: &str) -> StatusCode {
    h.http
        .get(format!("{}{path}", h.origin))
        .header(header::COOKIE, cookie)
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test(flavor = "multi_thread")]
async fn members_leave_on_their_own_but_cannot_remove_anyone_else() {
    let h = Harness::new(false).await;
    let (id, alice_id, bob_id) = crate::test_support::http::organizations::bootstrap(&h).await;
    let root = format!("/api/organizations/{id}");
    let removal = format!("{root}/members/removal");
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    invite(&h, &root, &alice, bob_id, 0, "member").await;
    h.write(
        Method::POST,
        &format!("{root}/invitation"),
        &bob,
        json!({"accept":true,"version":1}),
    )
    .await
    .error_for_status()
    .unwrap();

    let own = h.json(&format!("{root}/members/{bob_id}"), &bob).await;
    assert_eq!(own["member"]["role"], "member");
    let version = own["member"]["version"].as_u64().unwrap();
    assert_eq!(
        status(&h, &format!("{root}/members/{alice_id}"), &bob).await,
        StatusCode::NOT_FOUND,
    );
    for (path, members) in [
        (&removal, json!([{"actor":alice_id,"version":1}])),
        (
            &removal,
            json!([{"actor":alice_id,"version":1},{"actor":bob_id,"version":version}]),
        ),
        (
            &format!("{root}/members/conversion"),
            json!([{"actor":bob_id,"version":version}]),
        ),
    ] {
        let response = h
            .write(Method::POST, path, &bob, json!({ "members": members }))
            .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    let last_owner = h
        .write(
            Method::POST,
            &removal,
            &alice,
            json!({"members":[{"actor":alice_id,"version":1}]}),
        )
        .await;
    assert_eq!(last_owner.status(), StatusCode::BAD_REQUEST);

    let left: Value = h
        .write(
            Method::POST,
            &removal,
            &bob,
            json!({"members":[{"actor":bob_id,"version":version}]}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(left["removed"], 1);
    assert_eq!(status(&h, &root, &bob).await, StatusCode::NOT_FOUND);
    assert_eq!(h.json("/api/organizations", &bob).await["items"], json!([]));
    let members = h.json(&format!("{root}/members"), &alice).await;
    assert_eq!(members["items"].as_array().unwrap().len(), 1);
    h.close().await;
}
