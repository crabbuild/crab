//! JSON-lines contract for an owned native pointer read session.

use serde::{Deserialize, Serialize};

/// Commands for one file, with at most one range in flight.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Open { pointer_b64: String },
    Range { offset: u64, length: u32 },
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
    Cancelled,
    Failed {
        code: String,
        message: String,
    },
    Closed,
}
