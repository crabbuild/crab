use super::*;
use crate::cells::organization::teams::hierarchy_requests::{Decision, List, ListInput, Side};

#[tokio::test(flavor = "multi_thread")]
async fn child_maintainer_reviews_parent_request_using_both_current_authorities() {
    let f = fixture().await;
    let client = f.server.cells().unwrap();
    let carol = Uuid::now_v7();
    let other = Uuid::now_v7();
    join(&client, f.organization, f.owner, carol, 0).await;
    join(&client, f.organization, f.owner, other, 0).await;
    let parent = create(&f, f.bob, "Engineering", Visibility::Closed).await;
    let child = create(&f, carol, "Infrastructure", Visibility::Closed).await;

    let send = async |actor, command| {
        write::<Change>(&client, f.organization, request(actor, command)).await
    };
    let pending = async |actor| {
        read::<List>(
            &client,
            f.organization,
            ListInput {
                actor,
                team: None,
                after: None,
                request: None,
                incoming: false,
                limit: 30,
            },
        )
        .await
        .items
    };
    let proposal = |team| Command::ProposeHierarchy {
        team,
        parent,
        version: 1,
        side: Side::Parent,
    };
    let approve = |team, request| Command::DecideHierarchy {
        team,
        request,
        decision: Decision::Approve,
    };
    assert_eq!(
        send(f.bob, proposal(child)).await,
        Outcome::Applied { id: child }
    );
    assert!(pending(other).await.is_empty());
    let requests = pending(carol).await;
    assert!(requests[0].can_approve);
    let id = requests[0].id;
    assert_eq!(send(other, approve(child, id)).await, Outcome::Missing);
    assert_eq!(own(&f, carol, child).await.team.parent, None);
    assert_eq!(
        send(carol, approve(child, id)).await,
        Outcome::Applied { id: child }
    );
    assert_eq!(own(&f, carol, child).await.team.parent, Some(parent));
    assert!(pending(carol).await.is_empty());

    let next = create(&f, carol, "Network", Visibility::Closed).await;
    assert_eq!(
        send(f.bob, proposal(next)).await,
        Outcome::Applied { id: next }
    );
    let id = pending(carol).await[0].id;
    change_member(&f, f.owner, parent, f.bob, 1, None).await;
    assert!(!pending(carol).await[0].can_approve);
    assert_eq!(send(carol, approve(next, id)).await, Outcome::Conflict);
    assert_eq!(own(&f, carol, next).await.team.parent, None);
    assert_eq!(
        send(
            carol,
            Command::DecideHierarchy {
                team: next,
                request: id,
                decision: Decision::Decline,
            }
        )
        .await,
        Outcome::Applied { id: next }
    );
    assert!(pending(carol).await.is_empty());
}
