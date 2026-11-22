use super::*;
use crate::{
    app,
    cells::repository::collaboration::watches::{Mutation, MutationResult, thread_key},
    test_support::http::Harness,
};
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Deserialize, Serialize)]
pub(crate) struct FollowTimes {
    actor: IdentityKey,
    times: Vec<(Subject, Option<u64>)>,
}

pub(crate) struct SetFollowTimes;

impl Mutation for SetFollowTimes {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 984;
    type Input = FollowTimes;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, input: FollowTimes) -> Result<MutationResult<()>> {
        let actor = identity_key(&input.actor)?;
        for (subject, timestamp) in input.times {
            let mut current = thread_subscription(context, subject, actor)?;
            let previous = current.origin.unwrap();
            current.origin = timestamp.map(|subscribed_at_ms| SubscriptionOrigin {
                subscribed_at_ms,
                ..previous
            });
            update(context, actor, subject, Some(previous), current.origin)?;
            context.put_document(&thread_key(subject, actor), &current)?;
        }
        Ok(MutationResult::Success(()))
    }
}

#[tokio::test]
async fn time_order_merges_repositories_without_losing_equal_dates_or_undated_follows() {
    let h = Harness::new(false).await;
    h.add_repository("library").await;
    let alice = h.login().await;
    let session = h.json("/api/session", &alice).await;
    let actor = IdentityKey {
        issuer: session["user"]["issuer"].as_str().unwrap().into(),
        subject: session["user"]["subject"].as_str().unwrap().into(),
    };
    let mut expected = Vec::new();
    let mut undated = None;
    for name in ["library", "private"] {
        let repo = h
            .server
            .repositories
            .get(&("team".into(), name.into()))
            .unwrap();
        let mut times = Vec::new();
        for number in 1..=12 {
            h.http.post(format!("{}/api/repos/team/{name}/issues", h.origin))
                .header("cookie", &alice)
                .header("origin", &h.origin)
                .header("x-csrf-token", session["csrf"].as_str().unwrap())
                .header("x-crab-repository-id", repo.id.to_string())
                .json(&json!({"request_id":Uuid::now_v7(),"title":format!("{name} {number}"),"body":""}))
                .send().await.unwrap().error_for_status().unwrap();
            let timestamp = match (name, number) {
                ("private", 1) => None,
                ("library", 1) => Some(2_000),
                _ => Some(1_000),
            };
            if let Some(timestamp) = timestamp {
                expected.push((timestamp, repo.id, number));
            } else {
                undated = Some(repo.id);
            }
            times.push((
                Subject {
                    kind: SubjectKind::Issue,
                    number,
                },
                timestamp,
            ));
        }
        let cells = h.server.cells().unwrap();
        let cell = cells.repository(repo.id).unwrap();
        cell.client
            .command::<SetFollowTimes>(
                &cell.target,
                app::mutation_identity().unwrap(),
                FollowTimes {
                    actor: actor.clone(),
                    times,
                },
            )
            .await
            .unwrap();
    }
    expected.sort_unstable();
    let rank = |row: &Value| {
        (
            row["origin"]["subscribed_at_ms"].as_u64().unwrap(),
            serde_json::from_value::<Uuid>(row["repository"]["id"].clone()).unwrap(),
            row["subject"]["number"].as_u64().unwrap(),
        )
    };
    for sort in ["oldest", "recent"] {
        let listing = format!("/api/notifications/subscriptions?reason=participation&sort={sort}");
        let first = h.json(&listing, &alice).await;
        let cursor = first["next"].as_str().unwrap();
        let second = h.json(&format!("{listing}&after={cursor}"), &alice).await;
        assert_eq!(second["next"], Value::Null);
        let actual: Vec<_> = first["items"]
            .as_array()
            .unwrap()
            .iter()
            .chain(second["items"].as_array().unwrap())
            .map(rank)
            .collect();
        let mut target = expected.clone();
        if sort == "recent" {
            target.reverse();
        }
        assert_eq!(actual, target);
    }

    let listing = "/api/notifications/subscriptions?sort=recent";
    let first = h.json(listing, &alice).await;
    let second = h
        .json(
            &format!("{listing}&after={}", first["next"].as_str().unwrap()),
            &alice,
        )
        .await;
    assert_eq!(second["next"], "undated");
    let last = h.json(&format!("{listing}&after=undated"), &alice).await;
    let [unknown] = last["items"].as_array().unwrap().as_slice() else {
        panic!("expected the historical follow")
    };
    assert_eq!(unknown["repository"]["id"], json!(undated.unwrap()));
    assert_eq!(unknown["subject"]["number"], 1);
    assert_eq!(unknown["origin"], Value::Null);
    assert_eq!(last["next"], Value::Null);

    let pref = "/api/repos/team/private/issues/1/subscription";
    h.write(
        Method::PUT,
        pref,
        &alice,
        json!({"mode":"state_changes","version":1}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json("/api/notifications/subscriptions?reason=unknown", &alice)
            .await["items"][0]["origin"],
        Value::Null
    );
    for (mode, version) in [("unsubscribed", 2), ("follow", 3), ("state_changes", 4)] {
        h.write(
            Method::PUT,
            pref,
            &alice,
            json!({"mode":mode,"version":version}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let manual = h
        .json(
            "/api/notifications/subscriptions?reason=manual&sort=recent",
            &alice,
        )
        .await;
    let [manual] = manual["items"].as_array().unwrap().as_slice() else {
        panic!("expected the explicit follow")
    };
    assert!(manual["origin"]["subscribed_at_ms"].as_u64().unwrap() > 2_000);
    assert_eq!(
        manual["preference"],
        json!({"mode":"state_changes","version":5})
    );
    h.close().await;
}
