use std::{fmt, sync::Arc, time::Duration};

use futures_util::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, memory::InMemory, path::Path,
};

use super::*;
use crate::{manifest_store, manifests::Manifest};

#[derive(Clone, Copy, Debug)]
enum Fault {
    LostReply,
    LostReplyAndRead,
    RejectedWrite,
    MismatchedMarker,
    OversizedMarker,
    CompactedBeforeRead,
}

#[derive(Debug)]
struct MarkerFaultStore {
    inner: Arc<InMemory>,
    marker_prefix: String,
    fault: Fault,
}

impl fmt::Display for MarkerFaultStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MarkerFaultStore")
    }
}

fn disconnected() -> object_store::Error {
    object_store::Error::Generic {
        store: "marker-fault",
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "lost reply",
        )),
    }
}

#[async_trait::async_trait]
impl ObjectStore for MarkerFaultStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        if !location.as_ref().starts_with(&self.marker_prefix) {
            return self.inner.put_opts(location, payload, options).await;
        }
        if matches!(self.fault, Fault::RejectedWrite) {
            return Err(disconnected());
        }
        let payload = match self.fault {
            Fault::MismatchedMarker => Bytes::from_static(b"wrong marker").into(),
            Fault::OversizedMarker => Bytes::from(vec![b'x'; 1024]).into(),
            _ => payload,
        };
        self.inner.put_opts(location, payload, options).await?;
        // The provider accepted the write, but the caller never receives success.
        Err(disconnected())
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        if location.as_ref().starts_with(&self.marker_prefix) && !options.head {
            match self.fault {
                Fault::LostReplyAndRead => return Err(disconnected()),
                Fault::CompactedBeforeRead => {
                    let store = Store::new(self.inner.clone());
                    let layout = StoreLayout::new(store.clone(), "commit-recovery".to_owned());
                    manifest_store::compact_ref_journal(
                        &store,
                        &layout,
                        "2026-09-04T00:00:00.000Z".to_owned(),
                        None,
                        "compactor".to_owned(),
                    )
                    .await
                    .unwrap()
                    .expect("committed transaction compacted before readback");
                }
                _ => {}
            }
        }
        self.inner.get_opts(location, options).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }
    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }
    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

async fn fixture(
    fault: Fault,
) -> (
    Store,
    StoreLayout<Store>,
    Store,
    RefJournalTransaction,
    Vec<RefJournalHeadSnapshot>,
) {
    let inner = Arc::new(InMemory::new());
    let origin = Store::new(inner.clone());
    let layout = StoreLayout::new(origin.clone(), "commit-recovery".to_owned());
    manifest_store::create_manifest(
        &origin,
        &layout,
        &Manifest::default_for_repo("refs/heads/main"),
    )
    .await
    .unwrap();
    let store = Store::with_retry(
        Arc::new(MarkerFaultStore {
            inner,
            marker_prefix: format!("{}/", layout.ref_journal_active_prefix()),
            fault,
        }),
        crab_storage::RetryPolicy {
            max_attempts: 1,
            base: Duration::ZERO,
            cap: Duration::ZERO,
        },
    );
    let layout = StoreLayout::new(store.clone(), "commit-recovery".to_owned());
    let mut heads = Vec::new();
    let mut parents = BTreeMap::new();
    let mut edits = Vec::new();
    for name in ["refs/heads/dev", "refs/heads/main"] {
        heads.push(read_ref_head(&store, &layout, name).await.unwrap());
        parents.insert(name.to_owned(), None);
        edits.push(RefJournalEdit {
            ref_name: name.to_owned(),
            old_oid: None,
            new_oid: Some("a".repeat(40)),
            peeled_oid: None,
            lock_holder: None,
            visibility_evidence_hash: None,
        });
    }
    let transaction = RefJournalTransaction::new(parents, edits, None, vec![], vec![]).unwrap();
    (store, layout, origin, transaction, heads)
}

#[tokio::test]
async fn lost_marker_reply_is_confirmed_before_returning_committed() {
    let (store, layout, origin, transaction, heads) = fixture(Fault::LostReply).await;
    let committed = commit_ref_transaction(&store, &layout, &transaction, &heads)
        .await
        .unwrap();
    assert_eq!(committed.transaction_id, transaction.id().unwrap());
    let observed = list_ref_heads(&origin, &layout).await.unwrap();
    assert!(
        observed
            .iter()
            .all(|head| head.head.prepared_transaction.is_none()
                && head.head.committed_transaction.as_ref() == Some(&committed.transaction_id))
    );
}

#[tokio::test]
async fn unconfirmed_marker_preserves_identity_and_typed_failure() {
    for fault in [
        Fault::LostReplyAndRead,
        Fault::RejectedWrite,
        Fault::MismatchedMarker,
        Fault::OversizedMarker,
    ] {
        let (store, layout, origin, transaction, heads) = fixture(fault).await;
        let error = commit_ref_transaction(&store, &layout, &transaction, &heads)
            .await
            .unwrap_err();
        let MetadataError::RefJournalCommitUncertain {
            transaction_id,
            source,
            verification,
        } = error
        else {
            panic!("expected an uncertain outcome for {fault:?}");
        };
        assert_eq!(transaction_id, transaction.id().unwrap());
        assert!(std::error::Error::source(source.as_ref()).is_some());
        assert_eq!(
            verification.is_some(),
            !matches!(fault, Fault::RejectedWrite)
        );
        // No rollback follows an attempted marker write, even when readback fails.
        let observed = list_ref_heads(&origin, &layout).await.unwrap();
        assert!(
            observed
                .iter()
                .all(|head| head.head.prepared_transaction.as_ref() == Some(&transaction_id))
        );
    }
}

#[tokio::test]
async fn missing_marker_after_compaction_is_not_reported_as_rejection() {
    let (store, layout, origin, transaction, heads) = fixture(Fault::CompactedBeforeRead).await;
    let error = commit_ref_transaction(&store, &layout, &transaction, &heads)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        MetadataError::RefJournalCommitUncertain {
            verification: None,
            ..
        }
    ));
    let (manifest, _) = manifest_store::read_manifest(&origin, &layout)
        .await
        .unwrap();
    assert_eq!(
        manifest.refs,
        BTreeMap::from([
            ("refs/heads/dev".to_owned(), "a".repeat(40)),
            ("refs/heads/main".to_owned(), "a".repeat(40)),
        ])
    );
}
