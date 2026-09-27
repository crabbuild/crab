use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_admission_follows_the_winner_of_a_concurrent_owner_claim() {
    use crab_cell_runtime::{
        control::{Owner, Transition},
        fleet::placement::PlacementPlanner,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Notify;

    let fixture = Fixture::new().await;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let first = Arc::new(AtomicBool::new(true));
    let remote = super::provisioning::Remote::with_router(&fixture, |router| {
        let entered = entered.clone();
        let release = release.clone();
        router.layer(axum::middleware::from_fn(
            move |request, next: axum::middleware::Next| {
                let entered = entered.clone();
                let release = release.clone();
                let first = first.clone();
                async move {
                    if first.swap(false, Ordering::SeqCst) {
                        entered.notify_one();
                        release.notified().await;
                    }
                    next.run(request).await
                }
            },
        ))
    })
    .await;
    let cell = fixture.data[0].0.cell_id();
    fixture.data[0].0.drain().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let selected = fixture
                .directory
                .choose_advertised_placement(&PlacementPlanner::default(), cell, now_ms(), 4)
                .await
                .unwrap();
            if selected.is_some_and(|selected| selected.session == remote.session) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let sdk = super::provisioning::sdk_without_retries(&fixture);
    let item = fixture.data[0].1.clone();
    let key = item["id"].clone();
    let request = tokio::spawn(async move {
        sdk.get_item()
            .table_name("Residency")
            .key("id", key)
            .consistent_read(true)
            .send()
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    // Placement selected the peer, but another admission wins the authority CAS
    // before its activation arrives. Leave that winner in recovery deliberately.
    let authority = CellAuthority::new(fixture.layout.clone());
    let idle = authority.load(cell).await.unwrap().unwrap();
    let claimed = idle
        .value()
        .takeover(Owner {
            session: fixture.session,
            endpoint: fixture.endpoint.clone(),
        })
        .unwrap();
    authority
        .transition(&idle, claimed, Transition::Takeover)
        .await
        .unwrap();
    release.notify_one();
    let read = request.await.unwrap().unwrap();
    assert_eq!(read.item, Some(item));
    assert_eq!(
        authority
            .load(cell)
            .await
            .unwrap()
            .unwrap()
            .value()
            .owner
            .as_ref()
            .unwrap()
            .session,
        fixture.session
    );
    remote.shutdown().await;
    fixture.shutdown().await;
}
