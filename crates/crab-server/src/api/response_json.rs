use std::{io::Write, sync::Arc};

use bytes::Bytes;
use crab_remote_git::{Error, RemoteGitRuntime, SharedBlame};
use serde::{
    Serialize,
    ser::{SerializeSeq, SerializeStruct},
};
use tokio_util::sync::CancellationToken;

use super::representation::commit_json;

const MAX_BYTES: usize = 8 * 1024 * 1024;

struct BoundedWriter {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_BYTES - self.bytes.len() {
            self.exceeded = true;
            return Err(std::io::Error::other("HTTP response byte limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) async fn encode<T: Serialize + Send + 'static>(
    runtime: &Arc<RemoteGitRuntime>,
    cancellation: &CancellationToken,
    value: T,
    temporary_bytes: usize,
) -> crab_remote_git::Result<Bytes> {
    let memory = runtime
        .reserve_bytes(MAX_BYTES.saturating_add(temporary_bytes))
        .await?;
    runtime
        .run_cpu(cancellation, move || {
            let mut writer = BoundedWriter {
                bytes: Vec::new(),
                exceeded: false,
            };
            writer
                .bytes
                .try_reserve_exact(MAX_BYTES)
                .map_err(|source| Error::Allocation {
                    requested: MAX_BYTES,
                    source,
                })?;
            if let Err(source) = serde_json::to_writer(&mut writer, &value) {
                if writer.exceeded {
                    return Err(Error::LimitExceeded {
                        limit: "HTTP response bytes",
                        actual: MAX_BYTES as u64 + 1,
                        maximum: MAX_BYTES as u64,
                    });
                }
                return Err(Error::Consumer {
                    source: Box::new(source),
                });
            }
            memory.bind(Bytes::from(writer.bytes))
        })
        .await
}

struct BlameReply {
    blame: SharedBlame,
    generation: u64,
}

impl Serialize for BlameReply {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        struct Ranges<'a>(&'a SharedBlame);
        impl Serialize for Ranges<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut sequence = serializer.serialize_seq(Some(self.0.ranges.len()))?;
                for range in &self.0.ranges {
                    // Present one range at a time. Repeated origins must not
                    // multiply an entire intermediate JSON tree in memory.
                    sequence.serialize_element(&serde_json::json!({
                        "start": range.start, "lines": range.lines, "commit": commit_json(&range.commit),
                    }))?;
                }
                sequence.end()
            }
        }
        let mut value = serializer.serialize_struct("Blame", 3)?;
        value.serialize_field("ranges", &Ranges(&self.blame))?;
        value.serialize_field("commit", &self.blame.commit.to_string())?;
        value.serialize_field("generation", &self.generation)?;
        value.end()
    }
}

pub(super) async fn blame(
    runtime: &Arc<RemoteGitRuntime>,
    cancellation: &CancellationToken,
    blame: SharedBlame,
    generation: u64,
) -> crab_remote_git::Result<Bytes> {
    // Lossy UTF-8, hex, parent IDs and serde Value nodes exist for at most one
    // range. Admit their conservative capacity before constructing those nodes.
    let temporary = blame
        .ranges
        .iter()
        .map(|range| {
            let commit = &range.commit;
            commit
                .author
                .name
                .len()
                .saturating_add(commit.message.len())
                .saturating_add(commit.parents.len().saturating_mul(64))
                .saturating_add(1024)
                .saturating_mul(12)
        })
        .max()
        .unwrap_or(0);
    encode(
        runtime,
        cancellation,
        BlameReply { blame, generation },
        temporary,
    )
    .await
}
