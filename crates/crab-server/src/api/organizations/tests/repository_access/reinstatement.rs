use super::*;
use crate::{
    catalog::{MembershipActor, membership_changes::Rejoin},
    cells::organization::{ChangeOutcome, DecideInvitation, Decision},
};

#[tokio::test(flavor = "multi_thread")]
async fn batch_removal_validates_the_whole_selection_before_revoking_access() {
    let f = AccessFixture::new().await;
    let vendor = super::conversion::invite_vendor(&f, "member").await;
    let catalog = f.h.server.catalog().unwrap();
    let (owner, name) = f
        .repository
        .strip_prefix("/api/repos/")
        .unwrap()
        .split_once('/')
        .unwrap();
    let mut direct = catalog.membership(owner, name).await.unwrap().members;
    direct.push(crate::RepositoryMember {
        subject: "vendor-id".into(),
        name: "Vendor".into(),
        access: crate::RepositoryAccess::Read,
    });
    catalog
        .set_members(owner, name, direct.clone(), true)
        .await
        .unwrap();

    let removal = format!("{}/members/removal", f.org);
    let request = |other: Uuid, version: u64| {
        json!({"members":[
            {"actor":f.member,"version":1}, {"actor":other,"version":version},
        ]})
    };
    let selected = request(vendor, 1);
    let revision = catalog.load().await.unwrap().0.version;
    for (selection, expected) in [
        (request(vendor, 2), StatusCode::CONFLICT),
        (request(f.owner_id, 1), StatusCode::BAD_REQUEST),
    ] {
        assert_eq!(
            f.h.write(Method::POST, &removal, &f.alice, selection)
                .await
                .status(),
            expected,
        );
        assert_eq!(catalog.load().await.unwrap().0.version, revision);
        assert_eq!(
            catalog.membership(owner, name).await.unwrap().members,
            direct
        );
    }
    assert_eq!(
        f.h.write(Method::POST, &removal, &f.bob, selected.clone())
            .await
            .status(),
        StatusCode::NOT_FOUND,
    );
    assert_eq!(
        f.get(&format!("{}/refs", f.repository), &f.bob)
            .await
            .status(),
        StatusCode::OK
    );

    let result: Value =
        f.h.write(Method::POST, &removal, &f.alice, selected)
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(result, json!({"removed":2}));
    let document = catalog.load().await.unwrap().0;
    assert!(document.pending_membership_changes.is_empty());
    for actor in [f.member, vendor] {
        let path = format!("{}/members/{actor}", f.org);
        let member = f.h.json(&path, &f.alice).await;
        assert_eq!(member["member"]["active"], false);
        assert_eq!(member["member"]["version"], 2);
        assert!(
            document
                .former_grants(f.organization, actor)
                .unwrap()
                .matches(2)
        );
    }
    assert_eq!(
        catalog.membership(owner, name).await.unwrap().members,
        vec![]
    );
    assert_eq!(
        f.get(&format!("{}/refs", f.repository), &f.bob)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    f.h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reinstatement_restores_eligible_team_and_direct_grants_only_after_acceptance() {
    let f = AccessFixture::new().await;
    let membership = format!("{}/members/{}", f.org, f.member);
    let offer = format!("{}/invitations/{}", f.org, f.member);
    let own_offer = format!("{}/invitation", f.org);
    let commands = format!("{}/teams/commands", f.org);
    let retired = Uuid::now_v7();
    f.h.write(
        Method::POST,
        &commands,
        &f.alice,
        json!({
            "request_id":retired, "command":{
                "kind":"create","name":"Retired","description":"","visibility":"secret",
            },
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    f.h.write(
        Method::POST,
        &commands,
        &f.alice,
        json!({
            "request_id":Uuid::now_v7(), "command":{
                "kind":"member","team":retired,"member":f.member,"version":0,"role":"maintainer",
            },
        }),
    )
    .await
    .error_for_status()
    .unwrap();

    for restore in [true, false] {
        let member = f.h.json(&membership, &f.alice).await;
        let removed: Value =
            f.h.write(
                Method::DELETE,
                &membership,
                &f.alice,
                json!({"version":member["member"]["version"]}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let view = f.h.json(&offer, &f.alice).await;
        assert_eq!(
            f.get(&format!("{}/refs", f.repository), &f.bob)
                .await
                .status(),
            StatusCode::NOT_FOUND,
        );
        assert_eq!(
            view["reinstatement"],
            json!({"role":"member","version":removed["version"]})
        );
        assert!(
            f.h.json(&own_offer, &f.bob)
                .await
                .get("reinstatement")
                .is_none()
        );
        assert_eq!(
            f.h.write(
                Method::PUT,
                &membership,
                &f.alice,
                json!({
                    "role":"member", "version":removed["version"],
                })
            )
            .await
            .status(),
            StatusCode::NOT_FOUND,
        );

        let mut input = json!({"role":"member","version":view["invitation"]["version"]});
        if restore {
            input["reinstate_version"] = json!(1);
            assert_eq!(
                f.h.write(Method::PUT, &offer, &f.alice, input.clone())
                    .await
                    .status(),
                StatusCode::CONFLICT,
            );
            input["reinstate_version"] = removed["version"].clone();
        }
        let invited: Value =
            f.h.write(Method::PUT, &offer, &f.alice, input)
                .await
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
        assert_eq!(invited["reinstates"].as_bool().unwrap_or(false), restore);
        assert_eq!(
            f.get(&format!("{}/teams/{}", f.org, f.team), &f.bob)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        if restore {
            f.h.write(
                Method::POST,
                &commands,
                &f.alice,
                json!({
                    "request_id":Uuid::now_v7(), "command":{
                        "kind":"delete","team":retired,"version":2,
                    },
                }),
            )
            .await
            .error_for_status()
            .unwrap();
        }
        f.h.write(
            Method::POST,
            &own_offer,
            &f.bob,
            json!({
                "accept":true,"version":invited["version"],
            }),
        )
        .await
        .error_for_status()
        .unwrap();
        let access = f.h.json(&f.path(f.member), &f.alice).await;
        assert_eq!(access["items"][0]["name"], "library");
        assert_eq!(
            access["items"][0]["access"],
            if restore { json!("write") } else { Value::Null }
        );
        assert_eq!(
            access["items"][0]["direct"],
            if restore { json!("read") } else { Value::Null }
        );
        assert_eq!(
            f.get(&format!("{}/teams/{retired}", f.org), &f.bob)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert!(
            f.h.json(&offer, &f.alice)
                .await
                .get("reinstatement")
                .is_none()
        );
    }

    f.h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_only_restores_grants_for_the_exact_accepted_invitation() {
    for accept in [true, false] {
        let f = AccessFixture::new().await;
        let membership = format!("{}/members/{}", f.org, f.member);
        f.h.write(Method::DELETE, &membership, &f.alice, json!({"version":1}))
            .await
            .error_for_status()
            .unwrap();
        let offer = format!("{}/invitations/{}", f.org, f.member);
        f.h.write(
            Method::PUT,
            &offer,
            &f.alice,
            json!({
                "role":"member", "version":2, "reinstate_version":2,
            }),
        )
        .await
        .error_for_status()
        .unwrap();
        let catalog = f.h.server.catalog().unwrap();
        catalog
            .prepare_rejoin(
                f.organization,
                Rejoin {
                    recipient: f.member,
                    invitation_version: 3,
                    member_version: 2,
                    reinstates: true,
                    identity: MembershipActor {
                        issuer: f.h.provider.issuer.clone(),
                        subject: "bob-id".into(),
                    },
                },
                &f.h.server.cancellation,
            )
            .await
            .unwrap();
        let acl = format!("{}/members", f.repository);
        let before = f.h.json(&acl, &f.alice).await;
        assert_eq!(before["members"], json!([]));
        if accept {
            let cells = f.h.server.cells().unwrap();
            let applied = cells
                .command::<DecideInvitation>(
                    crate::cells::Address::Organization(f.organization),
                    crate::app::mutation_identity().unwrap(),
                    Decision {
                        actor: f.member,
                        version: 3,
                        accept,
                    },
                )
                .await
                .unwrap()
                .output;
            assert!(matches!(applied, ChangeOutcome::Applied(_)));
        } else {
            f.h.write(Method::DELETE, &offer, &f.alice, json!({"version":3}))
                .await
                .error_for_status()
                .unwrap();
        }
        for _ in 0..2 {
            crate::api::recover_organization_membership_change(&f.h.server, f.organization)
                .await
                .unwrap();
        }
        let (document, _) = catalog.load().await.unwrap();
        assert!(document.pending_membership_changes.is_empty());
        assert_eq!(
            document.former_grants(f.organization, f.member).is_none(),
            accept
        );
        let after = f.h.json(&acl, &f.alice).await;
        assert_eq!(
            after["members"],
            if accept {
                json!([{"subject":"bob-id", "name":"Bob", "access":"read"}])
            } else {
                json!([])
            }
        );
        assert_ne!(after["revision"], before["revision"]);
        f.h.close().await;
    }
}
