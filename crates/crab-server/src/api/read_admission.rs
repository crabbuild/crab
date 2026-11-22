use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use axum::{body::Body, response::Response};
use bytes::Bytes;
use http_body::{Frame, SizeHint};
use tokio::sync::{OwnedSemaphorePermit, TryAcquireError};

use super::Action;
use crate::server::Server;

pub(super) struct Admission {
    _total: OwnedSemaphorePermit,
    _bulk: Option<OwnedSemaphorePermit>,
}

pub(super) fn acquire(server: &Server, action: Action) -> Result<Admission, TryAcquireError> {
    // A caller cannot opt into metadata priority. Preview is capped at 1 MiB;
    // attribution, history and payload reads each consume bulk capacity even
    // when they join a shared producer.
    let bulk = !matches!(
        action,
        Action::Refs | Action::Tree | Action::Search | Action::FilePreview
    );
    reserve(server, bulk)
}

pub(super) fn acquire_bulk(server: &Server) -> Result<Admission, TryAcquireError> {
    reserve(server, true)
}

fn reserve(server: &Server, bulk: bool) -> Result<Admission, TryAcquireError> {
    let bulk = bulk
        .then(|| server.bulk_reads.clone().try_acquire_owned())
        .transpose()?;
    let total = server.admission.clone().try_acquire_owned()?;
    Ok(Admission {
        _total: total,
        _bulk: bulk,
    })
}

struct AdmittedBody {
    inner: Body,
    admission: Arc<Admission>,
}

struct AdmittedBytes {
    bytes: Bytes,
    _admission: Arc<Admission>,
}

impl AsRef<[u8]> for AdmittedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl http_body::Body for AdmittedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_frame(cx).map(|frame| {
            frame.map(|result| {
                result.map(|frame| {
                    frame.map_data(|bytes| {
                        Bytes::from_owner(AdmittedBytes {
                            bytes,
                            _admission: this.admission.clone(),
                        })
                    })
                })
            })
        })
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

pub(super) fn retain(response: Response, admission: Admission) -> Response {
    let (parts, inner) = response.into_parts();
    Response::from_parts(
        parts,
        Body::new(AdmittedBody {
            inner,
            admission: Arc::new(admission),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    fn server() -> Server {
        Server::for_test(
            std::collections::BTreeMap::<(String, String), crate::repository::Repository>::new()
                .into(),
            crate::transfer_admission::TransferAdmission::new(
                crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new())),
                "admission-test".into(),
                1,
            ),
        )
    }

    #[tokio::test]
    async fn bulk_readers_leave_four_metadata_slots_and_transport_owns_permits() {
        let server = server();
        let bulk = (0..12)
            .map(|_| acquire(&server, Action::Blame).unwrap())
            .collect::<Vec<_>>();
        assert!(acquire(&server, Action::Blame).is_err());
        let metadata = (0..4)
            .map(|_| acquire(&server, Action::FilePreview).unwrap())
            .collect::<Vec<_>>();
        assert!(acquire(&server, Action::Refs).is_err());
        drop(metadata);
        drop(bulk);
        let response = retain(
            Response::new(Body::from("payload")),
            acquire(&server, Action::Blob).unwrap(),
        );
        let mut body = response.into_body();
        let bytes = body.frame().await.unwrap().unwrap().into_data().unwrap();
        let slice = bytes.slice(1..2);
        drop(body);
        drop(bytes);
        assert_eq!(
            (
                server.admission.available_permits(),
                server.bulk_reads.available_permits()
            ),
            (15, 11)
        );
        drop(slice);
        assert_eq!(
            (
                server.admission.available_permits(),
                server.bulk_reads.available_permits()
            ),
            (16, 12)
        );
    }
}
