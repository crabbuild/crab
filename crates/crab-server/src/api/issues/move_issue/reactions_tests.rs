use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app,
    cells::repository::{
        RepositoryAuthor,
        collaboration::{
            Target,
            reactions::{Kind, Outcome, Selection, SetReaction},
        },
    },
    test_support::http::Harness,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_move_preserves_paged_reactions_and_each_viewers_memberships() {
    let h = Harness::new(false).await;
    h.add_repository("library").await;
    let alice = h.login().await;
    let origin = "/api/repos/team/private";
    let issue_list = format!("{origin}/issues");
    for title in ["Other issue", "Move the discussion"] {
        h.write(
            Method::POST,
            &issue_list,
            &alice,
            json!({"request_id":Uuid::now_v7(),"title":title,"body":"Discussion"}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let issue_path = format!("{issue_list}/2");
    let issue = h.json(&issue_path, &alice).await;
    let comment: Value = h
        .write(
            Method::POST,
            &format!("{issue_path}/comments"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"body":"An attached response"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let comment_number = comment["number"].as_u64().unwrap();
    let source = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    let target = h
        .server
        .repositories
        .get(&("team".into(), "library".into()))
        .unwrap();
    let cell = app::repository_cell(&h.server, &source).unwrap();
    for index in 0..52 {
        assert!(matches!(
            cell.client
                .command::<SetReaction>(
                    cell.target,
                    app::mutation_identity().unwrap(),
                    Selection {
                        target: Target::Issue { number: 2 },
                        kind: Kind::ThumbsUp,
                        active: true,
                        actor: RepositoryAuthor {
                            issuer: h.provider.issuer.clone(),
                            subject: format!("participant-{index}"),
                            name: "Participant".into(),
                        },
                    },
                )
                .await
                .unwrap()
                .output,
            Outcome::Applied(_)
        ));
    }
    h.write(
        Method::PUT,
        &format!("{origin}/reactions/issues/2"),
        &alice,
        json!({"kind":"thumbs_up","active":true}),
    )
    .await
    .error_for_status()
    .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    h.write(
        Method::PUT,
        &format!("{origin}/reactions/issues/2/comments/{comment_number}"),
        &bob,
        json!({"kind":"heart","active":true}),
    )
    .await
    .error_for_status()
    .unwrap();

    let moved = h
        .write(
            Method::POST,
            &format!("{issue_path}/transfer"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"version":issue["version"],"destination":target.id}),
        )
        .await;
    assert_eq!(moved.status(), StatusCode::ACCEPTED);
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let old = h.json(&issue_path, &alice).await;
            if old["transfer"]["state"] == "ready" {
                assert_eq!(old["transfer"]["number"], 1);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    let destination = "/api/repos/team/library/issues/1";
    let comments = format!("{destination}/comments");
    for (cookie, issue_reacted, comment_reacted) in [(&alice, true, false), (&bob, false, true)] {
        let response = h.json(destination, cookie).await;
        assert_eq!(
            response["reactions"]["groups"]["thumbs_up"],
            json!({"count":53,"reacted":issue_reacted})
        );
        let response = h.json(&comments, cookie).await;
        assert_eq!(response["items"][0]["number"], comment_number);
        assert_eq!(
            response["items"][0]["reactions"]["groups"]["heart"],
            json!({"count":1,"reacted":comment_reacted})
        );
    }

    let transferred_list = h.json("/api/repos/team/library/issues", &alice).await;
    assert_eq!(transferred_list["items"][0]["reaction_count"], 53);
    let session = h.json("/api/session", &alice).await;
    let removed: Value = h
        .http
        .put(format!(
            "{}/api/repos/team/library/reactions/issues/1",
            h.origin
        ))
        .header(header::COOKIE, &alice)
        .header(header::ORIGIN, &h.origin)
        .header("x-crab-repository-id", target.id.to_string())
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .json(&json!({"kind":"thumbs_up","active":false}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        removed["groups"]["thumbs_up"],
        json!({"count":52,"reacted":false})
    );
    let list = h.json("/api/repos/team/library/issues", &alice).await;
    assert_eq!(list["items"][0]["reaction_count"], 52);
    h.close().await;
}
