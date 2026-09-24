//! Per-Cell request admission caps.

use super::*;

#[tokio::test]
async fn per_cell_request_admission_caps_inflight_and_queued_commands() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let first = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .execute(
                    mutation_identity_window(29, 10, 10_000),
                    Digest::from_bytes([29; 32]),
                    20,
                    0,
                    1,
                    move |_| {
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Ok(HandlerOutcome::Success(Vec::new()))
                    },
                )
                .await
        })
    };
    tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .unwrap();

    let (result_tx, mut result_rx) = tokio::sync::mpsc::unbounded_channel();
    for byte in 30..94 {
        let handle = handle.clone();
        let result_tx = result_tx.clone();
        tokio::spawn(async move {
            let result = handle
                .execute(
                    mutation_identity_window(byte, 10, 10_000),
                    Digest::from_bytes([byte; 32]),
                    21,
                    0,
                    1,
                    |_| Ok(HandlerOutcome::Success(Vec::new())),
                )
                .await;
            let _ = result_tx.send(result);
        });
    }
    drop(result_tx);
    assert!(matches!(
        result_rx.recv().await.unwrap(),
        Err(crab_cell_runtime::Error::Capacity(_))
    ));
    release_tx.send(()).unwrap();
    first.await.unwrap().unwrap();
    for _ in 0..63 {
        assert!(result_rx.recv().await.unwrap().is_ok());
    }
    handle.drain().await.unwrap();
}
