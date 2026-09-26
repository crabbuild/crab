//! Dispatch, compaction, and command/query/resolve semantics.

use super::*;

mod admission;
mod capacity;
mod dispatcher;
mod handlers;
mod resolve;
mod shutdown;
