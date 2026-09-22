use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use cellule_app::ApplicationHandle;
use cellule_runtime::{
    BoundedDecoder, Command, PreparedCommand, Resolution, StoredOutcome, WireValue,
};
use tokio::sync::Notify;

use crate::fixture;

pub fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("qualification clock")
            .as_millis(),
    )
    .expect("qualification clock bounds")
}

pub async fn cancel_prepared<C: Command>(
    prepared: PreparedCommand<C>,
    observer: &ApplicationHandle<fixture::ReferenceApplication>,
    entered: Arc<Notify>,
    dispatched: &Arc<AtomicUsize>,
    after_dispatch: bool,
) -> (PreparedCommand<C>, Resolution) {
    let evidence = prepared.evidence().clone();
    let retained_attempt = prepared.clone();
    let task = tokio::spawn(async move { prepared.execute().await });
    tokio::time::timeout(Duration::from_secs(20), entered.notified())
        .await
        .expect("mutation reached injected await boundary");
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    assert_eq!(
        dispatched.load(Ordering::Acquire),
        usize::from(after_dispatch)
    );
    let resolution = observer
        .resolve(&evidence)
        .await
        .expect("separate client request resolution");
    (retained_attempt, resolution)
}

pub fn committed_output<T: WireValue>(outcome: StoredOutcome) -> (T, u64) {
    let StoredOutcome::Success {
        result,
        commit_sequence,
    } = outcome
    else {
        panic!("cancelled request ledger recorded a rejection");
    };
    let mut decoder = BoundedDecoder::new(&result, 1 << 20).expect("bounded ledger result");
    let value = T::decode(&mut decoder).expect("typed ledger result");
    decoder
        .finish()
        .expect("ledger result has no trailing bytes");
    (value, commit_sequence)
}
