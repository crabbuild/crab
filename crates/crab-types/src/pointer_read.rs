//! JSON-lines contract for an owned native pointer read session.

use serde::{Deserialize, Serialize};

/// Commands for one file, with at most one query in flight.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Open { pointer_b64: String },
    Range { offset: u64, length: u32 },
    Chunks { start: u64, limit: u32 },
    Cancel,
    Close,
}

/// Acknowledgements emitted after the corresponding native work finishes.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
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
