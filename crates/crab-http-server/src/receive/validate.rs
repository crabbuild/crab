use crab_git::{
    incoming_pack::ReceiveLimits,
    receive_plan::{GraphLimits, RefPolicy},
};

const MAX_GRAPH_STEPS: usize = 100_000_000;
const MAX_GRAPH_READ_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_INCOMING_OBJECTS: u32 = 5_000_000;
const MAX_OBJECT_BYTES: usize = 128 * 1024 * 1024;
const MAX_INFLATED_BYTES: u64 = 64 * 1024 * 1024 * 1024;

pub(super) use crab_remote::prepare::Prepared;

pub(super) async fn prepare(
    repository: crab_remote_git::RemoteGitRepository,
    layout: crab_storage::StoreLayout<crab_storage::Store>,
    directory: std::path::PathBuf,
    input: Option<std::io::BufReader<std::fs::File>>,
    updates: Vec<crab_git::receive_plan::RefUpdate>,
    visibility_bases: std::collections::BTreeMap<String, (String, gix_hash::ObjectId)>,
    max_ref_updates: usize,
    cancel: &tokio_util::sync::CancellationToken,
) -> super::Result<Prepared> {
    let default_branch = repository
        .refs()
        .head
        .as_ref()
        .map(|head| head.name.clone());
    crab_remote::prepare::prepare(
        repository,
        directory,
        input,
        updates,
        visibility_bases,
        cancel,
        crab_remote::prepare::Options {
            layout,
            graph: GraphLimits {
                max_ref_updates,
                max_graph_steps: MAX_GRAPH_STEPS,
                max_object_bytes: MAX_OBJECT_BYTES,
                // A full-history create visits each unique object. This is a
                // streaming work budget; object data is not retained together.
                max_read_bytes: MAX_GRAPH_READ_BYTES,
            },
            pack: ReceiveLimits {
                max_pack_bytes: super::MAX_BODY,
                max_objects: MAX_INCOMING_OBJECTS,
                max_object_bytes: MAX_OBJECT_BYTES,
                max_inflated_bytes: MAX_INFLATED_BYTES,
                max_delta_depth: 128,
            },
            policy: move |name: &str| RefPolicy {
                allow_delete: default_branch.as_deref() != Some(name),
                allow_non_fast_forward: false,
            },
        },
    )
    .await
    .map_err(map_error)
}

pub(super) fn map_error(error: crab_remote::prepare::Error) -> super::ReceiveError {
    use super::ReceiveError;
    use crab_remote::prepare::Error;
    match error {
        Error::Cancelled => ReceiveError::Cancelled,
        Error::Request(reason) => ReceiveError::Request(reason),
        Error::Content(_) => ReceiveError::Request("Prepared content rejected"),
        Error::ContentFormat(_) => ReceiveError::Request("Prepared content is corrupt"),
        Error::Pack(error) => ReceiveError::Pack(error),
        Error::Graph(error) => ReceiveError::Graph(error),
        Error::Prepare(error) => ReceiveError::Prepare(error),
        Error::Io(error) => ReceiveError::Io(error),
        Error::Dependency(error) => ReceiveError::Dependency(error),
        Error::Write(error) => ReceiveError::Write(error),
        Error::Storage(error) => ReceiveError::Storage(error),
        Error::Metadata(error) => ReceiveError::Metadata(error),
        Error::Worker(error) => ReceiveError::Worker(error),
        Error::Remote(error) => ReceiveError::Remote(error),
        Error::Close { operation, close } => ReceiveError::Close {
            operation: Box::new(map_error(*operation)),
            close,
        },
    }
}
