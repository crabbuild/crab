use super::*;
use crate::{
    RepositoryAccess,
    catalog::{
        MembershipActor,
        membership_changes::{Begin, DepartureGrants, Pending},
    },
    cells::{
        Address,
        directory::{IdentityKey, RegisterAccounts},
        organization::{
            DecideInvitation, Decision, Departure, DepartureMember, DepartureMode,
            FinalizeDeparture,
        },
    },
};

pub(super) async fn invite_vendor(f: &AccessFixture, role: &str) -> Uuid {
    let cells = f.h.server.cells().unwrap();
    let issuer = f.h.server.auth.as_ref().unwrap().issuer().to_owned();
    let registered = cells
        .command::<RegisterAccounts>(
            Address::Directory,
            crate::app::mutation_identity().unwrap(),
            vec![IdentityKey {
                issuer,
                subject: "vendor-id".into(),
            }],
        )
        .await
        .unwrap()
        .output;
    let vendor = registered[0];
    let invitation = format!("{}/invitations/{vendor}", f.org);
    f.h.write(
        Method::PUT,
        &invitation,
        &f.alice,
        json!({"role":role,"version":0}),
    )
    .await
    .error_for_status()
    .unwrap();
    cells
        .command::<DecideInvitation>(
            Address::Organization(f.organization),
            crate::app::mutation_identity().unwrap(),
            Decision {
                actor: vendor,
                version: 1,
                accept: true,
            },
        )
        .await
        .unwrap();
    vendor
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_conversion_validates_all_versions_and_keeps_each_members_team_access() {
    let f = AccessFixture::new().await;
    let policy = format!("{}/repository-policy", f.org);
    let mut input = f.h.json(&policy, &f.alice).await;
    input["baseline"] = json!("read");
    f.h.write(Method::PUT, &policy, &f.alice, input)
        .await
        .error_for_status()
        .unwrap();
    let extra: Value =
        f.h.write(
            Method::POST,
            &format!("{}/repositories/creation", f.org),
            &f.alice,
            json!({"request_id":Uuid::now_v7(),"name":"team-only","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let extra_id = Uuid::parse_str(extra["id"].as_str().unwrap()).unwrap();
    let extra_root = format!("/api/repos/{}/team-only", extra["owner"].as_str().unwrap());
    let team_access = format!("{extra_root}/team-access");
    let version = f.h.json(&team_access, &f.alice).await["revision"].clone();
    repository_write(
        &f.h,
        Method::PUT,
        &team_access,
        &f.alice,
        extra_id,
        json!({
            "expected_revision":version,"teams":[{"team":f.team,"access":"triage"}],
        }),
    )
    .await
    .error_for_status()
    .unwrap();

    let vendor = invite_vendor(&f, "owner").await;
    f.h.write(
        Method::POST,
        &format!("{}/teams/commands", f.org),
        &f.alice,
        json!({
            "request_id":Uuid::now_v7(), "command":{
                "kind":"member","team":f.team,"member":vendor,"version":0,"role":"member",
            },
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    let member = format!("{}/members/{}", f.org, f.member);
    let vendor_path = format!("{}/members/{vendor}", f.org);
    let path = format!("{}/members/conversion", f.org);
    let version = f.h.json(&member, &f.alice).await["member"]["version"]
        .as_u64()
        .unwrap();
    let vendor_version = f.h.json(&vendor_path, &f.alice).await["member"]["version"]
        .as_u64()
        .unwrap();
    let owner = format!("{}/members/{}", f.org, f.owner_id);
    let owner_version = f.h.json(&owner, &f.alice).await["member"]["version"].clone();
    let selection = json!({"members":[
        {"actor":f.member,"version":version}, {"actor":vendor,"version":vendor_version},
    ]});
    let catalog = f.h.server.catalog().unwrap();
    let before_revision = catalog.load().await.unwrap().0.version;
    assert_eq!(
        f.h.write(Method::POST, &path, &f.bob, selection.clone())
            .await
            .status(),
        StatusCode::NOT_FOUND,
    );
    assert_eq!(
        f.h.write(
            Method::POST,
            &path,
            &f.alice,
            json!({"members":[
                {"actor":f.member,"version":version},
                {"actor":f.member,"version":version},
            ]})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST,
    );
    assert_eq!(
        f.h.write(
            Method::POST,
            &path,
            &f.alice,
            json!({"members":[
                {"actor":f.owner_id,"version":owner_version},
                {"actor":vendor,"version":vendor_version},
            ]})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST,
    );
    assert_eq!(
        f.h.write(
            Method::POST,
            &path,
            &f.alice,
            json!({"members":[
                {"actor":f.member,"version":version},
                {"actor":vendor,"version":vendor_version + 1},
            ]})
        )
        .await
        .status(),
        StatusCode::CONFLICT,
    );
    assert_eq!(catalog.load().await.unwrap().0.version, before_revision);
    assert_eq!(f.h.json(&member, &f.alice).await["member"]["active"], true);
    assert_eq!(
        f.h.json(&vendor_path, &f.alice).await["member"]["active"],
        true
    );

    let baseline = format!(
        "{}/private-tool/refs",
        f.repository.rsplit_once('/').unwrap().0
    );
    assert_eq!(f.get(&baseline, &f.bob).await.status(), StatusCode::OK);
    let response: Value =
        f.h.write(Method::POST, &path, &f.alice, selection)
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(response, json!({"converted":2}));
    assert_eq!(f.get(&f.org, &f.bob).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        f.get(&baseline, &f.bob).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.get(&format!("{}/refs", f.repository), &f.bob)
            .await
            .status(),
        StatusCode::OK
    );

    let removed = f.h.json(&member, &f.alice).await;
    assert_eq!(removed["member"]["active"], false);
    assert_eq!(removed["member"]["version"], version + 1);
    let vendor_record = f.h.json(&vendor_path, &f.alice).await;
    assert_eq!(vendor_record["member"]["active"], false);
    assert_eq!(vendor_record["member"]["version"], vendor_version + 1);
    let people = format!("{}/outside-collaborators", f.org);
    let outside = f.h.json(&people, &f.alice).await;
    let outsiders = outside["items"].as_array().unwrap();
    assert_eq!(outsiders.len(), 2);
    for outsider in outsiders {
        assert_eq!(outsider["repository_count"], 2);
        let key = outsider["key"].as_str().unwrap();
        let grants =
            f.h.json(&format!("{people}/{key}/repositories"), &f.alice)
                .await;
        assert_eq!(grants["items"][0]["access"], "write");
        assert_eq!(grants["items"][1]["name"], "team-only");
        assert_eq!(grants["items"][1]["access"], "triage");
    }
    let audit =
        f.h.json(
            &format!("{}/audit-log?action=organization_membership", f.org),
            &f.alice,
        )
        .await;
    assert!(
        audit["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["account"] == json!(f.member)
                && event["detail"] == "Converted to outside collaborator")
    );

    let (owner, name) = f
        .repository
        .strip_prefix("/api/repos/")
        .unwrap()
        .split_once('/')
        .unwrap();
    let catalog = f.h.server.catalog().unwrap();
    let snapshot = catalog.membership(owner, name).await.unwrap();
    assert_eq!(snapshot.members[0].access, RepositoryAccess::Write);
    assert_eq!(snapshot.groups[0].group, "release-admins");
    let (document, _) = catalog.load().await.unwrap();
    assert!(document.pending_membership_changes.is_empty());
    f.h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_clears_an_already_finalized_conversion_exactly_once() {
    let f = AccessFixture::new().await;
    let vendor = invite_vendor(&f, "member").await;
    let vendor_path = format!("{}/members/{vendor}", f.org);
    let vendor_version = f.h.json(&vendor_path, &f.alice).await["member"]["version"]
        .as_u64()
        .unwrap();
    let member = format!("{}/members/{}", f.org, f.member);
    let loaded = f.h.json(&member, &f.alice).await;
    let version = loaded["member"]["version"].as_u64().unwrap();
    let decision = Departure {
        owner: f.owner_id,
        mode: DepartureMode::Convert,
        members: vec![
            DepartureMember {
                actor: f.member,
                version,
            },
            DepartureMember {
                actor: vendor,
                version: vendor_version,
            },
        ],
    };
    let catalog = f.h.server.catalog().unwrap();
    let (document, _) = catalog.load().await.unwrap();
    catalog
        .prepare_departure(
            Begin {
                organization: f.organization,
                revision: document.version,
                decision: decision.clone(),
                members: vec![
                    DepartureGrants {
                        identity: Some(MembershipActor {
                            issuer: f.h.provider.issuer.clone(),
                            subject: "bob-id".into(),
                        }),
                        name: "Bob".into(),
                        teams: [f.team].into_iter().collect(),
                    },
                    DepartureGrants {
                        identity: Some(MembershipActor {
                            issuer: f.h.provider.issuer.clone(),
                            subject: "vendor-id".into(),
                        }),
                        name: "Vendor".into(),
                        teams: Default::default(),
                    },
                ],
                actor: MembershipActor {
                    issuer: "urn:crab:local".into(),
                    subject: "operator".into(),
                },
            },
            &f.h.server.cancellation,
        )
        .await
        .unwrap();
    assert_eq!(f.h.json(&member, &f.alice).await["member"]["active"], true);
    let cells = f.h.server.cells().unwrap();
    cells
        .command::<FinalizeDeparture>(
            crate::cells::Address::Organization(f.organization),
            crate::app::mutation_identity().unwrap(),
            decision.clone(),
        )
        .await
        .unwrap();
    let (pending, _) = catalog.load().await.unwrap();
    assert_eq!(
        pending.pending_membership_changes[&f.organization],
        Pending::Departure(decision)
    );

    for _ in 0..2 {
        crate::api::recover_organization_membership_change(&f.h.server, f.organization)
            .await
            .unwrap();
    }
    let (recovered, _) = catalog.load().await.unwrap();
    assert!(recovered.pending_membership_changes.is_empty());
    let finalized = f.h.json(&member, &f.alice).await;
    assert_eq!(finalized["member"]["version"], version + 1);
    assert_eq!(finalized["member"]["active"], false);
    let finalized_vendor = f.h.json(&vendor_path, &f.alice).await;
    assert_eq!(finalized_vendor["member"]["version"], vendor_version + 1);
    assert_eq!(finalized_vendor["member"]["active"], false);
    f.h.close().await;
}
