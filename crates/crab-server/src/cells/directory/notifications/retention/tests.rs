use super::*;
use crate::{
    app,
    cells::{
        Address,
        application::Read,
        directory::{
            IdentityKey,
            notifications::{
                Action, Deliver, Delivery, Notice, Reason, Recipient, Selected, SetState,
                SetStateInput, View,
            },
        },
        repository::collaboration::Target,
    },
    test_support::http::Harness,
};
use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};

#[derive(Deserialize, Serialize)]
pub(crate) struct FiledItems {
    actor: Uuid,
    threads: Vec<Uuid>,
}

pub(crate) struct AgeFiled;

impl Mutation for AgeFiled {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 926;
    type Input = FiledItems;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, input: FiledItems) -> Result<MutationResult<()>> {
        for thread in input.threads {
            let key = filing::item_key(input.actor, thread);
            let mut record = context.document::<Filed>(&key)?.unwrap();
            assert!(record.done_at.is_some());
            record.done_at = Some(context.now_ms() - filing::RETENTION_MS);
            context.put_document(&key, &record)?;
        }
        Ok(MutationResult::Success(()))
    }
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct Stored {
    filed: bool,
    indexed: bool,
    recent_copies: usize,
    through: Option<Position>,
}

pub(crate) struct InspectFiled;

impl Read for InspectFiled {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 926;
    type Input = FiledItems;
    type Output = Vec<Stored>;

    fn execute(context: &mut Transaction<'_>, input: FiledItems) -> Result<Vec<Stored>> {
        let (done, _) = filing::page(context, input.actor, None, View::Done)?;
        let mut entries = Vec::new();
        for access in Access::CHANNELS {
            let inbox: Inbox = context
                .document(&access.inbox(input.actor))?
                .unwrap_or_default();
            entries.extend(inbox.entries);
        }
        input
            .threads
            .into_iter()
            .map(|id| {
                Ok(Stored {
                    filed: filing::load(context, input.actor, id)?.is_some(),
                    indexed: done.iter().any(|record| record.id() == id),
                    recent_copies: entries
                        .iter()
                        .filter(|entry| entry.notice.thread_id() == id)
                        .count(),
                    through: completed_through(context, input.actor, id)?,
                })
            })
            .collect()
    }
}

#[tokio::test]
async fn expired_completion_retires_recent_copies_and_replays_without_destroying_bookmarks() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    for title in ["Completed", "Bookmarked completion", "Recent completion"] {
        h.write(
            Method::POST,
            "/api/repos/team/private/issues",
            &alice,
            json!({"request_id":Uuid::now_v7(),"title":title,"body":""}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let profile: Value = h
        .write(
            Method::POST,
            "/api/profile",
            &bob,
            json!({"request_id":Uuid::now_v7(),"handle":"bob","name":"Bob"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let actor: Uuid = serde_json::from_value(profile["id"].clone()).unwrap();
    let repository = h.server.repositories.values()[0].id;
    let cells = h.server.cells().unwrap();

    let notice = |number| Notice {
        id: Uuid::now_v7(),
        repository,
        target: Target::Issue { number }.into(),
        created_at_ms: number,
        sender: "Alice".into(),
    };
    let deliver = async |notice: Notice, access| {
        cells
            .command::<Deliver>(
                Address::Directory,
                app::mutation_identity().unwrap(),
                Delivery {
                    notice,
                    recipients: vec![Recipient {
                        actor,
                        access,
                        reason: Reason::Mention,
                    }],
                },
            )
            .await
            .unwrap();
    };
    let set_state = async |notice: &Notice, action: Value| {
        h.write(
            Method::PUT,
            &format!("/api/notifications/{}/state", notice.thread_id()),
            &bob,
            json!({"repository_id":repository,"expected":notice.position(),"action":action}),
        )
        .await
    };
    let first = notice(1);
    let saved = notice(2);
    let fresh = notice(3);
    for entry in [&first, &saved, &fresh] {
        deliver(entry.clone(), Access::Direct).await;
        deliver(entry.clone(), Access::Group).await;
        set_state(entry, json!({"done":true}))
            .await
            .error_for_status()
            .unwrap();
    }
    set_state(&saved, json!({"save":true}))
        .await
        .error_for_status()
        .unwrap();

    // Continue a native page after the first sweep has already removed rows
    // from the scanned key range.
    let extra: Vec<_> = (10..10 + FRAME as u64).map(notice).collect();
    for entry in &extra {
        deliver(entry.clone(), Access::Direct).await;
    }
    let session = h.json("/api/session", &bob).await;
    let identity = IdentityKey {
        issuer: session["user"]["issuer"].as_str().unwrap().to_owned(),
        subject: session["user"]["subject"].as_str().unwrap().to_owned(),
    };
    cells
        .command::<SetState>(
            Address::Directory,
            app::mutation_identity().unwrap(),
            SetStateInput {
                identity,
                items: extra
                    .iter()
                    .map(|entry| Selected {
                        id: entry.thread_id(),
                        repository_id: repository,
                        expected: entry.position(),
                    })
                    .collect(),
                action: Action::Done(true),
            },
        )
        .await
        .unwrap()
        .output
        .unwrap();

    let threads = vec![first.thread_id(), saved.thread_id(), fresh.thread_id()];
    let mut aged = threads[..2].to_vec();
    aged.extend(extra.iter().map(Notice::thread_id));
    cells
        .command::<AgeFiled>(
            Address::Directory,
            app::mutation_identity().unwrap(),
            FiledItems {
                actor,
                threads: aged,
            },
        )
        .await
        .unwrap();
    let inspect = async || {
        cells
            .query::<InspectFiled>(
                Address::Directory,
                None,
                FiledItems {
                    actor,
                    threads: threads.clone(),
                },
            )
            .await
            .unwrap()
            .output
    };
    assert!(
        inspect()
            .await
            .iter()
            .all(|stored| stored.indexed && stored.recent_copies == 2)
    );
    let visible_done = h.json("/api/notifications?view=done", &bob).await;
    assert_eq!(visible_done["items"].as_array().unwrap().len(), 1);
    assert_eq!(visible_done["items"][0]["id"], json!(fresh.thread_id()));

    let sweep = async |before| {
        cells
            .command::<SweepFiling>(
                Address::Directory,
                app::mutation_identity().unwrap(),
                FilingSweep { before },
            )
            .await
            .unwrap()
            .output
            .next
    };
    let next = sweep(None).await.unwrap();
    assert!(sweep(Some(next)).await.is_none());
    let after = inspect().await;
    assert_eq!(
        after[0],
        Stored {
            filed: false,
            indexed: false,
            recent_copies: 0,
            through: Some(first.position())
        }
    );
    assert!(after[1].filed && !after[1].indexed);
    assert!(after[2].filed && after[2].indexed);
    assert!(sweep(None).await.is_none());
    assert_eq!(inspect().await, after);
    assert_eq!(
        h.json("/api/notifications?view=done", &bob).await,
        visible_done
    );
    assert_eq!(
        h.json("/api/notifications?view=saved", &bob).await["items"][0]["id"],
        json!(saved.thread_id())
    );

    for access in Access::CHANNELS {
        deliver(first.clone(), access).await;
        deliver(
            Notice {
                created_at_ms: 0,
                id: Uuid::now_v7(),
                ..first.clone()
            },
            access,
        )
        .await;
    }
    assert_eq!(h.json("/api/notifications", &bob).await["items"], json!([]));
    assert_eq!(
        h.json("/api/notifications/unread", &bob).await,
        json!({"unread":false})
    );
    assert_eq!(inspect().await, after);
    assert_eq!(
        set_state(&first, json!({"done":false})).await.status(),
        StatusCode::CONFLICT
    );

    let newer = Notice {
        id: Uuid::now_v7(),
        created_at_ms: 4,
        ..first.clone()
    };
    deliver(newer.clone(), Access::Direct).await;
    deliver(first, Access::Group).await;
    let inbox = h.json("/api/notifications", &bob).await;
    assert_eq!(inbox["items"].as_array().unwrap().len(), 1);
    assert_eq!(inbox["items"][0]["updated"], json!(newer.position()));
    assert_eq!(
        h.json("/api/notifications/unread", &bob).await,
        json!({"unread":true})
    );
    h.close().await;
}
