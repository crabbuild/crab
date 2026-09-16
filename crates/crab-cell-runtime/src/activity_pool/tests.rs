use std::{
    sync::{Arc, Barrier},
    time::Duration,
};

use super::*;

fn completed(value: u8) -> ActivityJob {
    Box::new(move || Ok(ActivityExecution::Completed(vec![value])))
}

#[tokio::test(flavor = "multi_thread")]
async fn reservation_bounds_submitted_work_until_callback_finishes() {
    let pool = BlockingActivityPool::new(1).unwrap();
    let reservation = pool.try_reserve().unwrap().unwrap();
    assert!(pool.try_reserve().unwrap().is_none());
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let task = tokio::spawn({
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        async move {
            reservation
                .execute(Box::new(move || {
                    entered.wait();
                    release.wait();
                    Ok(ActivityExecution::Completed(vec![1]))
                }))
                .await
        }
    });
    tokio::task::spawn_blocking(move || entered.wait())
        .await
        .unwrap();
    task.abort();
    assert!(pool.try_reserve().unwrap().is_none());
    tokio::task::spawn_blocking(move || release.wait())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if pool.try_reserve().unwrap().is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    pool.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn handler_panic_isolated_from_next_job() {
    let pool = BlockingActivityPool::new(1).unwrap();
    let panic = pool
        .try_reserve()
        .unwrap()
        .unwrap()
        .execute(Box::new(|| panic!("handler panic")))
        .await
        .unwrap_err();
    assert!(matches!(panic, Error::ActivityPanic));
    let result = pool
        .try_reserve()
        .unwrap()
        .unwrap()
        .execute(completed(7))
        .await
        .unwrap();
    assert_eq!(result, ActivityExecution::Completed(vec![7]));
    pool.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_drains_submitted_callbacks_and_closes_admission() {
    let pool = BlockingActivityPool::new(1).unwrap();
    let reservation = pool.try_reserve().unwrap().unwrap();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let task = tokio::spawn({
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        async move {
            reservation
                .execute(Box::new(move || {
                    entered.wait();
                    release.wait();
                    Ok(ActivityExecution::Completed(vec![3]))
                }))
                .await
        }
    });
    tokio::task::spawn_blocking(move || entered.wait())
        .await
        .unwrap();
    let shutdown = tokio::spawn({
        let pool = pool.clone();
        async move { pool.shutdown().await }
    });
    tokio::task::spawn_blocking(move || release.wait())
        .await
        .unwrap();
    shutdown.await.unwrap().unwrap();
    assert_eq!(
        task.await.unwrap().unwrap(),
        ActivityExecution::Completed(vec![3])
    );
    assert!(matches!(pool.try_reserve(), Err(Error::RuntimeClosed)));
}
