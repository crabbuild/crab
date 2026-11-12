//! JSON-lines contract for an owned native pointer read session.

use serde::{Deserialize, Serialize};

/// Commands for one file, with at most one query in flight.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Open {
        pointer_b64: String,
        /// Apply the worktree's prefetch protection before opening this source.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dehydrate_path: Option<String>,
    },
    Range {
        offset: u64,
        length: u32,
    },
    Chunks {
        start: u64,
        limit: u32,
    },
    Cancel,
    Close,
}

/// Acknowledgements emitted after the corresponding native work finishes.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    /// Cumulative physical origin-body bytes; queries count range/chunk commands
    /// from one on this pipe. None attributes retained-reader background work.
    ReadProgress {
        query: Option<u64>,
        received_bytes: u64,
    },
    Ready {
        size: u64,
        remote_url: Option<String>,
    },
    Data {
        offset: u64,
        bytes_b64: String,
    },
    Chunks {
        page: crate::file_layout::ChunkPage,
    },
    Cancelled,
    Failed {
        code: String,
        message: String,
    },
    Closed,
}
