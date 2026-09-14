//! Bounded independent reads; dropping the cohort cancels all remaining tasks.

use super::*;

pub(crate) async fn ordered<T: Send + 'static>(
    jobs: impl Iterator<Item = impl std::future::Future<Output = Result<T>> + Send + 'static>,
) -> Result<Vec<T>> {
    let mut remaining = jobs.enumerate();
    let mut running = tokio::task::JoinSet::new();
    let mut output = std::collections::BTreeMap::new();
    loop {
        while running.len() < 8 {
            let Some((index, job)) = remaining.next() else {
                break;
            };
            running.spawn(async move { job.await.map(|value| (index, value)) });
        }
        let Some(result) = running.join_next().await else {
            break;
        };
        let (index, value) = result??;
        output.insert(index, value);
    }
    Ok(output.into_values().collect())
}

impl Replica {
    pub(super) async fn put(&self, key: &object_store::path::Path, bytes: Bytes) -> Result<()> {
        let _permit = self.host.io_permit().await?;
        self.layout.store().put(key, bytes).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn cohorts_overlap_reads_preserve_order_and_share_one_io_ceiling() {
        let slots = Arc::new(tokio::sync::Semaphore::new(3));
        let host = crate::Host::default().with_io_slots(slots.clone());
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let run = || {
            ordered((0..20).map(|index| {
                let host = host.clone();
                let active = active.clone();
                let peak = peak.clone();
                async move {
                    let _permit = host.io_permit().await?;
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(current, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis((20 - index) % 4 + 1))
                        .await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(index)
                }
            }))
        };
        let (first, second) = tokio::join!(run(), run());
        assert_eq!(first.unwrap(), (0..20).collect::<Vec<_>>());
        assert_eq!(second.unwrap(), (0..20).collect::<Vec<_>>());
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert_eq!(slots.available_permits(), 3);
    }

    #[tokio::test]
    async fn cancelling_a_cohort_releases_all_inflight_io_permits() {
        let slots = Arc::new(tokio::sync::Semaphore::new(2));
        let host = crate::Host::default().with_io_slots(slots.clone());
        let (started, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            ordered((0..30).map(|_| {
                let host = host.clone();
                let started = started.clone();
                async move {
                    let _permit = host.io_permit().await?;
                    started.send(()).unwrap();
                    std::future::pending::<Result<()>>().await
                }
            }))
            .await
        });
        observed.recv().await.unwrap();
        observed.recv().await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let permit = tokio::time::timeout(std::time::Duration::from_secs(2), slots.acquire_many(2))
            .await
            .unwrap()
            .unwrap();
        drop(permit);
        assert_eq!(slots.available_permits(), 2);
    }
}
